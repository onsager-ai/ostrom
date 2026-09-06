//! Golden fixtures for normalisers.
//!
//! A case directory contains `raw.ndjson`, `expected.jsonl`, and `meta.toml`.
//! [`run_case`] always drives a fresh normaliser through both a slice source
//! and a file source, then requires their canonical event bytes to agree.
//! Metadata uses four non-empty TOML basic strings: `harness`, `cli_version`,
//! `capture_date`, and `exercises`. Under a harness directory, each line of
//! every regular file in `refuses/` is tested in isolation and must make the
//! normaliser return a [`CaptureFault`].

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use ethogram::{StampFields, serialise_event, stamp};
use thiserror::Error;

use crate::{CaptureFault, FileLineSource, LineSource, Normaliser, SliceLineSource};

/// Deterministic run identity used for every golden event.
pub const FIXED_RUN_ID: &str = "umwelt-golden-run";
/// Deterministic sink timestamp used for every golden event.
pub const FIXED_TS: &str = "2000-01-01T00:00:00.000Z";

const RAW_FILE: &str = "raw.ndjson";
const EXPECTED_FILE: &str = "expected.jsonl";
const META_FILE: &str = "meta.toml";
const REFUSES_DIRECTORY: &str = "refuses";

/// Recorded provenance for one golden case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaseMetadata {
    /// Harness whose output was captured.
    pub harness: String,
    /// Exact harness CLI version used for the capture.
    pub cli_version: String,
    /// Date on which the capture was taken.
    pub capture_date: String,
    /// Behaviour exercised by the case.
    pub exercises: String,
}

/// Result of successfully checking one case.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaseReport {
    /// Validated fixture provenance.
    pub metadata: CaseMetadata,
    /// Number of canonical events produced by each source.
    pub events: usize,
}

/// Counts returned after walking one harness's fixture directory.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CorpusReport {
    /// Number of case directories successfully checked.
    pub cases: usize,
    /// Number of refusal lines that produced a [`CaptureFault`].
    pub refusals: usize,
}

/// A structural or comparison failure in a golden corpus.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum GoldenError {
    /// A required fixture path could not be read.
    #[error("cannot read fixture {path}: {reason}")]
    FixtureUnreadable {
        /// Path that could not be read.
        path: PathBuf,
        /// Underlying failure.
        reason: String,
    },

    /// The required metadata file is absent.
    #[error("case is missing metadata file {path}")]
    MetadataMissing {
        /// Expected metadata path.
        path: PathBuf,
    },

    /// Metadata does not follow the golden schema.
    #[error("invalid metadata in {path}: {reason}")]
    MetadataInvalid {
        /// Invalid metadata path.
        path: PathBuf,
        /// Parse or schema failure.
        reason: String,
    },

    /// Reading or normalising a source was refused.
    #[error("capture failed through {mode}: {fault}")]
    Capture {
        /// Source mode being exercised.
        mode: &'static str,
        /// Capture refusal.
        #[source]
        fault: CaptureFault,
    },

    /// Ethogram could not serialise a produced event.
    #[error("cannot serialise golden event {seq}: {reason}")]
    Serialisation {
        /// One-based event sequence.
        seq: u64,
        /// Serialisation failure.
        reason: String,
    },

    /// Spawn-shaped and attach-shaped input produced different bytes.
    #[error(
        "in-memory and file sources differ at event line {line}: in-memory={in_memory:?}, file={file:?}"
    )]
    SourceMismatch {
        /// One-based first differing output line.
        line: usize,
        /// In-memory output at that line, or `None` at end of output.
        in_memory: Option<Vec<u8>>,
        /// File output at that line, or `None` at end of output.
        file: Option<Vec<u8>>,
    },

    /// Produced canonical bytes differ from the expected fixture line.
    #[error("golden output differs at event line {line}: expected={expected:?}, actual={actual:?}")]
    ExpectedMismatch {
        /// One-based first differing output line.
        line: usize,
        /// Fixture bytes at that line, or `None` at end of fixture.
        expected: Option<Vec<u8>>,
        /// Produced bytes at that line, or `None` at end of output.
        actual: Option<Vec<u8>>,
    },

    /// A refusal line was accepted rather than rejected.
    #[error("refusal line {line} in {path} was accepted by the normaliser")]
    RefusalAccepted {
        /// File containing the accepted line.
        path: PathBuf,
        /// One-based raw line number.
        line: usize,
    },
}

/// Run one case through both required source implementations.
///
/// Expected event lines are compared directly to ethogram's canonical bytes;
/// they are never parsed into values for field equality.
pub fn run_case<N, F>(
    mut normaliser_factory: F,
    case_directory: impl AsRef<Path>,
) -> Result<CaseReport, GoldenError>
where
    N: Normaliser,
    F: FnMut() -> N,
{
    run_case_with_factory(case_directory.as_ref(), &mut normaliser_factory)
}

/// Walk and run one harness's fixture corpus.
///
/// A successful report with `cases == 0` means `harness_directory` existed,
/// was readable, and contained no case directories other than `refuses/`.
/// Zero is reported explicitly so callers that expect a populated corpus can
/// make that expectation an assertion; a missing or unreadable directory is
/// always an error.
pub fn walk_corpus<N, F>(
    mut normaliser_factory: F,
    harness_directory: impl AsRef<Path>,
) -> Result<CorpusReport, GoldenError>
where
    N: Normaliser,
    F: FnMut() -> N,
{
    let harness_directory = harness_directory.as_ref();
    let mut entries = directory_entries(harness_directory)?;
    entries.sort();

    let mut report = CorpusReport::default();
    for path in entries {
        if !is_directory(&path)? {
            continue;
        }
        if path
            .file_name()
            .is_some_and(|name| name == REFUSES_DIRECTORY)
        {
            report.refusals += run_refusals(&path, &mut normaliser_factory)?;
            continue;
        }
        run_case_with_factory(&path, &mut normaliser_factory)?;
        report.cases += 1;
    }
    Ok(report)
}

fn run_case_with_factory<N, F>(
    case_directory: &Path,
    normaliser_factory: &mut F,
) -> Result<CaseReport, GoldenError>
where
    N: Normaliser,
    F: FnMut() -> N,
{
    let metadata = read_metadata(&case_directory.join(META_FILE))?;
    let raw_path = case_directory.join(RAW_FILE);
    let raw = read_utf8(&raw_path)?;
    let raw_lines: Vec<String> = raw.lines().map(str::to_owned).collect();

    let in_memory = normalise(
        normaliser_factory(),
        SliceLineSource::new(&raw_lines),
        "in-memory source",
    )?;
    let file_source = FileLineSource::open(&raw_path).map_err(|fault| GoldenError::Capture {
        mode: "file source",
        fault,
    })?;
    let file = normalise(normaliser_factory(), file_source, "file source")?;
    compare_lines(&in_memory, &file).map_err(|difference| GoldenError::SourceMismatch {
        line: difference.line,
        in_memory: difference.left,
        file: difference.right,
    })?;

    let expected = jsonl_lines(&case_directory.join(EXPECTED_FILE))?;
    compare_lines(&expected, &in_memory).map_err(|difference| GoldenError::ExpectedMismatch {
        line: difference.line,
        expected: difference.left,
        actual: difference.right,
    })?;

    Ok(CaseReport {
        metadata,
        events: in_memory.len(),
    })
}

fn normalise<N, S>(
    mut normaliser: N,
    source: S,
    mode: &'static str,
) -> Result<Vec<Vec<u8>>, GoldenError>
where
    N: Normaliser,
    S: LineSource,
{
    let mut drafts = Vec::new();
    for raw in source {
        let raw = raw.map_err(|fault| GoldenError::Capture { mode, fault })?;
        drafts.extend(
            normaliser
                .line(&raw)
                .map_err(|fault| GoldenError::Capture { mode, fault })?,
        );
    }
    drafts.extend(
        normaliser
            .finish()
            .map_err(|fault| GoldenError::Capture { mode, fault })?,
    );

    drafts
        .into_iter()
        .enumerate()
        .map(|(index, draft)| {
            let seq = u64::try_from(index)
                .ok()
                .and_then(|value| value.checked_add(1))
                .expect("an in-memory vector cannot contain u64::MAX events");
            let event = stamp(
                draft,
                StampFields {
                    run_id: FIXED_RUN_ID.to_owned(),
                    seq,
                    ts: FIXED_TS.to_owned(),
                },
            );
            serialise_event(&event)
                .map(String::into_bytes)
                .map_err(|error| GoldenError::Serialisation {
                    seq,
                    reason: error.to_string(),
                })
        })
        .collect()
}

fn run_refusals<N, F>(
    refuses_directory: &Path,
    normaliser_factory: &mut F,
) -> Result<usize, GoldenError>
where
    N: Normaliser,
    F: FnMut() -> N,
{
    let mut files = directory_entries(refuses_directory)?;
    files.sort();
    let mut refusals = 0;

    for path in files {
        if is_directory(&path)? {
            continue;
        }
        let source = FileLineSource::open(&path).map_err(|fault| GoldenError::Capture {
            mode: "refusal file",
            fault,
        })?;
        for (index, raw) in source.enumerate() {
            let raw = raw.map_err(|fault| GoldenError::Capture {
                mode: "refusal file",
                fault,
            })?;
            if normaliser_factory().line(&raw).is_ok() {
                return Err(GoldenError::RefusalAccepted {
                    path: path.clone(),
                    line: index + 1,
                });
            }
            refusals += 1;
        }
    }
    Ok(refusals)
}

fn read_metadata(path: &Path) -> Result<CaseMetadata, GoldenError> {
    if !path.exists() {
        return Err(GoldenError::MetadataMissing {
            path: path.to_owned(),
        });
    }
    let input = read_utf8(path)?;
    parse_metadata(&input).map_err(|reason| GoldenError::MetadataInvalid {
        path: path.to_owned(),
        reason,
    })
}

fn parse_metadata(input: &str) -> Result<CaseMetadata, String> {
    let mut fields = BTreeMap::new();
    for (index, raw_line) in input.lines().enumerate() {
        let line = strip_comment(raw_line)?.trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("line {} is not a key/value assignment", index + 1))?;
        let key = key.trim();
        if !matches!(
            key,
            "harness" | "cli_version" | "capture_date" | "exercises"
        ) {
            return Err(format!("line {} has unknown key {key:?}", index + 1));
        }
        let value = serde_json::from_str::<String>(value.trim()).map_err(|error| {
            format!(
                "line {} value must be a TOML basic string: {error}",
                index + 1
            )
        })?;
        if value.is_empty() {
            return Err(format!("line {} value must not be empty", index + 1));
        }
        if fields.insert(key.to_owned(), value).is_some() {
            return Err(format!("line {} repeats key {key:?}", index + 1));
        }
    }

    Ok(CaseMetadata {
        harness: take_field(&mut fields, "harness")?,
        cli_version: take_field(&mut fields, "cli_version")?,
        capture_date: take_field(&mut fields, "capture_date")?,
        exercises: take_field(&mut fields, "exercises")?,
    })
}

fn strip_comment(line: &str) -> Result<&str, String> {
    let mut quoted = false;
    let mut escaped = false;
    for (index, byte) in line.bytes().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        if quoted && byte == b'\\' {
            escaped = true;
        } else if byte == b'"' {
            quoted = !quoted;
        } else if !quoted && byte == b'#' {
            return Ok(&line[..index]);
        }
    }
    if quoted || escaped {
        return Err("unterminated string in metadata".to_owned());
    }
    Ok(line)
}

fn take_field(fields: &mut BTreeMap<String, String>, key: &str) -> Result<String, String> {
    fields
        .remove(key)
        .ok_or_else(|| format!("missing required key {key:?}"))
}

fn jsonl_lines(path: &Path) -> Result<Vec<Vec<u8>>, GoldenError> {
    let bytes = fs::read(path).map_err(|error| fixture_unreadable(path, error))?;
    let mut lines: Vec<Vec<u8>> = bytes
        .split(|byte| *byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect();
    if lines.last().is_some_and(Vec::is_empty) {
        lines.pop();
    }
    Ok(lines)
}

fn compare_lines(left: &[Vec<u8>], right: &[Vec<u8>]) -> Result<(), LineDifference> {
    let count = left.len().max(right.len());
    for index in 0..count {
        if left.get(index) != right.get(index) {
            return Err(LineDifference {
                line: index + 1,
                left: left.get(index).cloned(),
                right: right.get(index).cloned(),
            });
        }
    }
    Ok(())
}

struct LineDifference {
    line: usize,
    left: Option<Vec<u8>>,
    right: Option<Vec<u8>>,
}

fn read_utf8(path: &Path) -> Result<String, GoldenError> {
    fs::read_to_string(path).map_err(|error| fixture_unreadable(path, error))
}

fn directory_entries(path: &Path) -> Result<Vec<PathBuf>, GoldenError> {
    fs::read_dir(path)
        .map_err(|error| fixture_unreadable(path, error))?
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|error| fixture_unreadable(path, error))
        })
        .collect()
}

fn is_directory(path: &Path) -> Result<bool, GoldenError> {
    path.metadata()
        .map(|metadata| metadata.is_dir())
        .map_err(|error| fixture_unreadable(path, error))
}

fn fixture_unreadable(path: &Path, error: std::io::Error) -> GoldenError {
    GoldenError::FixtureUnreadable {
        path: path.to_owned(),
        reason: error.to_string(),
    }
}
