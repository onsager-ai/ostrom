// Each integration test binary includes this module and uses a subset of it.
#![allow(dead_code)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::OnceLock,
};

use rsa::{
    RsaPrivateKey, RsaPublicKey,
    pkcs8::{EncodePrivateKey as _, EncodePublicKey as _, LineEnding},
    rand_core::OsRng,
};
use tempfile::TempDir;

const KEY_ID: &str = "placeholder-principal";

pub fn sign_manifest(manifest: &Path) -> PathBuf {
    sign_manifest_as(manifest, manifest, None)
}

pub fn sign_manifest_from(manifest: &Path, argument: &Path, working_directory: &Path) -> PathBuf {
    sign_manifest_as(manifest, argument, Some(working_directory))
}

fn sign_manifest_as(manifest: &Path, argument: &Path, working_directory: Option<&Path>) -> PathBuf {
    let substrate = manifest.parent().expect("manifest directory");
    let trusted_keys = substrate.join("trusted-policy-keys");
    fs::create_dir_all(&trusted_keys).expect("create trusted policy key directory");
    fs::write(trusted_keys.join(format!("{KEY_ID}.pem")), &key_pair().1)
        .expect("write test-only trusted public key");

    let principal = TempDir::new().expect("temporary signing principal directory");
    let private_key = principal.path().join("private.pem");
    fs::write(&private_key, &key_pair().0).expect("write generated test-only private key");
    let mut command = Command::new(env!("CARGO_BIN_EXE_ostrom"));
    command
        .arg("sign")
        .args(["--key-id", KEY_ID, "--key"])
        .arg(&private_key)
        .arg(argument);
    if let Some(working_directory) = working_directory {
        command.current_dir(working_directory);
    }
    let output = command.output().expect("run policy signer");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    drop(principal);

    assert!(
        !substrate.join("private.pem").exists(),
        "loop substrate must not contain the signing key"
    );
    trusted_keys
}

fn key_pair() -> &'static (String, String) {
    static KEY_PAIR: OnceLock<(String, String)> = OnceLock::new();
    KEY_PAIR.get_or_init(|| {
        let private = RsaPrivateKey::new(&mut OsRng, 2048).expect("generate test-only key");
        let public = RsaPublicKey::from(&private);
        let private_pem = private
            .to_pkcs8_pem(LineEnding::LF)
            .expect("encode test-only private key")
            .to_string();
        let public_pem = public
            .to_public_key_pem(LineEnding::LF)
            .expect("encode test-only public key");
        (private_pem, public_pem)
    })
}

/// Copy a checked-in fixture tree into a temporary directory so signing may
/// write the detached signature beside it without dirtying the repository.
pub fn copy_fixture_directory(source: &Path) -> TempDir {
    fn copy_contents(source: &Path, destination: &Path) {
        for entry in fs::read_dir(source).expect("read fixture directory") {
            let entry = entry.expect("read fixture entry");
            let source_path = entry.path();
            let destination_path = destination.join(entry.file_name());
            if entry.file_type().expect("read fixture file type").is_dir() {
                fs::create_dir(&destination_path).expect("create copied fixture directory");
                copy_contents(&source_path, &destination_path);
            } else {
                fs::copy(&source_path, &destination_path).expect("copy fixture file");
            }
        }
    }

    let destination = TempDir::new().expect("temporary fixture directory");
    copy_contents(source, destination.path());
    destination
}
