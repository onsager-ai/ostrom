use std::ffi::OsString;

#[derive(Debug, Clone, Copy)]
pub(crate) struct EnvironmentVariable(&'static str);

impl EnvironmentVariable {
    pub(crate) fn value_os(self) -> Option<OsString> {
        std::env::var_os(self.0)
    }
}

pub(crate) const ASDF_DATA_DIR: EnvironmentVariable = EnvironmentVariable("ASDF_DATA_DIR");
pub(crate) const CODEX_BIN: EnvironmentVariable = EnvironmentVariable("CODEX_BIN");
pub(crate) const FNM_DIR: EnvironmentVariable = EnvironmentVariable("FNM_DIR");
pub(crate) const HOME: EnvironmentVariable = EnvironmentVariable("HOME");
pub(crate) const NVM_DIR: EnvironmentVariable = EnvironmentVariable("NVM_DIR");
pub(crate) const PATH: EnvironmentVariable = EnvironmentVariable("PATH");
pub(crate) const VOLTA_HOME: EnvironmentVariable = EnvironmentVariable("VOLTA_HOME");
