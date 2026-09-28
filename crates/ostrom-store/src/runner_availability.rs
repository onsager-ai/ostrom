//! Which implementer runners are out of allowance, and until when (#626).
//!
//! A runner that refuses on an allowance limit is marked unavailable in
//! `<state>/runner-availability.json` until the reset time it reported, or for
//! the declared retry when it reported none. Dispatch reads it to skip that
//! runner; an entry whose `until` has passed is available again. This is
//! private state: one JSON object, written whole and replaced atomically.
//!
//! ```json
//! {"schema_version":1,"runners":{"agent/codex":{"until":"2026-09-28T11:34:00Z",
//!   "reset_reported":true,"message":"You've hit your usage limit. ...",
//!   "recorded_at":"2026-09-28T09:00:00Z","run_id":"..."}}}
//! ```

use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use chrono::{DateTime, NaiveDate, NaiveTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

use crate::{Clock, OstromPaths, StoreError, io_error, set_private_file_mode};

pub const RUNNER_AVAILABILITY_FILE: &str = "runner-availability.json";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerAvailability {
    pub schema_version: u32,
    #[serde(default)]
    pub runners: BTreeMap<String, UnavailableRunner>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnavailableRunner {
    /// When the runner may be tried again, RFC 3339 UTC.
    pub until: String,
    /// Whether `until` is the reset the runner reported. `false` means it
    /// reported none that could be read, and `until` is the declared retry.
    pub reset_reported: bool,
    /// The runner's own words, recorded so an unread reset can be checked.
    pub message: String,
    pub recorded_at: String,
    pub run_id: String,
}

#[must_use]
pub fn availability_path(paths: &OstromPaths) -> PathBuf {
    paths.state.join(RUNNER_AVAILABILITY_FILE)
}

/// The recorded availability. A missing file is an empty record; a file that
/// cannot be read or parsed is an error, never "every runner available".
pub fn read_availability(paths: &OstromPaths) -> Result<RunnerAvailability, StoreError> {
    let path = availability_path(paths);
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| StoreError::MalformedTrace {
            message: format!("{}: {error}", path.display()),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(RunnerAvailability {
            schema_version: 1,
            runners: BTreeMap::new(),
        }),
        Err(error) => Err(io_error("read runner availability", &path, error)),
    }
}

/// When `runner` becomes available again, or `None` when it is available now.
#[must_use]
pub fn unavailable_until(
    availability: &RunnerAvailability,
    runner: &str,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    availability
        .runners
        .get(runner)
        .and_then(|entry| DateTime::parse_from_rfc3339(&entry.until).ok())
        .map(|until| until.with_timezone(&Utc))
        .filter(|until| *until > now)
}

/// Mark `runner` unavailable until `until`.
pub fn mark_unavailable(
    paths: &OstromPaths,
    runner: &str,
    entry: UnavailableRunner,
) -> Result<(), StoreError> {
    let mut availability = read_availability(paths).unwrap_or_default();
    availability.schema_version = 1;
    availability.runners.insert(runner.to_owned(), entry);
    let path = availability_path(paths);
    write_private_atomic(
        &path,
        &serde_json::to_vec(&availability).unwrap_or_default(),
    )
}

fn write_private_atomic(path: &Path, contents: &[u8]) -> Result<(), StoreError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| io_error("create state", parent, error))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)
        .map_err(|error| io_error("create runner availability", path, error))?;
    temporary
        .write_all(contents)
        .and_then(|()| temporary.write_all(b"\n"))
        .map_err(|error| io_error("write runner availability", path, error))?;
    set_private_file_mode(temporary.path())?;
    temporary
        .persist(path)
        .map_err(|error| io_error("replace runner availability", path, error.error))?;
    Ok(())
}

/// The reset time an allowance refusal reported, read in `zone` (the machine's
/// local time, which is how Codex prints it). Recognised forms, inferred from
/// observed Codex and Claude Code messages: `Try again at 11:34 AM.`,
/// `Try again at Sep 29th, 2026 3:00 PM.`, and Claude's `...|<epoch seconds>`.
/// A time of day earlier than `now` is taken as tomorrow's.
#[must_use]
pub fn parse_reset<Tz: TimeZone>(
    message: &str,
    now: DateTime<Utc>,
    zone: &Tz,
) -> Option<DateTime<Utc>> {
    if let Some((_, epoch)) = message.rsplit_once('|')
        && let Ok(seconds) = epoch.trim().parse::<i64>()
    {
        return DateTime::from_timestamp(seconds, 0);
    }
    let (_, rest) = message.split_once("Try again at ")?;
    let text = rest.trim().trim_end_matches('.').trim();
    let local_now = now.with_timezone(zone);
    let (date, time) = match text.rsplit_once(", ") {
        Some((month_day, year_time)) => {
            let (month, day) = month_day.split_once(' ')?;
            let day = day.trim_end_matches(|character: char| character.is_ascii_alphabetic());
            let (year, time) = year_time.split_once(' ')?;
            let date =
                NaiveDate::parse_from_str(&format!("{month} {day} {year}"), "%b %d %Y").ok()?;
            (Some(date), time)
        }
        None => (None, text),
    };
    let time = NaiveTime::parse_from_str(time.trim(), "%I:%M %p").ok()?;
    let date = date.unwrap_or_else(|| local_now.date_naive());
    let mut reset = zone
        .from_local_datetime(&date.and_time(time))
        .earliest()?
        .with_timezone(&Utc);
    if reset <= now && date == local_now.date_naive() {
        reset += chrono::Duration::days(1);
    }
    Some(reset)
}

/// The machine's local zone, read without a clock call: the offset is derived
/// from the injected instant, never from `Local::now`.
#[must_use]
pub fn local_zone(clock: &Clock) -> chrono::FixedOffset {
    use chrono::Offset;
    chrono::Local
        .offset_from_utc_datetime(&clock.now().naive_utc())
        .fix()
}

#[cfg(test)]
mod tests {
    use chrono::{FixedOffset, TimeZone, Utc};

    use super::*;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 28, 9, 0, 0).unwrap()
    }

    #[test]
    fn a_reported_reset_is_read_in_the_machines_zone() {
        let utc = FixedOffset::east_opt(0).unwrap();
        assert_eq!(
            parse_reset(
                "You've hit your usage limit. Try again at 11:34 AM.",
                now(),
                &utc
            ),
            Some(Utc.with_ymd_and_hms(2026, 9, 28, 11, 34, 0).unwrap())
        );
        let plus_two = FixedOffset::east_opt(2 * 3600).unwrap();
        assert_eq!(
            parse_reset(
                "You've hit your usage limit. Try again at 12:34 PM.",
                now(),
                &plus_two
            ),
            Some(Utc.with_ymd_and_hms(2026, 9, 28, 10, 34, 0).unwrap())
        );
        assert_eq!(
            parse_reset(
                "You've hit your usage limit. Try again at 8:00 AM.",
                now(),
                &utc
            ),
            Some(Utc.with_ymd_and_hms(2026, 9, 29, 8, 0, 0).unwrap()),
            "a time already past today is tomorrow's"
        );
        assert_eq!(
            parse_reset(
                "You've hit your usage limit. Try again at Sep 30th, 2026 3:00 PM.",
                now(),
                &utc
            ),
            Some(Utc.with_ymd_and_hms(2026, 9, 30, 15, 0, 0).unwrap())
        );
        assert_eq!(
            parse_reset("Claude AI usage limit reached|1790000000", now(), &utc),
            DateTime::from_timestamp(1_790_000_000, 0)
        );
        assert_eq!(
            parse_reset("You've hit your usage limit. Try again later.", now(), &utc),
            None
        );
    }

    #[test]
    fn a_reset_in_the_past_is_available_again() {
        let mut availability = RunnerAvailability::default();
        availability.runners.insert(
            "agent/codex".to_owned(),
            UnavailableRunner {
                until: "2026-09-28T08:59:59Z".to_owned(),
                reset_reported: true,
                message: String::new(),
                recorded_at: "2026-09-28T08:00:00Z".to_owned(),
                run_id: "placeholder".to_owned(),
            },
        );
        assert_eq!(unavailable_until(&availability, "agent/codex", now()), None);
        availability.runners.get_mut("agent/codex").unwrap().until =
            "2026-09-28T09:00:01Z".to_owned();
        assert_eq!(
            unavailable_until(&availability, "agent/codex", now()),
            Some(Utc.with_ymd_and_hms(2026, 9, 28, 9, 0, 1).unwrap())
        );
        assert_eq!(
            unavailable_until(&availability, "agent/claude", now()),
            None
        );
    }
}
