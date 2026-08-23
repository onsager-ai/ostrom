use std::{
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ostrom_core::PolicyManifest;
use rsa::{
    RsaPrivateKey, RsaPublicKey,
    pkcs1::{DecodeRsaPrivateKey as _, DecodeRsaPublicKey as _},
    pkcs1v15::{Signature, SigningKey, VerifyingKey},
    pkcs8::{DecodePrivateKey as _, DecodePublicKey as _},
    signature::{SignatureEncoding as _, Signer as _, Verifier as _},
    traits::PublicKeyParts as _,
};
use serde::{Deserialize, Serialize};
use serde_yaml::{Number, Value};
use sha2::{Digest as _, Sha256};
use tempfile::NamedTempFile;
use thiserror::Error;

const SIGNATURE_VERSION: u32 = 1;
const SIGNATURE_ALGORITHM: &str = "rsa-pkcs1v15-sha256";
const MINIMUM_RSA_BYTES: usize = 256;

#[derive(Debug, Error)]
pub enum PolicySignatureError {
    #[error("policy signature is missing: `{}`", path.display())]
    MissingSignature { path: PathBuf },
    #[error("could not read policy signature `{}`: {source}", path.display())]
    ReadSignature {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("policy signature `{}` is malformed: {message}", path.display())]
    MalformedSignature { path: PathBuf, message: String },
    #[error("policy signature names invalid key id `{0}`")]
    InvalidKeyId(String),
    #[error("policy signature names untrusted key `{key_id}`")]
    UnknownKey { key_id: String },
    #[error("trusted policy key `{key_id}` is unreadable: {source}")]
    ReadTrustedKey {
        key_id: String,
        #[source]
        source: io::Error,
    },
    #[error("trusted policy key `{key_id}` is malformed")]
    MalformedTrustedKey { key_id: String },
    #[error("trusted policy key `{key_id}` is smaller than 2048 bits")]
    WeakTrustedKey { key_id: String },
    #[error("policy signature verification failed for trusted key `{key_id}`")]
    Verification { key_id: String },
    #[error("private signing key is unreadable: {0}")]
    ReadPrivateKey(io::Error),
    #[error("private signing key is malformed")]
    MalformedPrivateKey,
    #[error("private signing key is smaller than 2048 bits")]
    WeakPrivateKey,
    #[error("could not canonicalise policy manifest: {0}")]
    Canonicalisation(serde_yaml::Error),
    #[error("could not create policy signature `{}`: {source}", path.display())]
    CreateSignature {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SignatureDocument {
    signature_version: u32,
    key_id: String,
    algorithm: String,
    signature: String,
}

/// Sign the fully composed manifest and atomically write its detached sidecar.
///
/// The private key is supplied only to this principal-side operation. Policy
/// loading has no private-key parameter and no signing-key environment seam.
pub fn sign_policy_manifest(
    manifest: &PolicyManifest,
    manifest_path: &Path,
    key_id: &str,
    private_key_path: &Path,
) -> Result<PathBuf, PolicySignatureError> {
    validate_key_id(key_id)?;
    let private_key_pem =
        fs::read_to_string(private_key_path).map_err(PolicySignatureError::ReadPrivateKey)?;
    let private_key = decode_private_key(&private_key_pem)?;
    if private_key.size() < MINIMUM_RSA_BYTES {
        return Err(PolicySignatureError::WeakPrivateKey);
    }

    let canonical = canonical_manifest(manifest)?;
    let signature = SigningKey::<Sha256>::new(private_key).sign(&canonical);
    let document = SignatureDocument {
        signature_version: SIGNATURE_VERSION,
        key_id: key_id.to_owned(),
        algorithm: SIGNATURE_ALGORITHM.to_owned(),
        signature: URL_SAFE_NO_PAD.encode(signature.to_bytes()),
    };
    let rendered = serde_yaml::to_string(&document).map_err(|source| {
        PolicySignatureError::MalformedSignature {
            path: signature_path(manifest_path),
            message: source.to_string(),
        }
    })?;
    let path = signature_path(manifest_path);
    write_atomic(&path, rendered.as_bytes())?;
    Ok(path)
}

/// Verify a composed manifest against a separately provisioned public-key set.
///
/// A key ID `principal` resolves only to `principal.pem` below
/// `trusted_keys_directory`; neither the manifest nor its sidecar can choose a
/// path outside that directory.
pub fn verify_policy_manifest(
    manifest: &PolicyManifest,
    manifest_path: &Path,
    trusted_keys_directory: &Path,
) -> Result<(), PolicySignatureError> {
    let path = signature_path(manifest_path);
    let source = match fs::read_to_string(&path) {
        Ok(source) => source,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(PolicySignatureError::MissingSignature { path });
        }
        Err(source) => return Err(PolicySignatureError::ReadSignature { path, source }),
    };
    let document: SignatureDocument = serde_yaml::from_str(&source).map_err(|error| {
        PolicySignatureError::MalformedSignature {
            path: path.clone(),
            message: error.to_string(),
        }
    })?;
    validate_signature_document(&document, &path)?;

    let key_path = trusted_keys_directory.join(format!("{}.pem", document.key_id));
    let public_key_pem = match fs::read_to_string(key_path) {
        Ok(source) => source,
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            return Err(PolicySignatureError::UnknownKey {
                key_id: document.key_id,
            });
        }
        Err(source) => {
            return Err(PolicySignatureError::ReadTrustedKey {
                key_id: document.key_id,
                source,
            });
        }
    };
    let public_key = decode_public_key(&public_key_pem).ok_or_else(|| {
        PolicySignatureError::MalformedTrustedKey {
            key_id: document.key_id.clone(),
        }
    })?;
    if public_key.size() < MINIMUM_RSA_BYTES {
        return Err(PolicySignatureError::WeakTrustedKey {
            key_id: document.key_id,
        });
    }
    let signature_bytes = URL_SAFE_NO_PAD
        .decode(document.signature.as_bytes())
        .map_err(|error| PolicySignatureError::MalformedSignature {
            path,
            message: format!("signature is not base64url: {error}"),
        })?;
    let signature = Signature::try_from(signature_bytes.as_slice()).map_err(|error| {
        PolicySignatureError::MalformedSignature {
            path: signature_path(manifest_path),
            message: format!("signature bytes are invalid: {error}"),
        }
    })?;
    VerifyingKey::<Sha256>::new(public_key)
        .verify(&canonical_manifest(manifest)?, &signature)
        .map_err(|_| PolicySignatureError::Verification {
            key_id: document.key_id,
        })
}

fn validate_signature_document(
    document: &SignatureDocument,
    path: &Path,
) -> Result<(), PolicySignatureError> {
    if document.signature_version != SIGNATURE_VERSION {
        return Err(PolicySignatureError::MalformedSignature {
            path: path.to_path_buf(),
            message: format!(
                "unsupported signature_version {}; expected {SIGNATURE_VERSION}",
                document.signature_version
            ),
        });
    }
    if document.algorithm != SIGNATURE_ALGORITHM {
        return Err(PolicySignatureError::MalformedSignature {
            path: path.to_path_buf(),
            message: format!("unsupported algorithm `{}`", document.algorithm),
        });
    }
    validate_key_id(&document.key_id)
}

fn validate_key_id(key_id: &str) -> Result<(), PolicySignatureError> {
    let valid = !key_id.is_empty()
        && key_id.len() <= 128
        && key_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if valid {
        Ok(())
    } else {
        Err(PolicySignatureError::InvalidKeyId(key_id.to_owned()))
    }
}

fn decode_private_key(pem: &str) -> Result<RsaPrivateKey, PolicySignatureError> {
    RsaPrivateKey::from_pkcs8_pem(pem)
        .or_else(|_| RsaPrivateKey::from_pkcs1_pem(pem))
        .map_err(|_| PolicySignatureError::MalformedPrivateKey)
}

fn decode_public_key(pem: &str) -> Option<RsaPublicKey> {
    RsaPublicKey::from_public_key_pem(pem)
        .or_else(|_| RsaPublicKey::from_pkcs1_pem(pem))
        .ok()
}

fn signature_path(manifest_path: &Path) -> PathBuf {
    let mut path = manifest_path.as_os_str().to_os_string();
    path.push(".sig");
    PathBuf::from(path)
}

/// Derive the stable content identity of a fully composed policy manifest.
///
/// This deliberately hashes the same canonical representation used by policy
/// signatures. It contains no authored version field, source path, map
/// iteration order, or observation time.
pub fn policy_manifest_digest(manifest: &PolicyManifest) -> Result<String, PolicySignatureError> {
    let digest = Sha256::digest(canonical_manifest(manifest)?);
    Ok(format!("{digest:x}"))
}

fn write_atomic(path: &Path, contents: &[u8]) -> Result<(), PolicySignatureError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut temporary =
        NamedTempFile::new_in(parent).map_err(|source| PolicySignatureError::CreateSignature {
            path: path.to_path_buf(),
            source,
        })?;
    temporary
        .write_all(contents)
        .map_err(|source| PolicySignatureError::CreateSignature {
            path: path.to_path_buf(),
            source,
        })?;
    temporary
        .persist(path)
        .map_err(|error| PolicySignatureError::CreateSignature {
            path: path.to_path_buf(),
            source: error.error,
        })?;
    Ok(())
}

fn canonical_manifest(manifest: &PolicyManifest) -> Result<Vec<u8>, PolicySignatureError> {
    let value = serde_yaml::to_value(manifest).map_err(PolicySignatureError::Canonicalisation)?;
    let mut output = b"ostrom-policy-manifest-v1\0".to_vec();
    encode_value(&value, &mut output);
    Ok(output)
}

fn encode_value(value: &Value, output: &mut Vec<u8>) {
    match value {
        Value::Null => output.push(b'n'),
        Value::Bool(value) => output.extend_from_slice(if *value { b"b1" } else { b"b0" }),
        Value::Number(value) => encode_number(value, output),
        Value::String(value) => encode_bytes(b's', value.as_bytes(), output),
        Value::Sequence(values) => {
            output.push(b'[');
            encode_len(values.len(), output);
            for value in values {
                encode_value(value, output);
            }
        }
        Value::Mapping(values) => {
            let mut entries = values
                .iter()
                .map(|(key, value)| {
                    let mut key_bytes = Vec::new();
                    let mut value_bytes = Vec::new();
                    encode_value(key, &mut key_bytes);
                    encode_value(value, &mut value_bytes);
                    (key_bytes, value_bytes)
                })
                .collect::<Vec<_>>();
            entries.sort();
            output.push(b'{');
            encode_len(entries.len(), output);
            for (key, value) in entries {
                encode_bytes(b'k', &key, output);
                encode_bytes(b'v', &value, output);
            }
        }
        Value::Tagged(tagged) => {
            output.push(b't');
            encode_bytes(b'g', tagged.tag.to_string().as_bytes(), output);
            encode_value(&tagged.value, output);
        }
    }
}

fn encode_number(value: &Number, output: &mut Vec<u8>) {
    if let Some(value) = value.as_u64() {
        encode_bytes(b'u', value.to_string().as_bytes(), output);
    } else if let Some(value) = value.as_i64() {
        encode_bytes(b'i', value.to_string().as_bytes(), output);
    } else if let Some(value) = value.as_f64() {
        encode_bytes(b'f', format!("{:016x}", value.to_bits()).as_bytes(), output);
    }
}

fn encode_bytes(kind: u8, value: &[u8], output: &mut Vec<u8>) {
    output.push(kind);
    encode_len(value.len(), output);
    output.extend_from_slice(value);
}

fn encode_len(length: usize, output: &mut Vec<u8>) {
    output.extend_from_slice(length.to_string().as_bytes());
    output.push(b':');
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::OnceLock};

    use rsa::{
        RsaPrivateKey, RsaPublicKey,
        pkcs8::{EncodePrivateKey as _, EncodePublicKey as _, LineEnding},
        rand_core::OsRng,
    };

    use super::{sign_policy_manifest, verify_policy_manifest};
    use ostrom_core::PolicyManifest;

    const KEY_ID: &str = "placeholder-principal";

    fn key_pair() -> &'static (String, String) {
        static KEY_PAIR: OnceLock<(String, String)> = OnceLock::new();
        KEY_PAIR.get_or_init(|| {
            let private = RsaPrivateKey::new(&mut OsRng, 2048).expect("generate test-only key");
            let public = RsaPublicKey::from(&private);
            let private_pem = private
                .to_pkcs8_pem(LineEnding::LF)
                .expect("encode private key")
                .to_string();
            let public_pem = public
                .to_public_key_pem(LineEnding::LF)
                .expect("encode public key");
            (private_pem, public_pem)
        })
    }

    fn manifest(yaml: &str) -> PolicyManifest {
        PolicyManifest::from_yaml(yaml).expect("valid policy fixture")
    }

    #[test]
    fn signature_verifies_with_the_named_trusted_key() {
        let root = tempfile::tempdir().expect("temporary directory");
        let manifest_path = root.path().join("policy.yaml");
        let private_path = root.path().join("principal.pem");
        let trusted = root.path().join("trusted");
        fs::create_dir(&trusted).expect("create trusted key directory");
        fs::write(&private_path, &key_pair().0).expect("write test-only private key");
        fs::write(trusted.join(format!("{KEY_ID}.pem")), &key_pair().1)
            .expect("write test-only public key");
        let policy = manifest("manifest_version: 1\nloops: {}\n");

        sign_policy_manifest(&policy, &manifest_path, KEY_ID, &private_path)
            .expect("sign manifest");
        verify_policy_manifest(&policy, &manifest_path, &trusted).expect("verify manifest");
    }

    #[test]
    fn canonicalisation_ignores_mapping_order_but_covers_values() {
        let root = tempfile::tempdir().expect("temporary directory");
        let manifest_path = root.path().join("policy.yaml");
        let private_path = root.path().join("principal.pem");
        let trusted = root.path().join("trusted");
        fs::create_dir(&trusted).expect("create trusted key directory");
        fs::write(&private_path, &key_pair().0).expect("write test-only private key");
        fs::write(trusted.join(format!("{KEY_ID}.pem")), &key_pair().1)
            .expect("write test-only public key");
        let signed = manifest(
            "manifest_version: 1\nactors:\n  builder: {name: Builder}\n  gatekeeper: {}\n",
        );
        let reordered = manifest(
            "actors:\n  gatekeeper: {}\n  builder: {name: Builder}\nmanifest_version: 1\n",
        );
        let changed = manifest(
            "manifest_version: 1\nactors:\n  builder: {name: Changed}\n  gatekeeper: {}\n",
        );

        sign_policy_manifest(&signed, &manifest_path, KEY_ID, &private_path)
            .expect("sign manifest");
        verify_policy_manifest(&reordered, &manifest_path, &trusted)
            .expect("mapping order is not semantic");
        let error = verify_policy_manifest(&changed, &manifest_path, &trusted)
            .expect_err("changed value must fail");
        assert!(error.to_string().contains("verification failed"));
    }

    #[test]
    fn absent_malformed_and_unknown_key_signatures_are_refused() {
        let root = tempfile::tempdir().expect("temporary directory");
        let manifest_path = root.path().join("policy.yaml");
        let private_path = root.path().join("principal.pem");
        let trusted = root.path().join("trusted");
        fs::create_dir(&trusted).expect("create trusted key directory");
        fs::write(&private_path, &key_pair().0).expect("write test-only private key");
        let policy = manifest("manifest_version: 1\n");

        let missing = verify_policy_manifest(&policy, &manifest_path, &trusted)
            .expect_err("missing signature must fail");
        assert!(missing.to_string().contains("signature is missing"));

        fs::write(
            root.path().join("policy.yaml.sig"),
            "signature_version: 1\nkey_id: placeholder-principal\n",
        )
        .expect("write malformed signature");
        let malformed = verify_policy_manifest(&policy, &manifest_path, &trusted)
            .expect_err("malformed signature must fail");
        assert!(malformed.to_string().contains("is malformed"));

        sign_policy_manifest(&policy, &manifest_path, KEY_ID, &private_path)
            .expect("sign manifest");
        let unknown = verify_policy_manifest(&policy, &manifest_path, &trusted)
            .expect_err("unknown key must fail");
        assert!(unknown.to_string().contains("untrusted key"));
    }
}
