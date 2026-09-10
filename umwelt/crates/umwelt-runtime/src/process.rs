use std::{
    io::Read,
    process::{Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

pub enum ProcessResult {
    Completed {
        status: ExitStatus,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    SpawnFailed,
    WaitFailed,
    TimedOut,
}

pub fn run_bounded(command: &mut Command, timeout: Duration) -> ProcessResult {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::process_control::set_process_group(command);
    let Ok(mut child) = command.spawn() else {
        return ProcessResult::SpawnFailed;
    };
    let capture = |mut stream: Box<dyn Read + Send>| {
        thread::spawn(move || {
            const DIAGNOSTIC_LIMIT: usize = 8 * 1024;
            let mut diagnostic = Vec::new();
            let mut buffer = [0_u8; 1024];
            while let Ok(read) = stream.read(&mut buffer) {
                if read == 0 {
                    break;
                }
                let remaining = DIAGNOSTIC_LIMIT.saturating_sub(diagnostic.len());
                diagnostic.extend_from_slice(&buffer[..read.min(remaining)]);
            }
            diagnostic
        })
    };
    let stdout = child.stdout.take().map(|stdout| capture(Box::new(stdout)));
    let stderr = child.stderr.take().map(|stderr| capture(Box::new(stderr)));
    let pid = child.id();
    let Some(deadline) = Instant::now().checked_add(timeout) else {
        terminate(&mut child, pid);
        return ProcessResult::WaitFailed;
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                kill_process_group(pid);
                let stdout = stdout
                    .and_then(|reader| reader.join().ok())
                    .unwrap_or_default();
                let stderr = stderr
                    .and_then(|reader| reader.join().ok())
                    .unwrap_or_default();
                return ProcessResult::Completed {
                    status,
                    stdout,
                    stderr,
                };
            }
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(5));
            }
            Ok(None) => {
                terminate(&mut child, pid);
                return ProcessResult::TimedOut;
            }
            Err(_) => {
                terminate(&mut child, pid);
                return ProcessResult::WaitFailed;
            }
        }
    }
}

fn terminate(child: &mut std::process::Child, pid: u32) {
    kill_process_group(pid);
    let _ = child.kill();
    let _ = child.wait();
}

#[cfg(unix)]
fn kill_process_group(pid: u32) {
    let executable = if std::path::Path::new("/bin/kill").is_file() {
        "/bin/kill"
    } else {
        "kill"
    };
    let _ = Command::new(executable)
        .args(["-KILL", "--", &format!("-{pid}")])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(unix))]
fn kill_process_group(_pid: u32) {}

pub fn invalid_parameters() -> crate::ActionFault {
    crate::ActionFault::new("invalid_action_parameters", None)
}
