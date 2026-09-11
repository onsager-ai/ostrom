use std::{
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::Duration,
};

/// Configure `command` so its child leads a new process group.
///
/// Group leadership lets every termination path stop the harness and any
/// subprocesses it has started with one grace-aware operation.
#[cfg(unix)]
pub fn set_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    command.process_group(0);
}

/// Leave process-group configuration unchanged on platforms without Unix
/// process groups.
#[cfg(not(unix))]
pub fn set_process_group(_command: &mut Command) {}

#[cfg(unix)]
pub(crate) fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    path.metadata()
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
pub(crate) fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

/// Terminate a child-led process group with `SIGTERM`, escalating to `SIGKILL`
/// when it remains alive after `grace`.
///
/// The child must have been configured with [`set_process_group`] before it
/// was spawned. The returned string names the last signal sent.
pub fn terminate_child_process_group(child: &mut Child, grace: Duration) -> Option<String> {
    let pid = child.id();
    let group = format!("-{pid}");
    let _ = Command::new(kill_command())
        .args(["-TERM", "--", &group])
        .status();
    let deadline = std::time::Instant::now() + grace;
    while std::time::Instant::now() < deadline {
        let _ = child.try_wait();
        if !process_group_alive(pid) {
            return Some("SIGTERM".to_owned());
        }
        thread::sleep(Duration::from_millis(50));
    }
    let _ = Command::new(kill_command())
        .args(["-KILL", "--", &group])
        .status();
    Some("SIGKILL".to_owned())
}

pub(crate) fn kill_remaining_process_group(pid: u32) {
    if process_group_alive(pid) {
        let group = format!("-{pid}");
        let _ = Command::new(kill_command())
            .args(["-KILL", "--", &group])
            .status();
    }
}

fn process_group_alive(pid: u32) -> bool {
    Command::new(kill_command())
        .args(["-0", "--", &format!("-{pid}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

pub(crate) fn process_alive(pid: u32) -> bool {
    Command::new(kill_command())
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

fn kill_command() -> &'static str {
    if Path::new("/bin/kill").is_file() {
        "/bin/kill"
    } else {
        "kill"
    }
}

/// Pins `terminate_child_process_group`'s three externally-observable
/// outcomes: a cooperative child reports `SIGTERM`, a stubborn one is
/// escalated to `SIGKILL`, and — the acceptance row this exists to prove
/// (onsager-ai/umwelt#5) — a grandchild that outlives its parent and ignores
/// `SIGTERM` is still reached, because the signal targets the process
/// *group*, not the direct child's pid.
///
/// Every helper here is spawned with a bounded lifetime of its own (a capped
/// sleep loop, never an unbounded one) and is torn down by a `Drop` guard
/// that sends `SIGKILL` to its group regardless of how the test exits, so a
/// panicking assertion cannot leak a process on a shared machine.
#[cfg(all(test, unix))]
mod tests {
    use std::{
        path::{Path, PathBuf},
        process::Command,
    };

    use super::*;

    /// Sends `SIGKILL` to a spawned group on drop. A test holds one for the
    /// lifetime of every helper process it starts, so an assertion panic
    /// midway through the test still reaps whatever was left running.
    struct KillGroupOnDrop(u32);

    impl Drop for KillGroupOnDrop {
        fn drop(&mut self) {
            kill_remaining_process_group(self.0);
        }
    }

    /// Configure and spawn `command` as the leader of its own process group.
    fn spawn_group(command: &mut Command) -> Child {
        set_process_group(command);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command.spawn().expect("spawn helper process")
    }

    /// Reserve a path guaranteed not to exist yet. A helper script creates it
    /// once it reaches a specific point, letting the test wait for that
    /// point deterministically instead of guessing with a sleep.
    fn reserve_marker_path() -> PathBuf {
        let marker = tempfile::Builder::new()
            .prefix("umwelt-process-control-test-")
            .tempfile()
            .expect("reserve a unique marker path");
        let path = marker.path().to_path_buf();
        drop(marker); // deletes the file; the now-unique path stays reserved
        path
    }

    /// Block, up to a generous ceiling, until `marker` exists, then remove
    /// it. Used to know a spawned shell has reached a specific line of its
    /// script — installing a trap, forking a grandchild — before the test
    /// acts on it, rather than racing a fixed sleep against process startup
    /// under unpredictable machine load.
    fn wait_for_marker(marker: &Path, ceiling: Duration) {
        let deadline = std::time::Instant::now() + ceiling;
        while !marker.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "helper process never reached its marker at {}",
                marker.display()
            );
            thread::sleep(Duration::from_millis(10));
        }
        let _ = std::fs::remove_file(marker);
    }

    /// A shell one-liner that installs a `TERM` trap, signals readiness by
    /// creating `marker`, and then loops over a bounded number of
    /// one-second sleeps rather than blocking forever, so a leaked instance
    /// still exits on its own well within a test run.
    fn ignores_term_script(marker: &Path) -> String {
        format!(
            "trap '' TERM; : > '{marker}'; i=0; while [ \"$i\" -lt 60 ]; do sleep 1; i=$((i + 1)); done",
            marker = marker.display()
        )
    }

    /// Poll for the group to disappear, up to a generous ceiling.
    ///
    /// `SIGKILL` is fire-and-forget: the kernel guarantees the target dies,
    /// but not synchronously with the syscall that sent it, and an orphaned
    /// grandchild is reaped by init on its own schedule, not by anything
    /// this test owns. This waits for that eventual outcome rather than
    /// asserting it is already true the instant the call returns — the
    /// assertion is still on the outcome (dead or not), never on how long
    /// reaching it took, so a correct-but-slow reap still passes.
    fn wait_until_group_dead(pid: u32, ceiling: Duration) -> bool {
        let deadline = std::time::Instant::now() + ceiling;
        loop {
            if !process_group_alive(pid) {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn terminate_reports_sigterm_when_the_group_exits_promptly() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 30"]);
        let mut child = spawn_group(&mut command);
        let pid = child.id();
        let _guard = KillGroupOnDrop(pid);

        // Generous on purpose: the call returns as soon as the group dies,
        // so a wide grace costs this test nothing and cannot make it flaky.
        let signal = terminate_child_process_group(&mut child, Duration::from_secs(2));
        let _ = child.wait();

        assert_eq!(signal.as_deref(), Some("SIGTERM"));
        assert!(
            wait_until_group_dead(pid, Duration::from_secs(2)),
            "process group {pid} should be gone after a cooperative SIGTERM"
        );
    }

    #[test]
    fn terminate_escalates_to_sigkill_when_the_child_ignores_sigterm() {
        let marker = reserve_marker_path();
        let mut command = Command::new("sh");
        command.args(["-c", &ignores_term_script(&marker)]);
        let mut child = spawn_group(&mut command);
        let pid = child.id();
        let _guard = KillGroupOnDrop(pid);

        // Wait for the trap to actually be installed. A SIGTERM that beats
        // it uses the shell's default (terminating) disposition, and the
        // test would not be exercising the ignore-and-escalate path at all.
        wait_for_marker(&marker, Duration::from_secs(2));

        let signal = terminate_child_process_group(&mut child, Duration::from_millis(300));
        // Every real call site reaps the leader immediately after calling
        // this function (agent.rs, control.rs, watchdog.rs all do). Reaping
        // is what clears a just-killed leader's zombie entry, which is what
        // `kill -0` on the group would otherwise still see as "alive".
        let _ = child.wait();

        assert_eq!(signal.as_deref(), Some("SIGKILL"));
        assert!(
            wait_until_group_dead(pid, Duration::from_secs(2)),
            "process group {pid} should be gone after escalation to SIGKILL"
        );
    }

    #[test]
    fn terminate_reaches_a_grandchild_that_ignores_sigterm_after_its_parent_exits() {
        // The direct child backgrounds a grandchild that ignores SIGTERM and
        // then exits immediately itself. The grandchild inherits the same
        // process group and is the only thing left alive in it — proving
        // that group-directed termination, not a pid-directed one, is what
        // reaches it.
        let marker = reserve_marker_path();
        let script = format!("( {} ) & exit 0", ignores_term_script(&marker));
        let mut command = Command::new("sh");
        command.args(["-c", &script]);
        let mut child = spawn_group(&mut command);
        let pid = child.id();
        let _guard = KillGroupOnDrop(pid);

        // Wait for the grandchild to install its trap before signaling.
        // This is setup synchronization on an outcome (the marker exists),
        // not a correctness assertion on elapsed time.
        wait_for_marker(&marker, Duration::from_secs(2));

        let _ = terminate_child_process_group(&mut child, Duration::from_millis(300));
        // The direct child (the group leader) has already exited on its own
        // by this point; reap it so a lingering zombie leader cannot make
        // the group look "alive" independently of the grandchild's fate.
        let _ = child.wait();

        assert!(
            wait_until_group_dead(pid, Duration::from_secs(2)),
            "grandchild in process group {pid} should not outlive its parent"
        );
    }
}
