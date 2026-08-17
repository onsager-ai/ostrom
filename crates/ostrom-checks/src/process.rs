use std::{
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

pub(crate) enum ProcessResult {
    Completed(ExitStatus, Option<String>),
    SpawnFailed,
    WaitFailed,
    TimedOut,
    OutputMalformed,
}

pub(crate) fn run_bounded(
    command: &mut Command,
    timeout: Duration,
    capture_stdout: bool,
) -> ProcessResult {
    command.stdin(Stdio::null()).stderr(Stdio::null());
    if capture_stdout {
        command.stdout(Stdio::piped());
    } else {
        command.stdout(Stdio::null());
    }
    let Ok(mut child) = command.spawn() else {
        return ProcessResult::SpawnFailed;
    };
    let Some(deadline) = Instant::now().checked_add(timeout) else {
        let _ = child.kill();
        let _ = child.wait();
        return ProcessResult::WaitFailed;
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return completed(child, status, capture_stdout),
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(5));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return ProcessResult::TimedOut;
            }
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return ProcessResult::WaitFailed;
            }
        }
    }
}

fn completed(mut child: Child, status: ExitStatus, capture_stdout: bool) -> ProcessResult {
    if !capture_stdout {
        return ProcessResult::Completed(status, None);
    }
    let Some(mut stdout) = child.stdout.take() else {
        return ProcessResult::OutputMalformed;
    };
    let mut bytes = Vec::new();
    if std::io::Read::read_to_end(&mut stdout, &mut bytes).is_err() {
        return ProcessResult::OutputMalformed;
    }
    match String::from_utf8(bytes) {
        Ok(output) => ProcessResult::Completed(status, Some(output)),
        Err(_) => ProcessResult::OutputMalformed,
    }
}

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
