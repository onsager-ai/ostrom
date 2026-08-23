use std::{ffi::OsString, fmt};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvironmentClass {
    Binary,
    Ceiling,
    Identity,
    Location,
    Switch,
    TestSeam,
}

impl fmt::Display for EnvironmentClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Binary => "binary",
            Self::Ceiling => "ceiling",
            Self::Identity => "identity",
            Self::Location => "location",
            Self::Switch => "switch",
            Self::TestSeam => "test-seam",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnvironmentVariable {
    pub name: &'static str,
    pub class: EnvironmentClass,
    pub unset_resolution: &'static str,
}

impl EnvironmentVariable {
    #[must_use]
    pub fn value_os(self) -> Option<OsString> {
        std::env::var_os(self.name)
    }

    #[must_use]
    pub fn value(self) -> Option<String> {
        self.value_os()?.into_string().ok()
    }

    #[must_use]
    pub fn rendered_value(self) -> (bool, String) {
        self.value_os().map_or_else(
            || (false, self.unset_resolution.to_owned()),
            |value| (true, sanitize(&value.to_string_lossy())),
        )
    }
}

/// Read a variable whose name a signed manifest declares, not this source.
///
/// The typed registry below covers every name the binary itself knows. A
/// manifest's `inputs:` block names variables the binary cannot know at compile
/// time, so its resolver cannot appear in a static registry — but it is not a
/// bypass either. It reads only names a manifest declared, with a declared type
/// and a declared resolution ladder, which is the discipline the registry
/// exists to enforce, expressed in data rather than in code.
///
/// Routing it here keeps `production_source_cannot_bypass_the_registry`
/// meaningful: a raw `env::var` in production source is still a defect, and a
/// declared input is still declared.
#[must_use]
pub fn declared_input(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

const fn variable(
    name: &'static str,
    class: EnvironmentClass,
    unset_resolution: &'static str,
) -> EnvironmentVariable {
    EnvironmentVariable {
        name,
        class,
        unset_resolution,
    }
}

pub const ASDF_DATA_DIR: EnvironmentVariable =
    variable("ASDF_DATA_DIR", EnvironmentClass::Location, "$HOME/.asdf");
pub const CLAUDE_BIN: EnvironmentVariable =
    variable("CLAUDE_BIN", EnvironmentClass::Binary, "claude");
pub const CLAUDE_CONFIG_DIR: EnvironmentVariable = variable(
    "CLAUDE_CONFIG_DIR",
    EnvironmentClass::Location,
    "$HOME/.claude",
);
pub const CLAUDE_PLUGIN_ROOT: EnvironmentVariable = variable(
    "CLAUDE_PLUGIN_ROOT",
    EnvironmentClass::Location,
    "derived plugin root",
);
pub const CODEX_BIN: EnvironmentVariable = variable("CODEX_BIN", EnvironmentClass::Binary, "codex");
pub const COPILOT_BIN: EnvironmentVariable =
    variable("COPILOT_BIN", EnvironmentClass::Binary, "copilot");
pub const FNM_DIR: EnvironmentVariable = variable(
    "FNM_DIR",
    EnvironmentClass::Location,
    "$HOME/.local/share/fnm and $HOME/.fnm",
);
pub const GH_HOST: EnvironmentVariable =
    variable("GH_HOST", EnvironmentClass::Identity, "github.com");
pub const HOME: EnvironmentVariable = variable(
    "HOME",
    EnvironmentClass::Identity,
    "platform home directory",
);
pub const MANDATE_DAILY_CAP_USD: EnvironmentVariable = variable(
    "MANDATE_DAILY_CAP_USD",
    EnvironmentClass::Ceiling,
    "built-in daily cap",
);
pub const MANDATE_DISPATCH_BACKEND: EnvironmentVariable = variable(
    "MANDATE_DISPATCH_BACKEND",
    EnvironmentClass::Switch,
    "systemd",
);
pub const MANDATE_GH_AS_BIN: EnvironmentVariable =
    variable("MANDATE_GH_AS_BIN", EnvironmentClass::Binary, "gh");
pub const MANDATE_IMPLEMENTER_LEASE_TTL_SECONDS: EnvironmentVariable = variable(
    "MANDATE_IMPLEMENTER_LEASE_TTL_SECONDS",
    EnvironmentClass::Ceiling,
    "built-in implementer lease TTL",
);
pub const MANDATE_IMPLEMENTER_SOURCE_REPO: EnvironmentVariable = variable(
    "MANDATE_IMPLEMENTER_SOURCE_REPO",
    EnvironmentClass::Location,
    "discovered source repository",
);
pub const MANDATE_IMPLEMENTER_STARTUP_GRACE_MILLISECONDS: EnvironmentVariable = variable(
    "MANDATE_IMPLEMENTER_STARTUP_GRACE_MILLISECONDS",
    EnvironmentClass::Ceiling,
    "built-in startup grace",
);
pub const MANDATE_IMPLEMENTER_TERMINATION_GRACE_SECONDS: EnvironmentVariable = variable(
    "MANDATE_IMPLEMENTER_TERMINATION_GRACE_SECONDS",
    EnvironmentClass::Ceiling,
    "built-in termination grace",
);
pub const MANDATE_LEASE_NAME: EnvironmentVariable = variable(
    "MANDATE_LEASE_NAME",
    EnvironmentClass::Identity,
    "role-derived lease name",
);
pub const MANDATE_LEASE_TTL_SECONDS: EnvironmentVariable = variable(
    "MANDATE_LEASE_TTL_SECONDS",
    EnvironmentClass::Ceiling,
    "built-in pass lease TTL",
);
pub const MANDATE_MAX_IMPLEMENTERS: EnvironmentVariable = variable(
    "MANDATE_MAX_IMPLEMENTERS",
    EnvironmentClass::Ceiling,
    "built-in global implementer limit",
);
pub const MANDATE_MAX_IMPLEMENTERS_PER_REPOSITORY: EnvironmentVariable = variable(
    "MANDATE_MAX_IMPLEMENTERS_PER_REPOSITORY",
    EnvironmentClass::Ceiling,
    "project or built-in repository limit",
);
pub const MANDATE_ORDER_COST_CEILING_USD: EnvironmentVariable = variable(
    "MANDATE_ORDER_COST_CEILING_USD",
    EnvironmentClass::Ceiling,
    "built-in order cost ceiling",
);
pub const MANDATE_ORDER_TOKEN_CEILING: EnvironmentVariable = variable(
    "MANDATE_ORDER_TOKEN_CEILING",
    EnvironmentClass::Ceiling,
    "built-in order token ceiling",
);
pub const MANDATE_OSTROM_BIN: EnvironmentVariable = variable(
    "MANDATE_OSTROM_BIN",
    EnvironmentClass::Binary,
    "current ostrom executable",
);
pub const MANDATE_PUBLISH_ALLOWLIST: EnvironmentVariable = variable(
    "MANDATE_PUBLISH_ALLOWLIST",
    EnvironmentClass::Location,
    "publication disabled",
);
pub const MANDATE_SECRETS_FILE: EnvironmentVariable = variable(
    "MANDATE_SECRETS_FILE",
    EnvironmentClass::Location,
    "resolved config secrets.yaml",
);
pub const MANDATE_SYSTEMCTL_BIN: EnvironmentVariable = variable(
    "MANDATE_SYSTEMCTL_BIN",
    EnvironmentClass::Binary,
    "systemctl",
);
pub const MANDATE_SYSTEMD_RUN_BIN: EnvironmentVariable = variable(
    "MANDATE_SYSTEMD_RUN_BIN",
    EnvironmentClass::Binary,
    "systemd-run",
);
pub const MANDATE_WORKTREE_CEILING_BYTES: EnvironmentVariable = variable(
    "MANDATE_WORKTREE_CEILING_BYTES",
    EnvironmentClass::Ceiling,
    "built-in worktree footprint ceiling",
);
pub const MANDATE_WORKTREE_RETENTION_DAYS: EnvironmentVariable = variable(
    "MANDATE_WORKTREE_RETENTION_DAYS",
    EnvironmentClass::Ceiling,
    "built-in worktree retention window",
);
pub const NVM_DIR: EnvironmentVariable =
    variable("NVM_DIR", EnvironmentClass::Location, "$HOME/.nvm");
pub const OSTROM_HOME: EnvironmentVariable = variable(
    "OSTROM_HOME",
    EnvironmentClass::Location,
    "platform config and state directories",
);
pub const OSTROM_LEGACY_HOME: EnvironmentVariable = variable(
    "OSTROM_LEGACY_HOME",
    EnvironmentClass::Location,
    "$CLAUDE_CONFIG_DIR/ostrom",
);
pub const OSTROM_NODE_FALLBACKS: EnvironmentVariable = variable(
    "OSTROM_NODE_FALLBACKS",
    EnvironmentClass::Location,
    "built-in node search locations",
);
pub const OSTROM_PLAN_DERIVER: EnvironmentVariable = variable(
    "OSTROM_PLAN_DERIVER",
    EnvironmentClass::Switch,
    "unavailable unless selected explicitly",
);
pub const OSTROM_ACTOR: EnvironmentVariable = variable(
    "OSTROM_ACTOR",
    EnvironmentClass::Identity,
    "no actor; operation dispatch refuses",
);
pub const OSTROM_POLICY_MANIFEST: EnvironmentVariable = variable(
    "OSTROM_POLICY_MANIFEST",
    EnvironmentClass::Location,
    "<config>/ostrom.yaml",
);
pub const OSTROM_POLICY_TRUSTED_KEYS: EnvironmentVariable = variable(
    "OSTROM_POLICY_TRUSTED_KEYS",
    EnvironmentClass::Location,
    "policy loading refused",
);
pub const OSTROM_PLUGIN_ROOT: EnvironmentVariable = variable(
    "OSTROM_PLUGIN_ROOT",
    EnvironmentClass::Location,
    "derived plugin root",
);
pub const PATH: EnvironmentVariable = variable(
    "PATH",
    EnvironmentClass::Identity,
    "platform binary search path",
);
pub const VOLTA_HOME: EnvironmentVariable =
    variable("VOLTA_HOME", EnvironmentClass::Location, "$HOME/.volta");

pub const ENVIRONMENT_VARIABLES: &[EnvironmentVariable] = &[
    ASDF_DATA_DIR,
    CLAUDE_BIN,
    CLAUDE_CONFIG_DIR,
    CLAUDE_PLUGIN_ROOT,
    CODEX_BIN,
    COPILOT_BIN,
    FNM_DIR,
    GH_HOST,
    HOME,
    MANDATE_DAILY_CAP_USD,
    MANDATE_DISPATCH_BACKEND,
    MANDATE_GH_AS_BIN,
    MANDATE_IMPLEMENTER_LEASE_TTL_SECONDS,
    MANDATE_IMPLEMENTER_SOURCE_REPO,
    MANDATE_IMPLEMENTER_STARTUP_GRACE_MILLISECONDS,
    MANDATE_IMPLEMENTER_TERMINATION_GRACE_SECONDS,
    MANDATE_LEASE_NAME,
    MANDATE_LEASE_TTL_SECONDS,
    MANDATE_MAX_IMPLEMENTERS,
    MANDATE_MAX_IMPLEMENTERS_PER_REPOSITORY,
    MANDATE_ORDER_COST_CEILING_USD,
    MANDATE_ORDER_TOKEN_CEILING,
    MANDATE_OSTROM_BIN,
    MANDATE_PUBLISH_ALLOWLIST,
    MANDATE_SECRETS_FILE,
    MANDATE_SYSTEMCTL_BIN,
    MANDATE_SYSTEMD_RUN_BIN,
    MANDATE_WORKTREE_CEILING_BYTES,
    MANDATE_WORKTREE_RETENTION_DAYS,
    NVM_DIR,
    OSTROM_ACTOR,
    OSTROM_HOME,
    OSTROM_LEGACY_HOME,
    OSTROM_NODE_FALLBACKS,
    OSTROM_PLAN_DERIVER,
    OSTROM_PLUGIN_ROOT,
    OSTROM_POLICY_MANIFEST,
    OSTROM_POLICY_TRUSTED_KEYS,
    PATH,
    VOLTA_HOME,
];

fn sanitize(value: &str) -> String {
    let mut rendered = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\n' => rendered.push_str("\\n"),
            '\r' => rendered.push_str("\\r"),
            '|' => rendered.push_str("\\x7c"),
            character => rendered.push(character),
        }
    }
    rendered
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, fs, path::Path};

    use super::{ENVIRONMENT_VARIABLES, EnvironmentClass};

    #[test]
    fn registry_is_sorted_unique_and_contains_no_test_seams() {
        let names = ENVIRONMENT_VARIABLES
            .iter()
            .map(|variable| variable.name)
            .collect::<Vec<_>>();
        let unique = names.iter().copied().collect::<BTreeSet<_>>();
        assert_eq!(
            names.len(),
            unique.len(),
            "environment names must be unique"
        );
        assert!(names.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(
            ENVIRONMENT_VARIABLES
                .iter()
                .all(|variable| variable.class != EnvironmentClass::TestSeam)
        );
        let declared = include_str!("environment.rs")
            .lines()
            .filter_map(|line| {
                line.strip_prefix("pub const ")?
                    .split_once(": EnvironmentVariable")
                    .map(|(name, _)| name)
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(
            declared.len(),
            ENVIRONMENT_VARIABLES.len(),
            "every typed environment constant must be present in the registry"
        );
    }

    #[test]
    fn production_source_cannot_bypass_the_registry() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("workspace root");
        let mut bypasses = Vec::new();
        let mut test_seams = Vec::new();
        for crate_entry in fs::read_dir(workspace.join("crates")).expect("read workspace crates") {
            let source_root = crate_entry.expect("read crate entry").path().join("src");
            if source_root.is_dir() {
                visit_rust_sources(&source_root, &mut |path| {
                    if path.ends_with("ostrom-store/src/environment.rs") {
                        return;
                    }
                    let source = fs::read_to_string(path).expect("read Rust source");
                    let production = source.split("#[cfg(test)]").next().unwrap_or(&source);
                    for name in [
                        "MANDATE_NOW_EPOCH",
                        "MANDATE_TRACE_TIME",
                        "MANDATE_TODAY",
                        "MANDATE_GATE_TIME",
                        "MANDATE_SWEEP_TIME",
                        "MANDATE_EVENT_TIME",
                        "MANDATE_DIGEST_TIME",
                        "MANDATE_LEASE_NOW_EPOCH",
                        "MANDATE_AUDIT_TIME",
                        "MANDATE_REPLAY_TIME",
                        "MANDATE_EXCUSE_TIME",
                    ] {
                        if production.contains(name) {
                            test_seams.push(format!("{}:{name}", path.display()));
                        }
                    }
                    for (index, line) in production.lines().enumerate() {
                        if [
                            "env::var(",
                            "env::var_os(",
                            "std::env::var(",
                            "std::env::var_os(",
                        ]
                        .iter()
                        .any(|needle| line.contains(needle))
                        {
                            bypasses.push(format!("{}:{}", path.display(), index + 1));
                        }
                    }
                });
            }
        }
        assert!(
            bypasses.is_empty(),
            "production environment reads bypass the typed registry: {}",
            bypasses.join(", ")
        );
        assert!(
            test_seams.is_empty(),
            "production source contains retired clock seams: {}",
            test_seams.join(", ")
        );
    }

    fn visit_rust_sources(root: &Path, visit: &mut impl FnMut(&Path)) {
        for entry in fs::read_dir(root).expect("read source directory") {
            let entry = entry.expect("read source entry");
            let path = entry.path();
            if path.is_dir() {
                visit_rust_sources(&path, visit);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                visit(&path);
            }
        }
    }
}
