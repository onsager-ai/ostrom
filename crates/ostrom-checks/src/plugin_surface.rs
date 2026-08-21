use std::{fmt, fs, io, path::Path};

use serde_json::Value;

use crate::check_modeled_role_allowlists;

const HOOK_COMMANDS: [&str; 2] = [
    "if command -v ostrom >/dev/null 2>&1; then ostrom hook session-start; fi",
    "if command -v ostrom >/dev/null 2>&1; then ostrom hook digest; fi",
];

const REQUIRED_CONTRACTS: [(&str, &str); 37] = [
    (
        "skills/gatekeep/SKILL.md",
        "ostrom lease acquire \"$lease_owner\"",
    ),
    (
        "skills/gatekeep/SKILL.md",
        "ostrom lease release \"$lease_owner\"",
    ),
    (
        "skills/gatekeep/SKILL.md",
        "Never infer concurrency or lease",
    ),
    (
        "skills/gatekeep/SKILL.md",
        "ostrom trace append pass-started",
    ),
    (
        "skills/gatekeep/SKILL.md",
        "ostrom trace append item-selected",
    ),
    ("skills/gatekeep/SKILL.md", "ostrom trace append pass-ended"),
    (
        "skills/gatekeep/SKILL.md",
        "exact same `ostrom credential` invocation once immediately",
    ),
    (
        "skills/gatekeep/SKILL.md",
        "continue to the next repository",
    ),
    (
        "skills/gatekeep/SKILL.md",
        "Continue enumerating every other roster repository",
    ),
    (
        "skills/gatekeep/SKILL.md",
        "repository once to `skipped_repos`",
    ),
    (
        "skills/gatekeep/SKILL.md",
        "**Credentials cannot be loaded at all.**",
    ),
    (
        "skills/gatekeep/SKILL.md",
        "**No exit-`111` path may run the command under an ambient credential",
    ),
    (
        "skills/merge/SKILL.md",
        "ostrom trace append artifact-produced",
    ),
    (
        "skills/merge/SKILL.md",
        "ostrom trace append gate-verdict-consumed",
    ),
    ("skills/merge/SKILL.md", "failing_conditions"),
    ("skills/merge/SKILL.md", "**Do not approve.**"),
    ("skills/merge/SKILL.md", "Do not pass `--body` here"),
    (
        "skills/merge/SKILL.md",
        "ostrom trace append decision-taken",
    ),
    ("skills/merge/SKILL.md", "closingIssuesReferences"),
    ("skills/merge/SKILL.md", "close_outcome=\"none-declared\""),
    ("skills/merge/SKILL.md", "close_outcome=\"all-closed\""),
    (
        "skills/merge/SKILL.md",
        "--argjson declared \"$declared\" --argjson still_open \"$still_open\"",
    ),
    ("skills/work/SKILL.md", "MANDATE_LEASE_NAME=builder.lease"),
    ("skills/work/SKILL.md", "\nostrom sweep\n"),
    (
        "skills/work/SKILL.md",
        "A per-repository concurrency refusal skips only that candidate",
    ),
    (
        "skills/work/SKILL.md",
        "invocation input as a natural-language filter",
    ),
    ("skills/work/SKILL.md", "ostrom trace append pass-started"),
    ("skills/work/SKILL.md", "ostrom trace append item-worked"),
    ("skills/work/SKILL.md", "ostrom trace append pass-ended"),
    ("skills/work/SKILL.md", "ostrom repair-prs"),
    (
        "skills/work/SKILL.md",
        "per-pass cap is **3 repair attempts**",
    ),
    ("skills/work/SKILL.md", "order stays undispatched"),
    ("skills/brief/SKILL.md", "**Plan match rate**"),
    (
        "skills/brief/SKILL.md",
        "PROBLEM: computed plans never applied",
    ),
    (
        "skills/brief/SKILL.md",
        "Never combine absent\nplans with rejected plans",
    ),
    (
        "../../docs/role-permission-boundaries.md",
        "self-asserted and advisory",
    ),
    (
        "skills/work/SKILL.md",
        "argument-hint: \"[optional queue focus, e.g. project name or item class]\"",
    ),
];

const FORBIDDEN_CONTRACTS: [(&str, &str); 4] = [
    ("skills/gatekeep/SKILL.md", "stop this iteration"),
    ("skills/gatekeep/SKILL.md", "failed_repo"),
    ("skills/work/SKILL.md", "$ARGUMENTS"),
    (
        "skills/work/SKILL.md",
        "documented Claude implementer fallback",
    ),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginSurfaceViolation {
    pub path: String,
    pub detail: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct PluginSurfaceReport {
    pub violations: Vec<PluginSurfaceViolation>,
}

impl PluginSurfaceReport {
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.violations.is_empty()
    }
}

impl fmt::Display for PluginSurfaceReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for violation in &self.violations {
            writeln!(
                formatter,
                "plugin surface: {}: {}",
                violation.path, violation.detail
            )?;
        }
        Ok(())
    }
}

/// Verify that the shipped plugin wiring and skill protocols still agree with
/// the native CLI surface they invoke.
pub fn check_plugin_surface(repository: &Path) -> io::Result<PluginSurfaceReport> {
    let plugin = repository.join("plugins/ostrom");
    let mut report = PluginSurfaceReport::default();
    check_hooks(&plugin, &mut report);
    check_contracts(&plugin, &mut report);
    check_role_permissions(&plugin, &mut report);
    check_private_paths(&plugin, &plugin, &mut report)?;
    Ok(report)
}

fn check_role_permissions(plugin: &Path, report: &mut PluginSurfaceReport) {
    for role_violation in check_modeled_role_allowlists(plugin).violations {
        let path = role_violation.skill.as_ref().map_or_else(
            || "plugins/ostrom/skills".to_owned(),
            |skill| format!("plugins/ostrom/skills/{skill}/SKILL.md"),
        );
        violation(
            report,
            &path,
            format!(
                "role allowlist gap for {}: {}",
                role_violation.role, role_violation.detail
            ),
        );
    }
}

fn violation(report: &mut PluginSurfaceReport, path: &str, detail: impl Into<String>) {
    report.violations.push(PluginSurfaceViolation {
        path: path.to_owned(),
        detail: detail.into(),
    });
}

fn check_hooks(plugin: &Path, report: &mut PluginSurfaceReport) {
    let relative = "plugins/ostrom/hooks/hooks.json";
    let path = plugin.join("hooks/hooks.json");
    let Ok(source) = fs::read(&path) else {
        violation(
            report,
            relative,
            "could not read shipped hook configuration",
        );
        return;
    };
    let Ok(document) = serde_json::from_slice::<Value>(&source) else {
        violation(
            report,
            relative,
            "shipped hook configuration is not valid JSON",
        );
        return;
    };
    let Some(entries) = document
        .pointer("/hooks/SessionStart")
        .and_then(Value::as_array)
    else {
        violation(report, relative, "SessionStart hook list is missing");
        return;
    };
    let commands = entries
        .iter()
        .filter_map(|entry| entry.pointer("/hooks/0/command").and_then(Value::as_str))
        .collect::<Vec<_>>();
    if commands != HOOK_COMMANDS {
        violation(
            report,
            relative,
            "SessionStart must contain the two fail-open native commands exactly once",
        );
    }
    if entries.iter().any(|entry| {
        entry.pointer("/hooks/0/type").and_then(Value::as_str) != Some("command")
            || entry
                .pointer("/hooks/0/command")
                .and_then(Value::as_str)
                .is_some_and(|command| command.contains("bash "))
    }) {
        violation(
            report,
            relative,
            "SessionStart hooks must be command hooks that invoke the native CLI",
        );
    }
}

fn check_contracts(plugin: &Path, report: &mut PluginSurfaceReport) {
    for (relative, needle) in REQUIRED_CONTRACTS {
        let path = plugin.join(relative);
        match fs::read_to_string(&path) {
            Ok(source) if source.contains(needle) => {}
            Ok(_) => violation(
                report,
                &display_path(relative),
                format!("required protocol contract is missing: {needle}"),
            ),
            Err(_) => violation(
                report,
                &display_path(relative),
                "could not read required protocol document",
            ),
        }
    }
    for (relative, needle) in FORBIDDEN_CONTRACTS {
        let path = plugin.join(relative);
        if fs::read_to_string(path).is_ok_and(|source| source.contains(needle)) {
            violation(
                report,
                &display_path(relative),
                format!("retired protocol text reappeared: {needle}"),
            );
        }
    }

    let work = fs::read_to_string(plugin.join("skills/work/SKILL.md")).unwrap_or_default();
    let repair = work.find("ostrom repair-prs");
    let selection = work.find("Then read, in order:");
    if !matches!((repair, selection), (Some(repair), Some(selection)) if repair < selection) {
        violation(
            report,
            "plugins/ostrom/skills/work/SKILL.md",
            "repair-prs must run before queue-backed selection",
        );
    }
}

fn display_path(relative: &str) -> String {
    relative
        .strip_prefix("../../")
        .map_or_else(|| format!("plugins/ostrom/{relative}"), str::to_owned)
}

fn check_private_paths(
    plugin: &Path,
    directory: &Path,
    report: &mut PluginSurfaceReport,
) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            check_private_paths(plugin, &path, report)?;
            continue;
        }
        if !entry.file_type()?.is_file() {
            continue;
        }
        let source = fs::read(&path)?;
        let text = String::from_utf8_lossy(&source);
        if ["/home/", "~/projects/", "dotclaude"]
            .iter()
            .any(|private| text.contains(private))
        {
            let relative = path.strip_prefix(plugin).unwrap_or(path.as_path());
            violation(
                report,
                &format!("plugins/ostrom/{}", relative.display()),
                "shipped plugin contains a machine-specific path",
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use tempfile::TempDir;

    use super::{FORBIDDEN_CONTRACTS, HOOK_COMMANDS, REQUIRED_CONTRACTS, check_plugin_surface};

    struct Fixture {
        root: TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let root = tempfile::tempdir().expect("plugin surface fixture");
            let plugin = root.path().join("plugins/ostrom");
            for (relative, _) in REQUIRED_CONTRACTS {
                let path = plugin.join(relative);
                fs::create_dir_all(path.parent().expect("contract parent"))
                    .expect("create contract parent");
            }
            let mut by_path = std::collections::BTreeMap::<&str, Vec<&str>>::new();
            for (path, needle) in REQUIRED_CONTRACTS {
                by_path.entry(path).or_default().push(needle);
            }
            for (path, needles) in by_path {
                fs::write(plugin.join(path), format!("{}\n", needles.join("\n")))
                    .expect("write contract fixture");
            }
            let work = plugin.join("skills/work/SKILL.md");
            let mut source = fs::read_to_string(&work).expect("read work fixture");
            source = source.replace(
                "ostrom repair-prs",
                "ostrom repair-prs\nThen read, in order:\n",
            );
            fs::write(work, source).expect("write ordered work fixture");
            fs::create_dir_all(plugin.join("hooks")).expect("create hooks fixture");
            fs::write(
                plugin.join("hooks/hooks.json"),
                serde_json::to_vec(&serde_json::json!({
                    "hooks": {"SessionStart": HOOK_COMMANDS.map(|command| {
                        serde_json::json!({"hooks": [{"type": "command", "command": command}]})
                    })}
                }))
                .expect("serialize hooks fixture"),
            )
            .expect("write hooks fixture");
            Self { root }
        }

        fn root(&self) -> &Path {
            self.root.path()
        }
    }

    #[test]
    fn accepts_a_coherent_plugin_surface() {
        let fixture = Fixture::new();
        let report = check_plugin_surface(fixture.root()).expect("check fixture");
        assert!(report.is_clean(), "{report}");
    }

    #[test]
    fn rejects_hook_protocol_and_private_path_drift() {
        let fixture = Fixture::new();
        fs::write(
            fixture.root().join("plugins/ostrom/hooks/hooks.json"),
            r#"{"hooks":{"SessionStart":[]}}"#,
        )
        .expect("break hook fixture");
        fs::create_dir_all(fixture.root().join("plugins/ostrom/config"))
            .expect("create private path fixture directory");
        fs::write(
            fixture.root().join("plugins/ostrom/config/private.txt"),
            "checkout=/home/placeholder/private\n",
        )
        .expect("write private path fixture");
        let forbidden = FORBIDDEN_CONTRACTS[0];
        let path = fixture.root().join("plugins/ostrom").join(forbidden.0);
        let mut source = fs::read_to_string(&path).expect("read protocol fixture");
        source.push_str(forbidden.1);
        fs::write(path, source).expect("break protocol fixture");

        let report = check_plugin_surface(fixture.root()).expect("check broken fixture");
        assert_eq!(report.violations.len(), 3, "{report}");
        let rendered = report.to_string();
        assert!(rendered.contains("two fail-open native commands"));
        assert!(rendered.contains("machine-specific path"));
        assert!(rendered.contains("retired protocol text reappeared"));
    }

    #[test]
    fn rejects_a_skill_command_outside_its_role_allowlist() {
        let fixture = Fixture::new();
        let work = fixture.root().join("plugins/ostrom/skills/work/SKILL.md");
        let mut source = fs::read_to_string(&work).expect("read work protocol");
        source.push_str("\n```sh\nostrom newly-added\n```\n");
        fs::write(work, source).expect("add ungranted command");

        let report = check_plugin_surface(fixture.root()).expect("check changed skill");
        assert!(!report.is_clean());
        let rendered = report.to_string();
        assert!(rendered.contains("role allowlist gap for builder"));
        assert!(rendered.contains("ostrom newly-added"));
    }
}
