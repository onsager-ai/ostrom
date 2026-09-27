//! Time the machine spent suspended (#637).
//!
//! A run's own watchdog measures its caps with `Instant`, which on Linux is
//! `CLOCK_MONOTONIC` and does not advance while the machine is suspended. The
//! stall reaper and lease expiry measure with the wall clock, which does. So
//! after a laptop resumes, every live run would read as silent for as long as
//! the machine slept, and the first `ostrom up` would reap them all at their
//! full cost ceilings. What those two judge is therefore measured net of
//! suspended time.
//!
//! **The reading.** The kernel's suspended total since boot is
//! `CLOCK_BOOTTIME` minus `CLOCK_MONOTONIC`, both read with `clock_gettime`
//! through `rustix`, once per reading. Only a Linux kernel has
//! `CLOCK_BOOTTIME`; anywhere else the reading fails, and a failed reading
//! never reaps.
//!
//! **The timeline.** The total says how long the machine has slept since boot,
//! not when. Each reaper run records its reading, with the wall-clock second it
//! was taken at, in `<state>/reaping/suspend-timeline.json`, under the boot it
//! belongs to. The time suspended since a wall-clock second `t` is then bounded
//! from above by the samples on either side of `t`: the total was at least the
//! last sample at or before `t`, and at least the next sample after `t` minus
//! the wall time between `t` and it, since the machine cannot sleep for longer
//! than the wall clock moves. A missing or dropped sample only widens that
//! bound: it counts more time as suspended, never less, so it can delay a reap
//! but never cause one.

use std::{
    fs,
    io::Write as _,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

use crate::Clock;

/// How many samples the timeline keeps. Runs of equal totals are collapsed to
/// their first and last sample, so this is room for many suspends; past it the
/// oldest go first, which only makes old gaps count more time as suspended.
const MAX_SAMPLES: usize = 64;

/// A difference of at most this many seconds between two totals is reading
/// noise, not a suspend.
const NOISE_SECONDS: u64 = 1;

/// One reading of the suspended total.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SuspendReading {
    /// The kernel's boot id: a total is only comparable within one boot.
    pub boot_id: String,
    /// `CLOCK_BOOTTIME` minus `CLOCK_MONOTONIC`, in whole seconds.
    pub suspended_seconds: u64,
}

/// Where a reading comes from: the kernel in production, a fixture in tests.
pub(crate) trait SuspendSource {
    /// The current reading, or why it cannot be taken.
    fn read(&self) -> Result<SuspendReading, String>;
}

/// The kernel's clocks.
pub(crate) struct SystemSuspend;

impl SuspendSource for SystemSuspend {
    fn read(&self) -> Result<SuspendReading, String> {
        let (boottime, monotonic) = clocks()?;
        // Read in that order, the boot clock cannot trail the monotonic one;
        // if it does, these are not the clocks this reads.
        if monotonic > boottime {
            return Err(format!(
                "CLOCK_MONOTONIC ({}s) is ahead of CLOCK_BOOTTIME ({}s)",
                monotonic.as_secs(),
                boottime.as_secs()
            ));
        }
        let boot_id_path = Path::new("/proc/sys/kernel/random/boot_id");
        let boot_id = fs::read_to_string(boot_id_path)
            .map_err(|error| format!("could not read {}: {error}", boot_id_path.display()))?
            .trim()
            .to_owned();
        if boot_id.is_empty() {
            return Err(format!("{} is empty", boot_id_path.display()));
        }
        Ok(SuspendReading {
            boot_id,
            suspended_seconds: boottime.saturating_sub(monotonic).as_secs(),
        })
    }
}

/// `(CLOCK_BOOTTIME, CLOCK_MONOTONIC)`, the monotonic clock read first.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn clocks() -> Result<(Duration, Duration), String> {
    use rustix::time::{ClockId, clock_gettime};

    let read = |id: ClockId, name: &str| {
        Duration::try_from(clock_gettime(id))
            .map_err(|error| format!("{name} is not a duration: {error}"))
    };
    let monotonic = read(ClockId::Monotonic, "CLOCK_MONOTONIC")?;
    // MUTATION C: the wrong clock stands in for CLOCK_BOOTTIME.
    let boottime = read(ClockId::Realtime, "CLOCK_BOOTTIME")?;
    Ok((boottime, monotonic))
}

/// No `CLOCK_BOOTTIME` here: suspended time cannot be read, and nothing is
/// judged by it.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn clocks() -> Result<(Duration, Duration), String> {
    Err(format!(
        "this platform ({}) has no CLOCK_BOOTTIME",
        std::env::consts::OS
    ))
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct TimelineFile {
    boot_id: String,
    /// `[wall-clock second, suspended total]`, ascending.
    samples: Vec<(u64, u64)>,
}

/// The suspended total now, and the samples earlier reaper runs recorded in
/// this boot.
#[derive(Debug, Clone)]
pub(crate) struct SuspendTimeline {
    boot_id: String,
    now: u64,
    now_suspended: u64,
    samples: Vec<(u64, u64)>,
}

impl SuspendTimeline {
    /// Take one reading and load the recorded samples. `Err` says why the
    /// suspended time cannot be determined; nothing may then be judged by it.
    pub(crate) fn read(
        state: &Path,
        clock: &Clock,
        source: &dyn SuspendSource,
    ) -> Result<Self, String> {
        let reading = source.read()?;
        let now = clock.epoch_seconds();
        // A timeline that is missing, unreadable or from another boot is no
        // samples, which only counts more time as suspended.
        let mut samples = fs::read(timeline_path(state))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<TimelineFile>(&bytes).ok())
            .filter(|file| file.boot_id == reading.boot_id)
            .map(|file| file.samples)
            .unwrap_or_default();
        // A sample from the future (the wall clock was set back) or above the
        // total now cannot be placed; it is dropped, not trusted.
        samples.retain(|&(wall, suspended)| {
            wall < now && suspended <= reading.suspended_seconds.saturating_add(NOISE_SECONDS)
        });
        samples.sort_unstable();
        let now_suspended = match samples.last() {
            Some(&(_, last)) if last.abs_diff(reading.suspended_seconds) <= NOISE_SECONDS => last,
            _ => reading.suspended_seconds,
        };
        Ok(Self {
            boot_id: reading.boot_id,
            now,
            now_suspended,
            samples,
        })
    }

    /// An upper bound on the seconds the machine was suspended between the
    /// wall-clock second `since` and now.
    pub(crate) fn suspended_since(&self, since: u64) -> u64 {
        if since >= self.now {
            return 0;
        }
        // The total at `since` is at least the last sample at or before it
        // (none: nothing is known, so zero), and at least the next sample
        // after it less the wall time in between.
        let mut before = 0;
        let mut after = (self.now, self.now_suspended);
        for &(wall, suspended) in &self.samples {
            if wall <= since {
                before = suspended;
            } else {
                after = (wall, suspended);
                break;
            }
        }
        let (after_wall, after_suspended) = after;
        let at_since = before.max(after_suspended.saturating_sub(after_wall - since));
        self.now_suspended.saturating_sub(at_since)
    }

    /// The wall-clock seconds from `since` to now, net of suspended time.
    pub(crate) fn awake_since(&self, since: u64) -> u64 {
        self.now
            .saturating_sub(since)
            .saturating_sub(self.suspended_since(since))
    }

    /// Record this reading as a sample for later reaper runs.
    pub(crate) fn record(&self, state: &Path) -> Result<(), String> {
        let mut samples = self.samples.clone();
        samples.push((self.now, self.now_suspended));
        let file = TimelineFile {
            boot_id: self.boot_id.clone(),
            samples: compact(samples),
        };
        let path = timeline_path(state);
        let parent = path.parent().unwrap_or(state);
        let write = || -> std::io::Result<()> {
            fs::create_dir_all(parent)?;
            let mut temporary = NamedTempFile::new_in(parent)?;
            let mut bytes = serde_json::to_vec(&file).map_err(std::io::Error::other)?;
            bytes.push(b'\n');
            temporary.write_all(&bytes)?;
            temporary.persist(&path).map_err(|error| error.error)?;
            Ok(())
        };
        write().map_err(|error| format!("could not record {}: {error}", path.display()))
    }
}

/// Keep the first and last sample of every run of equal totals, and at most
/// [`MAX_SAMPLES`], newest first.
fn compact(samples: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    let total = |index: usize| samples.get(index).map(|&(_, suspended)| suspended);
    let kept = samples
        .iter()
        .enumerate()
        .filter(|&(index, &(_, suspended))| {
            index == 0
                || index + 1 == samples.len()
                || total(index - 1) != Some(suspended)
                || total(index + 1) != Some(suspended)
        })
        .map(|(_, &sample)| sample)
        .collect::<Vec<_>>();
    let excess = kept.len().saturating_sub(MAX_SAMPLES);
    kept.into_iter().skip(excess).collect()
}

pub(crate) fn timeline_path(state: &Path) -> PathBuf {
    state.join("reaping").join("suspend-timeline.json")
}

#[cfg(test)]
mod tests {
    use std::{
        fs, thread,
        time::{Duration, Instant},
    };

    use tempfile::tempdir;

    use super::{SuspendReading, SuspendSource, SuspendTimeline, SystemSuspend, clocks, compact};
    use crate::Clock;

    /// #637: the reading is the kernel's own pair of clocks. The boot clock
    /// agrees with `/proc/uptime`, an independent reader of the same clock,
    /// and the monotonic clock advances with `Instant` (also
    /// `CLOCK_MONOTONIC`) and never exceeds the boot clock.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn the_system_source_reads_the_kernels_boot_and_monotonic_clocks() {
        let uptime = |text: String| {
            text.split_whitespace()
                .next()
                .and_then(|field| field.parse::<f64>().ok())
                .map(Duration::from_secs_f64)
                .expect("parse /proc/uptime")
        };
        let before = uptime(fs::read_to_string("/proc/uptime").expect("read /proc/uptime"));
        let started = Instant::now();
        let (boot, first) = clocks().expect("read the clocks");
        thread::sleep(Duration::from_millis(50));
        let (_, second) = clocks().expect("read the clocks");
        let elapsed = started.elapsed();
        let after = uptime(fs::read_to_string("/proc/uptime").expect("read /proc/uptime"));
        let reading = SystemSuspend.read().expect("read suspended time");
        let advanced = second.checked_sub(first);
        assert_eq!(
            (
                before <= boot + Duration::from_millis(20)
                    && boot <= after + Duration::from_millis(20),
                first <= boot,
                advanced.is_some_and(|advanced| advanced >= Duration::from_millis(50)
                    && advanced <= elapsed + Duration::from_millis(20)),
                reading.boot_id.is_empty(),
                u128::from(reading.suspended_seconds) <= after.as_millis() / 1_000,
            ),
            (true, true, true, false, true),
            "uptime {before:?}..{after:?}, boot {boot:?}, monotonic {first:?} then {second:?} \
             over {elapsed:?}, reading {reading:?}"
        );
    }

    struct Fixed(u64);

    impl SuspendSource for Fixed {
        fn read(&self) -> Result<SuspendReading, String> {
            Ok(SuspendReading {
                boot_id: "boot".to_owned(),
                suspended_seconds: self.0,
            })
        }
    }

    fn at(epoch: i64) -> Clock {
        Clock::fixed(
            chrono::DateTime::<chrono::Utc>::from_timestamp(epoch, 0).expect("valid epoch"),
        )
    }

    /// A suspend a previous reaper run already saw still counts for a gap that
    /// began before it; a gap that began after it counts none of it.
    #[test]
    fn a_suspend_is_placed_between_the_samples_around_it() {
        let state = tempdir().expect("state");
        let before = SuspendTimeline::read(state.path(), &at(1_000), &Fixed(0)).expect("read");
        before.record(state.path()).expect("record");
        // Three hours asleep, then a reaper run on resume.
        let resumed =
            SuspendTimeline::read(state.path(), &at(11_830), &Fixed(10_800)).expect("read");
        resumed.record(state.path()).expect("record");
        let later = SuspendTimeline::read(state.path(), &at(12_130), &Fixed(10_800)).expect("read");
        assert_eq!(
            (
                resumed.suspended_since(990),
                later.suspended_since(990),
                later.awake_since(990),
                later.suspended_since(11_900),
                // Between the two samples: the suspend may have come after it,
                // bounded by the wall time left before the resume sample.
                later.suspended_since(1_010),
            ),
            (10_800, 10_800, 340, 0, 10_800)
        );
    }

    #[test]
    fn equal_totals_collapse_to_their_first_and_last_sample() {
        assert_eq!(
            compact(vec![(1, 0), (2, 0), (3, 0), (4, 5), (5, 5), (6, 5), (7, 5)]),
            vec![(1, 0), (3, 0), (4, 5), (7, 5)]
        );
    }
}
