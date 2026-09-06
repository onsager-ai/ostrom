use std::fs::File;
use std::io::{self, BufRead, BufReader, Lines};
use std::path::Path;
use std::process::ChildStdout;
use std::slice;

use crate::CaptureFault;

/// A source whose only operation is iteration over raw lines.
pub trait LineSource: Iterator<Item = Result<String, CaptureFault>> {}

/// Lines read from a spawned child process's standard output.
#[derive(Debug)]
pub struct ChildStdoutSource {
    lines: Lines<BufReader<ChildStdout>>,
}

impl ChildStdoutSource {
    /// Wrap an already-piped child standard-output handle.
    #[must_use]
    pub fn new(stdout: ChildStdout) -> Self {
        Self {
            lines: BufReader::new(stdout).lines(),
        }
    }
}

impl Iterator for ChildStdoutSource {
    type Item = Result<String, CaptureFault>;

    fn next(&mut self) -> Option<Self::Item> {
        self.lines
            .next()
            .map(|line| line.map_err(|error| unreadable("child stdout", error)))
    }
}

impl LineSource for ChildStdoutSource {}

/// Lines read from a file used by the attach path.
#[derive(Debug)]
pub struct FileLineSource {
    origin: String,
    lines: Lines<BufReader<File>>,
}

impl FileLineSource {
    /// Open `path` and iterate over the lines currently available from it.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, CaptureFault> {
        let path = path.as_ref();
        let origin = path.display().to_string();
        let file = File::open(path).map_err(|error| unreadable(&origin, error))?;
        Ok(Self {
            origin,
            lines: BufReader::new(file).lines(),
        })
    }
}

impl Iterator for FileLineSource {
    type Item = Result<String, CaptureFault>;

    fn next(&mut self) -> Option<Self::Item> {
        self.lines
            .next()
            .map(|line| line.map_err(|error| unreadable(&self.origin, error)))
    }
}

impl LineSource for FileLineSource {}

/// Lines borrowed from an in-memory slice.
#[derive(Debug)]
pub struct SliceLineSource<'a, T> {
    lines: slice::Iter<'a, T>,
}

impl<'a, T> SliceLineSource<'a, T> {
    /// Borrow a slice whose items can be viewed as strings.
    #[must_use]
    pub fn new(lines: &'a [T]) -> Self {
        Self {
            lines: lines.iter(),
        }
    }
}

impl<T: AsRef<str>> Iterator for SliceLineSource<'_, T> {
    type Item = Result<String, CaptureFault>;

    fn next(&mut self) -> Option<Self::Item> {
        self.lines.next().map(|line| Ok(line.as_ref().to_owned()))
    }
}

impl<T: AsRef<str>> LineSource for SliceLineSource<'_, T> {}

fn unreadable(origin: &str, error: io::Error) -> CaptureFault {
    CaptureFault::UnreadableSource {
        origin: origin.to_owned(),
        reason: error.to_string(),
    }
}
