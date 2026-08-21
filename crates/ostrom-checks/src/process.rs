use std::{
    io::Read,
    process::{Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

pub(crate) enum ProcessResult {
    Completed {
        status: ExitStatus,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },
    SpawnFailed,
    WaitFailed,
    TimedOut,
}

pub(crate) fn run_bounded(command: &mut Command, timeout: Duration) -> ProcessResult {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    set_process_group(command);
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
fn set_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.process_group(0);
}

#[cfg(not(unix))]
fn set_process_group(_command: &mut Command) {}

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

pub(crate) fn parameter_timeout(
    value: Option<&serde_json::Value>,
) -> Result<Duration, crate::ActionFault> {
    let milliseconds = match value {
        None => 30_000,
        Some(serde_json::Value::Number(number)) => number
            .as_u64()
            .and_then(|seconds| seconds.checked_mul(1_000))
            .ok_or_else(invalid_parameters)?,
        Some(serde_json::Value::String(value)) => parse_duration(value)?,
        Some(_) => return Err(invalid_parameters()),
    };
    if milliseconds == 0 {
        return Err(invalid_parameters());
    }
    Ok(Duration::from_millis(milliseconds))
}

fn parse_duration(value: &str) -> Result<u64, crate::ActionFault> {
    let split = value
        .find(|character: char| !character.is_ascii_digit())
        .ok_or_else(invalid_parameters)?;
    let (amount, unit) = value.split_at(split);
    let amount = amount.parse::<u64>().map_err(|_| invalid_parameters())?;
    let multiplier = match unit {
        "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        _ => return Err(invalid_parameters()),
    };
    amount
        .checked_mul(multiplier)
        .ok_or_else(invalid_parameters)
}

pub(crate) fn invalid_parameters() -> crate::ActionFault {
    crate::ActionFault::new("invalid_action_parameters", None)
}

pub(crate) fn exact_keys(
    parameters: &std::collections::BTreeMap<String, serde_json::Value>,
    allowed: &[&str],
) -> bool {
    parameters
        .keys()
        .all(|key| key == "fresh_for" || allowed.contains(&key.as_str()))
}
