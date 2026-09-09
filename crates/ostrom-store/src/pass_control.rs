//! Inherited control input. Only the pass loop applies requests or writes events.

use std::{
    fs::OpenOptions,
    io::{BufRead, BufReader},
    sync::mpsc::{self, Receiver, SyncSender},
    thread,
};

use ethogram::{
    AGENT_WARNING, AgentWarningPayload, CAPTURE_REFUSED, CONTROL_APPLIED, CONTROL_REQUESTED,
    CaptureRefusalCause, CaptureRefusedPayload, ControlAppliedPayload, ControlAppliedReason,
    ControlRequestedPayload, EventDraft, MAX_EXCERPT_SCALARS, PayloadExtension, excerpt,
};

use crate::run_events::open_fd;

/// One item read off the inherited control descriptor. `Ended` is distinct
/// from `Refused` so the reader can say "I stopped" without that being
/// mistaken for "a line arrived and it was malformed" (see `refuse_input`
/// vs. `ended` below): the two are different facts about the run.
pub(crate) enum ControlInput {
    Request(ControlRequestedPayload),
    Refused(String),
    Ended(String),
}

pub(crate) fn read_control(fd: u32) -> Receiver<ControlInput> {
    // A rendezvous bounds read-ahead to one line. Dropping the receiver at the
    // terminal makes the next send fail, including a read that was blocked then.
    // Do not join this thread: an idle supervisor may keep its write end open.
    let (sender, receiver) = mpsc::sync_channel(0);
    thread::spawn(move || match open_fd(fd, OpenOptions::new().read(true)) {
        Ok(file) => read_lines(BufReader::new(file), &sender),
        Err(error) => {
            let _ = sender.send(ControlInput::Ended(format!(
                "could not open control fd {fd}: {error}"
            )));
        }
    });
    receiver
}

fn read_lines(mut reader: impl BufRead, sender: &SyncSender<ControlInput>) {
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            // Ok(0) used to `break` here with nothing sent: the one path that
            // produced ostrom#528's silent EOF. A closed write end is routine
            // (module doc, above) but "the reader stopped" must still reach
            // the run's events; only whether the send lands is out of our
            // hands, per that same rendezvous discipline.
            Ok(0) => {
                let _ = sender.send(ControlInput::Ended(
                    "reached end of input on control descriptor".to_owned(),
                ));
                break;
            }
            Ok(_) => {
                let input = match parse_request(&line) {
                    Ok(request) => ControlInput::Request(request),
                    Err(detail) => ControlInput::Refused(detail),
                };
                if sender.send(input).is_err() {
                    break;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => {
                let _ = sender.send(ControlInput::Ended(format!(
                    "could not read control descriptor: {error}"
                )));
                break;
            }
        }
    }
}

fn parse_request(line: &[u8]) -> Result<ControlRequestedPayload, String> {
    let draft: EventDraft = serde_json::from_slice(line).map_err(|error| error.to_string())?;
    ethogram::validate(&draft.event_type, &draft.payload).map_err(|error| error.to_string())?;
    if draft.event_type != CONTROL_REQUESTED {
        return Err("control descriptor accepts only control.requested drafts".to_owned());
    }
    serde_json::from_value(draft.payload).map_err(|error| error.to_string())
}

/// A non-terminal warning that the control reader has stopped, and why. Only
/// the pass loop appends it (module doc, above); this just drafts it.
pub(crate) fn ended(cause: &str) -> EventDraft {
    let message = excerpt(cause, MAX_EXCERPT_SCALARS);
    EventDraft {
        event_type: AGENT_WARNING.to_owned(),
        payload: serde_json::to_value(AgentWarningPayload {
            stage: Some("control-descriptor".to_owned()),
            message: message.text,
            extra: PayloadExtension::new(),
        })
        .expect("agent.warning payload serialises"),
        captured_at: None,
    }
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

    use ethogram::ControlKind;
    use serde_json::json;

    use super::*;

    #[test]
    fn a_reader_with_no_receiver_drops_one_line_and_stops() {
        let (sender, receiver) = mpsc::sync_channel(0);
        drop(receiver);
        let mut input = Cursor::new(b"first\nsecond\n");
        read_lines(&mut input, &sender);
        assert_eq!(input.position(), 6);
    }

    /// Principle 6 boundary guard ("one definition, or a test that they
    /// agree"): the reader and ethogram both define what a control kind is,
    /// so this asserts they agree on the closed set. It is not the fix for
    /// #528's silent-EOF incident above — that is `ended` and the pass
    /// loop's handling of `ControlInput::Ended`, exercised elsewhere.
    #[test]
    fn parse_request_accepts_every_ethogram_control_kind_and_refuses_an_unfamiliar_one() {
        for kind in [
            ControlKind::Interrupt,
            ControlKind::Steer,
            ControlKind::Answer,
        ] {
            let mut payload = json!({
                "controlId": "control-1",
                "kind": serde_json::to_value(&kind).expect("control kind serialises"),
                "by": "operator",
            });
            // decisionId/optionId are required for "answer" and absent for
            // every other kind, at validation (ethogram, ControlRequestedPayload).
            match kind {
                ControlKind::Steer => payload["text"] = json!("take point on the next turn"),
                ControlKind::Answer => {
                    payload["decisionId"] = json!("decision-1");
                    payload["optionId"] = json!("allow");
                }
                ControlKind::Interrupt | ControlKind::Unknown(_) => {}
            }
            let line = format!(
                "{}\n",
                json!({"type": CONTROL_REQUESTED, "payload": payload})
            );
            let result = parse_request(line.as_bytes());
            assert!(result.is_ok(), "{payload:?} was refused: {result:?}");
        }

        let unfamiliar = json!({
            "type": CONTROL_REQUESTED,
            "payload": {"controlId": "control-1", "kind": "teleport", "by": "operator"},
        });
        assert!(
            parse_request(format!("{unfamiliar}\n").as_bytes()).is_err(),
            "an unfamiliar control kind was accepted"
        );
    }
}
