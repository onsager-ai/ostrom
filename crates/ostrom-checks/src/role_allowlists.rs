use std::{collections::BTreeSet, fs, path::Path};

use serde::Deserialize;

struct RoleProtocol {
    role: &'static str,
    skills: &'static [&'static str],
    modeled_settings: &'static str,
}

const ROLE_PROTOCOLS: &[RoleProtocol] = &[
    RoleProtocol {
        role: "builder",
        skills: &["work"],
        modeled_settings: r#"{
            "permissions": {
                "allow": [
                    "Bash(ostrom lease *)",
                    "Bash(ostrom trace *)",
                    "Bash(ostrom repair-prs *)",
                    "Bash(ostrom sweep *)",
                    "Bash(ostrom select-work *)",
                    "Bash(ostrom credential *)",
                    "Bash(ostrom work-order *)",
                    "Bash(ostrom dispatch *)"
                ]
            }
        }"#,
    },
    RoleProtocol {
        role: "gatekeeper",
        skills: &["gatekeep", "merge"],
        modeled_settings: r#"{
            "permissions": {
                "allow": [
                    "Bash(ostrom lease *)",
                    "Bash(ostrom trace *)",
                    "Bash(ostrom config *)",
                    "Bash(ostrom credential *)"
                ]
            }
        }"#,
    },
];

#[derive(Debug, Default, Deserialize)]
struct RoleSettings {
    #[serde(default)]
    permissions: RolePermissions,
}

#[derive(Debug, Default, Deserialize)]
struct RolePermissions {
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    deny: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleAllowlistViolation {
    pub role: String,
    pub skill: Option<String>,
    pub subcommand: Option<String>,
    pub detail: String,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct RoleAllowlistReport {
    pub violations: Vec<RoleAllowlistViolation>,
}

impl RoleAllowlistReport {
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.violations.is_empty()
    }
}

/// Compare shipped role protocols with the operator settings used by passes.
///
/// `settings_directory` is explicit so doctor can inspect its resolved fixture
/// or runtime root without any build-time dependency on an operator's home.
#[must_use]
pub fn check_role_allowlists(settings_directory: &Path) -> RoleAllowlistReport {
    check_role_allowlists_with(shipped_prompt, |protocol| {
        let path = settings_directory.join(format!("{}.settings.json", protocol.role));
        fs::read_to_string(&path).map_err(|error| settings_read_error(&path, &error))
    })
}

/// Compare shipped role protocols with the checked-in model of the current
/// operator-managed grants. The model is validation input only: it neither
/// installs settings nor grants a role any capability.
#[must_use]
pub fn check_modeled_role_allowlists() -> RoleAllowlistReport {
    check_role_allowlists_with(shipped_prompt, |protocol| {
        Ok(protocol.modeled_settings.to_owned())
    })
}

/// Resolve a delivery-role skill's prompt from the assets compiled into the
/// binary. Returns `None` for a skill the build does not ship.
fn shipped_prompt(skill: &str) -> Option<String> {
    ostrom_store::role_skill_prompt(skill).map(str::to_owned)
}

fn settings_read_error(path: &Path, error: &std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::NotFound {
        format!("role settings are missing at {}", path.display())
    } else {
        format!(
            "could not read role settings at {}: {error}",
            path.display()
        )
    }
}

fn check_role_allowlists_with(
    prompt_source: impl Fn(&str) -> Option<String>,
    settings_source: impl Fn(&RoleProtocol) -> Result<String, String>,
) -> RoleAllowlistReport {
    let mut report = RoleAllowlistReport::default();
    for protocol in ROLE_PROTOCOLS {
        let source = match settings_source(protocol) {
            Ok(source) => source,
            Err(detail) => {
                report.violations.push(RoleAllowlistViolation {
                    role: protocol.role.to_owned(),
                    skill: None,
                    subcommand: None,
                    detail,
                });
                continue;
            }
        };
        let settings = match serde_json::from_str::<RoleSettings>(&source) {
            Ok(settings) => settings,
            Err(error) => {
                report.violations.push(RoleAllowlistViolation {
                    role: protocol.role.to_owned(),
                    skill: None,
                    subcommand: None,
                    detail: format!("role settings are not valid JSON: {error}"),
                });
                continue;
            }
        };
        let expected = expected_subcommands(&prompt_source, protocol, &mut report);
        for (skill, subcommand) in expected {
            if !settings.permissions.allows(&subcommand) {
                report.violations.push(RoleAllowlistViolation {
                    role: protocol.role.to_owned(),
                    skill: Some(skill.clone()),
                    subcommand: Some(subcommand.clone()),
                    detail: format!(
                        "skill {skill} invokes `ostrom {subcommand}` but the role cannot execute it"
                    ),
                });
            }
        }
    }
    report
}

fn expected_subcommands(
    prompt_source: &impl Fn(&str) -> Option<String>,
    protocol: &RoleProtocol,
    report: &mut RoleAllowlistReport,
) -> BTreeSet<(String, String)> {
    let mut expected = BTreeSet::new();
    for skill in protocol.skills {
        match prompt_source(skill) {
            Some(source) => {
                expected.extend(
                    invoked_subcommands(&source)
                        .into_iter()
                        .map(|subcommand| ((*skill).to_owned(), subcommand)),
                );
            }
            None => report.violations.push(RoleAllowlistViolation {
                role: protocol.role.to_owned(),
                skill: Some((*skill).to_owned()),
                subcommand: None,
                detail: format!("the build ships no prompt for role skill {skill}"),
            }),
        }
    }
    expected
}

impl RolePermissions {
    fn allows(&self, subcommand: &str) -> bool {
        self.allow
            .iter()
            .any(|rule| bash_rule_allows(rule, subcommand))
            && !self
                .deny
                .iter()
                .any(|rule| bash_rule_allows(rule, subcommand))
    }
}

fn bash_rule_allows(rule: &str, subcommand: &str) -> bool {
    if rule == "Bash" {
        return true;
    }
    let Some(command) = rule
        .strip_prefix("Bash(")
        .and_then(|value| value.strip_suffix(')'))
    else {
        return false;
    };
    if command.trim() == "*" {
        return true;
    }
    let mut words = command.split_ascii_whitespace();
    let Some(executable) = words.next() else {
        return false;
    };
    let executable = executable.trim_matches(['\'', '"']);
    if executable == "ostrom:*" {
        return true;
    }
    if executable.rsplit('/').next() != Some("ostrom") {
        return false;
    }
    let Some(granted) = words.next() else {
        return false;
    };
    let granted = granted
        .trim_matches(['\'', '"'])
        .trim_end_matches(":*")
        .trim_end_matches('*');
    granted.is_empty() || granted == subcommand
}

/// Extract executable `ostrom` subcommands from shell fences only.
///
/// Prose examples such as "never run `ostrom gate`" are intentionally not
/// commands. Within a shell fence, a nested child after `--` is likewise not a
/// separate harness invocation; the outer `ostrom credential` call is the
/// command whose Bash permission Claude requests.
pub(crate) fn invoked_subcommands(markdown: &str) -> BTreeSet<String> {
    let mut commands = BTreeSet::new();
    let mut shell_fence = false;
    for line in markdown.lines() {
        let trimmed = line.trim_start();
        if let Some(info) = trimmed.strip_prefix("```") {
            if shell_fence {
                shell_fence = false;
            } else {
                shell_fence = matches!(info.trim(), "sh" | "bash" | "shell");
            }
            continue;
        }
        if !shell_fence || trimmed.starts_with('#') {
            continue;
        }
        if let Some(subcommand) = shell_line_subcommand(line) {
            commands.insert(subcommand.to_owned());
        }
    }
    commands
}

fn shell_line_subcommand(line: &str) -> Option<&str> {
    let mut search_from = 0;
    while let Some(relative) = line.get(search_from..)?.find("ostrom") {
        let index = search_from + relative;
        let before = line.as_bytes().get(index.wrapping_sub(1)).copied();
        let after = line.as_bytes().get(index + "ostrom".len()).copied();
        let word_boundaries = before.is_none_or(|byte| !shell_word_byte(byte))
            && after.is_none_or(|byte| !shell_word_byte(byte));
        if word_boundaries && executable_prefix(&line[..index]) {
            let remainder = line[index + "ostrom".len()..].trim_start();
            let end = remainder
                .find(|character: char| !shell_word_character(character))
                .unwrap_or(remainder.len());
            let subcommand = &remainder[..end];
            if !subcommand.is_empty() {
                return Some(subcommand);
            }
        }
        search_from = index + "ostrom".len();
    }
    None
}

const fn shell_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')
}

const fn shell_word_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || matches!(character, '-' | '_')
}

fn executable_prefix(prefix: &str) -> bool {
    let prefix = prefix.trim();
    if prefix.is_empty() || prefix.ends_with("$(") {
        return true;
    }
    let segment = prefix
        .rsplit_once("&&")
        .map(|(_, tail)| tail)
        .or_else(|| prefix.rsplit_once("||").map(|(_, tail)| tail))
        .or_else(|| prefix.rsplit_once(';').map(|(_, tail)| tail))
        .unwrap_or(prefix)
        .trim();
    let mut saw_prefix = false;
    for word in segment.split_ascii_whitespace() {
        if matches!(
            word,
            "if" | "then" | "do" | "!" | "command" | "exec" | "env"
        ) || (word.contains('=') && !word.starts_with('='))
        {
            saw_prefix = true;
            continue;
        }
        return false;
    }
    saw_prefix
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::tempdir;

    use super::{
        ROLE_PROTOCOLS, bash_rule_allows, check_modeled_role_allowlists,
        check_role_allowlists_with, invoked_subcommands, settings_read_error,
    };

    #[test]
    fn extracts_commands_from_shell_fences_not_prose_or_credential_children() {
        let source = r#"
Never run `ostrom forbidden`.

```sh
ostrom sweep
selected="$(ostrom select-work select owner)"
ostrom credential gatekeeper repo -- ostrom gate repo#1
echo ostrom ignored
```

```json
{"command":"ostrom ignored-too"}
```
"#;
        assert_eq!(
            invoked_subcommands(source).into_iter().collect::<Vec<_>>(),
            ["credential", "select-work", "sweep"]
        );
    }

    #[test]
    fn understands_the_supported_claude_bash_allowlist_shapes() {
        assert!(bash_rule_allows("Bash(ostrom trace *)", "trace"));
        assert!(bash_rule_allows("Bash(ostrom trace:*)", "trace"));
        assert!(bash_rule_allows("Bash(ostrom *)", "anything"));
        assert!(bash_rule_allows("Bash(ostrom:*)", "anything"));
        assert!(bash_rule_allows(
            "Bash(/usr/local/bin/ostrom sweep *)",
            "sweep"
        ));
        assert!(!bash_rule_allows("Bash(ostrom trace *)", "sweep"));
        assert!(!bash_rule_allows("Bash(echo ostrom sweep)", "sweep"));
    }

    #[test]
    fn modeled_allowlists_cover_the_current_shipped_role_skills() {
        let report = check_modeled_role_allowlists();
        assert!(report.is_clean(), "{:#?}", report.violations);
    }

    #[test]
    fn every_role_skill_has_a_prompt_compiled_into_the_binary() {
        for protocol in ROLE_PROTOCOLS {
            for skill in protocol.skills {
                assert!(
                    ostrom_store::role_skill_prompt(skill).is_some(),
                    "role {} names skill {skill}, which the build does not ship",
                    protocol.role
                );
            }
        }
    }

    #[test]
    fn a_new_skill_command_fails_when_its_role_settings_do_not_allow_it() {
        let root = tempdir().expect("role allowlist fixture");
        let roles = root.path().join("roles");
        let prompts = |skill: &str| {
            Some(if skill == "work" {
                "```sh\nostrom newly-added\n```\n".to_owned()
            } else {
                "# no commands\n".to_owned()
            })
        };
        fs::create_dir_all(&roles).unwrap();
        for role in ["builder", "gatekeeper"] {
            fs::write(
                roles.join(format!("{role}.settings.json")),
                r#"{"permissions":{"allow":["Bash(ostrom trace *)"]}}"#,
            )
            .unwrap();
        }

        let report = check_role_allowlists_with(prompts, |protocol| {
            let path = roles.join(format!("{}.settings.json", protocol.role));
            fs::read_to_string(&path).map_err(|error| settings_read_error(&path, &error))
        });
        let gap = report
            .violations
            .iter()
            .find(|violation| violation.subcommand.as_deref() == Some("newly-added"))
            .expect("new command gap");
        assert_eq!(gap.role, "builder");
        assert_eq!(gap.skill.as_deref(), Some("work"));
        assert!(gap.detail.contains("ostrom newly-added"));
    }
}
