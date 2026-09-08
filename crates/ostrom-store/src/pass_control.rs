//! Inherited control input. Only the pass loop applies requests or writes events.

use std::{
    fs::OpenOptions,
    io::{BufRead, BufReader},
    sync::mpsc::{self, Receiver, SyncSender},
    thread,
};

use ethogram::{
    CAPTURE_REFUSED, CONTROL_APPLIED, CONTROL_REQUESTED, CaptureRefusalCause,
    CaptureRefusedPayload, ControlAppliedPayload, ControlAppliedReason, ControlRequestedPayload,
    EventDraft, MAX_EXCERPT_SCALARS, PayloadExtension, excerpt,
};

use crate::run_events::open_fd;

pub(crate) type ControlInput = Result<ControlRequestedPayload, String>;

pub(crate) fn read_control(fd: u32) -> Receiver<ControlInput> {
    // A rendezvous bounds read-ahead to one line. Dropping the receiver at the
    // terminal makes the next send fail, including a read that was blocked then.
    // Do not join this thread: an idle supervisor may keep its write end open.
    let (sender, receiver) = mpsc::sync_channel(0);
    thread::spawn(move || match open_fd(fd, OpenOptions::new().read(true)) {
        Ok(file) => read_lines(BufReader::new(file), &sender),
        Err(error) => {
            let _ = sender.send(Err(format!("could not open control fd {fd}: {error}")));
        }
    });
    receiver
}

fn read_lines(mut reader: impl BufRead, sender: &SyncSender<ControlInput>) {
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Ok(_) => {
                if sender.send(parse_request(&line)).is_err() {
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                let _ = sender.send(Err(format!("could not read control descriptor: {error}")));
                break;
            }
        }
    }
}

fn parse_request(line: &[u8]) -> ControlInput {
    let draft: EventDraft = serde_json::from_slice(line).map_err(|error| error.to_string())?;
    ethogram::validate(&draft.event_type, &draft.payload).map_err(|error| error.to_string())?;
    if draft.event_type != CONTROL_REQUESTED {
        return Err("control descriptor accepts only control.requested drafts".to_owned());
    }
    serde_json::from_value(draft.payload).map_err(|error| error.to_string())
}

pub(crate) fn refuse_input(run_id: &str, detail: &str) -> EventDraft {
    let detail = excerpt(detail, MAX_EXCERPT_SCALARS);
    EventDraft {
        event_type: CAPTURE_REFUSED.to_owned(),
        payload: serde_json::to_value(CaptureRefusedPayload {
            cause: CaptureRefusalCause::Malformed,
            source_run_id: run_id.to_owned(),
            source_seq: None,
            source_type: Some(CONTROL_REQUESTED.to_owned()),
            field: None,
            count: None,
            max: None,
            detail: Some(detail.text),
            truncated: Some(detail.truncated),
            extra: PayloadExtension::new(),
        })
        .expect("capture.refused payload serialises"),
        captured_at: None,
    }
}

pub(crate) fn unsupported(request: &ControlRequestedPayload) -> [EventDraft; 2] {
    [
        EventDraft {
            event_type: CONTROL_REQUESTED.to_owned(),
            payload: serde_json::to_value(request).expect("control.requested payload serialises"),
            captured_at: None,
        },
        EventDraft {
            event_type: CONTROL_APPLIED.to_owned(),
            payload: serde_json::to_value(ControlAppliedPayload {
                control_id: request.control_id.clone(),
                ok: false,
                reason: Some(ControlAppliedReason::Unsupported),
                truncated: None,
                landed_in: None,
                extra: PayloadExtension::from_iter([("by".to_owned(), request.by.clone().into())]),
            })
            .expect("control.applied payload serialises"),
            captured_at: None,
        },
    ]
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn a_reader_with_no_receiver_drops_one_line_and_stops() {
        let (sender, receiver) = mpsc::sync_channel(0);
        drop(receiver);
        let mut input = Cursor::new(b"first\nsecond\n");
        read_lines(&mut input, &sender);
        assert_eq!(input.position(), 6);
    }
}
