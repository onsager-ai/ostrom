use std::{
    ffi::OsStr,
    fmt, fs, io,
    path::{Path, PathBuf},
};

const SHELL_RETIREMENT_LINE_THRESHOLD: usize = 0;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellFile {
    pub path: PathBuf,
    pub lines: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub struct ShellRetirementReport {
    pub files: Vec<ShellFile>,
    pub total_lines: usize,
    pub threshold: usize,
}

impl ShellRetirementReport {
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.total_lines <= self.threshold
    }
}

impl fmt::Display for ShellRetirementReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for file in &self.files {
            writeln!(
                formatter,
                "shell retirement: {}: {} lines",
                file.path.display(),
                file.lines
            )?;
        }
        write!(
            formatter,
            "shell retirement: found {} shell lines under plugins/ostrom; threshold is {} and target is 0",
            self.total_lines, self.threshold
        )
    }
}

/// Count every shell line remaining below `plugins/ostrom`.
pub fn check_shell_retirement(repository: &Path) -> io::Result<ShellRetirementReport> {
    check_shell_retirement_at(repository, SHELL_RETIREMENT_LINE_THRESHOLD)
}

fn check_shell_retirement_at(
    repository: &Path,
    threshold: usize,
) -> io::Result<ShellRetirementReport> {
    let plugin_root = repository.join("plugins/ostrom");
    let mut paths = Vec::new();
    collect_shell_files(&plugin_root, &mut paths)?;
    paths.sort();

    let mut files = Vec::with_capacity(paths.len());
    let mut total_lines = 0;
    for path in paths {
        let contents = fs::read(&path)?;
        let lines = line_count(&contents);
        total_lines += lines;
        files.push(ShellFile {
            path: path
                .strip_prefix(repository)
                .unwrap_or(path.as_path())
                .to_owned(),
            lines,
        });
    }

    Ok(ShellRetirementReport {
        files,
        total_lines,
        threshold,
    })
}

fn collect_shell_files(directory: &Path, paths: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_shell_files(&entry.path(), paths)?;
        } else if file_type.is_file() && entry.path().extension() == Some(OsStr::new("sh")) {
            paths.push(entry.path());
        }
    }
    Ok(())
}

fn line_count(contents: &[u8]) -> usize {
    contents.iter().filter(|byte| **byte == b'\n').count()
        + usize::from(contents.last().is_some_and(|byte| *byte != b'\n'))
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use tempfile::TempDir;

    use super::check_shell_retirement_at;

    struct FixtureTree {
        temporary: TempDir,
    }

    impl FixtureTree {
        fn new(files: &[(&str, usize)]) -> Self {
            let temporary = tempfile::tempdir().expect("fixture directory");
            for (path, lines) in files {
                write_lines(temporary.path(), path, *lines);
            }
            Self { temporary }
        }

        fn root(&self) -> &Path {
            self.temporary.path()
        }
    }

    #[test]
    fn accepts_a_fixture_under_the_line_threshold() {
        let fixture = FixtureTree::new(&[("plugins/ostrom/scripts/one.sh", 2)]);

        let report = check_shell_retirement_at(fixture.root(), 3).expect("check");

        assert!(report.is_clean());
        assert_eq!(report.total_lines, 2);
    }

    #[test]
    fn accepts_a_fixture_at_the_line_threshold() {
        let fixture = FixtureTree::new(&[("plugins/ostrom/tests/test.sh", 3)]);

        let report = check_shell_retirement_at(fixture.root(), 3).expect("check");

        assert!(report.is_clean());
        assert_eq!(report.total_lines, 3);
    }

    #[test]
    fn rejects_an_over_threshold_fixture_and_names_its_files() {
        let fixture = FixtureTree::new(&[
            ("plugins/ostrom/hooks/alpha.sh", 2),
            ("plugins/ostrom/scripts/beta.sh", 2),
        ]);

        let report = check_shell_retirement_at(fixture.root(), 3).expect("check");

        assert!(!report.is_clean());
        assert_eq!(report.total_lines, 4);
        assert_eq!(report.files.len(), 2);
        assert_eq!(
            report.files[0].path,
            Path::new("plugins/ostrom/hooks/alpha.sh")
        );
        assert_eq!(
            report.files[1].path,
            Path::new("plugins/ostrom/scripts/beta.sh")
        );
        let failure = report.to_string();
        assert!(failure.contains("plugins/ostrom/hooks/alpha.sh: 2 lines"));
        assert!(failure.contains("plugins/ostrom/scripts/beta.sh: 2 lines"));
        assert!(failure.contains("found 4 shell lines"));
    }

    #[test]
    fn counts_a_final_unterminated_line() {
        let fixture = FixtureTree::new(&[("plugins/ostrom/scripts/last.sh", 0)]);
        fs::write(
            fixture.root().join("plugins/ostrom/scripts/last.sh"),
            "one\ntwo",
        )
        .expect("write fixture");

        let report = check_shell_retirement_at(fixture.root(), 1).expect("check");

        assert_eq!(report.total_lines, 2);
        assert!(!report.is_clean());
    }

    fn write_lines(root: &Path, path: &str, lines: usize) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().expect("fixture parent")).expect("create fixture parent");
        fs::write(path, "line\n".repeat(lines)).expect("write fixture");
    }
}
