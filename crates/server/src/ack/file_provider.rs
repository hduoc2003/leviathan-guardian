//! Local-file ACK secret provider: a stable Guardian identity without AWS.
//!
//! Reads hex-encoded Falcon and ECDSA ACK secret keys from files whose paths
//! come from `GUARDIAN_ACK_FALCON_SECRET_PATH` and
//! `GUARDIAN_ACK_ECDSA_SECRET_PATH`. This lets a self-hosted Guardian keep a
//! fixed identity across restarts without AWS Secrets Manager. The file format
//! is the hex string emitted by the `ack-keygen` binary — identical to what the
//! Secrets Manager path stores — so the same key material is portable between
//! the two.
//!
//! The alternative (no provider) mints a fresh keypair on every boot, which
//! changes the on-chain ack-key commitment and freezes any account that pinned
//! the old one (recovery then requires a per-account `SwitchGuardian`).

use crate::error::{GuardianError, Result};
use crate::secret::{SecretBytes, SecretString};
use async_trait::async_trait;
use miden_protocol::crypto::dsa::ecdsa_k256_keccak::SecretKey as EcdsaSecretKey;
use miden_protocol::crypto::dsa::falcon512_poseidon2::SecretKey as FalconSecretKey;
use miden_protocol::utils::serde::{Deserializable, Serializable};
use std::path::{Path, PathBuf};

use super::secrets_manager::{AckSecretProvider, decode_secret_key};

const ENV_ACK_FALCON_SECRET_PATH: &str = "GUARDIAN_ACK_FALCON_SECRET_PATH";
const ENV_ACK_ECDSA_SECRET_PATH: &str = "GUARDIAN_ACK_ECDSA_SECRET_PATH";
const ENV_ACK_SECRET_AUTOGEN: &str = "GUARDIAN_ACK_SECRET_AUTOGEN";

/// Reads the ACK secrets from local files. Construct with [`from_env`].
///
/// [`from_env`]: FileSecretProvider::from_env
pub struct FileSecretProvider {
    falcon_secret_path: PathBuf,
    /// `None` when `GUARDIAN_ACK_ECDSA_SECRET_PATH` is unset. The ECDSA secret
    /// file is only read for the in-memory ECDSA backend; an `aws-kms` backend
    /// signs with the KMS key and never calls [`ecdsa_secret_key`], so the path
    /// is optional and only required to exist when actually consulted.
    ///
    /// [`ecdsa_secret_key`]: AckSecretProvider::ecdsa_secret_key
    ecdsa_secret_path: Option<PathBuf>,
    /// Opt-in, because a mistyped path or an unmounted volume would otherwise
    /// mint a new identity and freeze every account pinned to the old commitment.
    autogen: bool,
}

impl FileSecretProvider {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            falcon_secret_path: required_path(ENV_ACK_FALCON_SECRET_PATH)?,
            ecdsa_secret_path: optional_path(ENV_ACK_ECDSA_SECRET_PATH)?,
            autogen: autogen_from_env()?,
        })
    }

    fn parsed_secret_key<T, F, G>(&self, path: &Path, parser: F, generate: G) -> Result<T>
    where
        F: FnOnce(&[u8]) -> std::result::Result<T, String>,
        G: FnOnce() -> SecretBytes,
    {
        if self.autogen
            && !path.try_exists().map_err(|error| {
                GuardianError::ConfigurationError(format!(
                    "Failed to check ack secret file {}: {error}",
                    path.display()
                ))
            })?
        {
            write_new_secret_file(path, generate())?;
        }
        ensure_owner_only(path)?;
        // Read-and-wrap in one expression so the key bytes never bind to a bare
        // `String` (CONTRIBUTING.md, "Secrets in server memory").
        let contents = SecretString::new(std::fs::read_to_string(path).map_err(|error| {
            GuardianError::ConfigurationError(format!(
                "Failed to read ack secret file {}: {error}",
                path.display()
            ))
        })?);
        decode_secret_key(
            &format!("Ack secret file {}", path.display()),
            &contents,
            parser,
        )
    }
}

#[async_trait]
impl AckSecretProvider for FileSecretProvider {
    async fn falcon_secret_key(&self) -> Result<FalconSecretKey> {
        self.parsed_secret_key(
            &self.falcon_secret_path,
            |secret_bytes| {
                FalconSecretKey::read_from_bytes(secret_bytes).map_err(|error| error.to_string())
            },
            || SecretBytes::new(FalconSecretKey::new().to_bytes()),
        )
    }

    async fn ecdsa_secret_key(&self) -> Result<EcdsaSecretKey> {
        let path = self.ecdsa_secret_path.as_deref().ok_or_else(|| {
            GuardianError::ConfigurationError(format!(
                "{ENV_ACK_ECDSA_SECRET_PATH} is required for the in-memory ECDSA backend; set it or use GUARDIAN_ACK_ECDSA_BACKEND=aws-kms"
            ))
        })?;
        self.parsed_secret_key(
            path,
            |secret_bytes| {
                EcdsaSecretKey::read_from_bytes(secret_bytes).map_err(|error| error.to_string())
            },
            || SecretBytes::new(EcdsaSecretKey::new().to_bytes()),
        )
    }
}

fn autogen_from_env() -> Result<bool> {
    match std::env::var(ENV_ACK_SECRET_AUTOGEN) {
        Ok(value) => parse_autogen(Some(&value)),
        Err(std::env::VarError::NotPresent) => Ok(false),
        Err(std::env::VarError::NotUnicode(_)) => Err(GuardianError::ConfigurationError(format!(
            "{ENV_ACK_SECRET_AUTOGEN} must contain valid UTF-8"
        ))),
    }
}

fn parse_autogen(raw: Option<&str>) -> Result<bool> {
    match raw.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        None | Some("") | Some("false") => Ok(false),
        Some("true") => Ok(true),
        Some(other) => Err(GuardianError::ConfigurationError(format!(
            "{ENV_ACK_SECRET_AUTOGEN} `{other}` is not supported (expected `true` or `false`)"
        ))),
    }
}

/// Fsyncs a temporary file, then hard-links it into place: `link` reports
/// `AlreadyExists` rather than overwriting, and the name appears only once the
/// bytes are durable, so a concurrent boot can neither clobber the winner's key
/// nor read a half-written one.
fn write_new_secret_file(path: &Path, secret: SecretBytes) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            GuardianError::ConfigurationError(format!(
                "Failed to create ack secret directory {}: {error}",
                parent.display()
            ))
        })?;
    }

    let temp_path = path.with_extension(format!("tmp.{}", std::process::id()));
    write_secret_bytes(&temp_path, secret)?;

    let result = match std::fs::hard_link(&temp_path, path) {
        Ok(()) => {
            tracing::warn!(
                path = %path.display(),
                "minted a new ACK secret; the on-chain ack-key commitment changes with it"
            );
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(GuardianError::ConfigurationError(format!(
            "Failed to publish ack secret file {}: {error}",
            path.display()
        ))),
    };
    std::fs::remove_file(&temp_path).ok();
    result
}

fn write_secret_bytes(path: &Path, secret: SecretBytes) -> Result<()> {
    use std::io::Write;

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }

    let mut file = options.open(path).map_err(|error| {
        GuardianError::ConfigurationError(format!(
            "Failed to create ack secret file {}: {error}",
            path.display()
        ))
    })?;
    // Encode-and-wrap in one expression so the hex copy is zeroized like the
    // bytes it came from.
    let encoded = SecretString::new(hex::encode(secret.expose_secret()));
    file.write_all(encoded.expose_secret().as_bytes())
        .and_then(|()| file.sync_all())
        .map_err(|error| {
            GuardianError::ConfigurationError(format!(
                "Failed to write ack secret file {}: {error}",
                path.display()
            ))
        })
}

fn required_path(env_var: &str) -> Result<PathBuf> {
    match std::env::var(env_var) {
        Ok(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                Err(GuardianError::ConfigurationError(format!(
                    "{env_var} must not be blank when GUARDIAN_ACK_SECRET_PROVIDER=file"
                )))
            } else {
                Ok(PathBuf::from(trimmed))
            }
        }
        Err(std::env::VarError::NotPresent) => Err(GuardianError::ConfigurationError(format!(
            "{env_var} is required when GUARDIAN_ACK_SECRET_PROVIDER=file"
        ))),
        Err(std::env::VarError::NotUnicode(_)) => Err(GuardianError::ConfigurationError(format!(
            "{env_var} must contain valid UTF-8"
        ))),
    }
}

/// Like [`required_path`] but absent is allowed (`Ok(None)`); a set-but-blank
/// value is still a misconfiguration.
fn optional_path(env_var: &str) -> Result<Option<PathBuf>> {
    match std::env::var(env_var) {
        Ok(value) => {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                Err(GuardianError::ConfigurationError(format!(
                    "{env_var} must not be blank when set"
                )))
            } else {
                Ok(Some(PathBuf::from(trimmed)))
            }
        }
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(GuardianError::ConfigurationError(format!(
            "{env_var} must contain valid UTF-8"
        ))),
    }
}

/// Reject a secret file that any principal other than the owner can touch, the
/// way OpenSSH guards private keys. These hold long-lived ACK signing keys, so a
/// group/other-accessible file fails startup rather than loading silently.
/// Unix-only; a no-op elsewhere (Windows ACLs are not modeled here).
#[cfg(unix)]
fn ensure_owner_only(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let mode = std::fs::metadata(path)
        .map_err(|error| {
            GuardianError::ConfigurationError(format!(
                "Failed to inspect ack secret file {}: {error}",
                path.display()
            ))
        })?
        .permissions()
        .mode();

    if mode & 0o077 != 0 {
        return Err(GuardianError::ConfigurationError(format!(
            "Ack secret file {} must not be accessible by group or others (set its permissions to 0600)",
            path.display()
        )));
    }

    Ok(())
}

#[cfg(not(unix))]
fn ensure_owner_only(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(all(test, not(any(feature = "integration", feature = "e2e"))))]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "guardian_file_provider_{tag}_{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write a secret file with owner-only (`0600`) permissions so it passes
    /// [`ensure_owner_only`].
    fn write_secret(path: &Path, contents: impl AsRef<[u8]>) {
        std::fs::write(path, contents).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    fn write_hex(path: &Path, bytes: &[u8]) {
        write_secret(path, hex::encode(bytes));
    }

    #[tokio::test]
    async fn reads_falcon_and_ecdsa_secrets_from_files() {
        let dir = temp_dir("read");
        let falcon = FalconSecretKey::new();
        let ecdsa = EcdsaSecretKey::new();
        let falcon_path = dir.join("falcon");
        let ecdsa_path = dir.join("ecdsa");
        write_hex(&falcon_path, &falcon.to_bytes());
        write_hex(&ecdsa_path, &ecdsa.to_bytes());
        let provider = FileSecretProvider {
            falcon_secret_path: falcon_path,
            ecdsa_secret_path: Some(ecdsa_path),
            autogen: false,
        };

        assert_eq!(
            provider.falcon_secret_key().await.unwrap().to_bytes(),
            falcon.to_bytes()
        );
        assert_eq!(
            provider.ecdsa_secret_key().await.unwrap().to_bytes(),
            ecdsa.to_bytes()
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn same_file_yields_same_identity_across_reads() {
        let dir = temp_dir("stable");
        let falcon = FalconSecretKey::new();
        let falcon_path = dir.join("falcon");
        write_hex(&falcon_path, &falcon.to_bytes());
        let provider = FileSecretProvider {
            falcon_secret_path: falcon_path,
            ecdsa_secret_path: None,
            autogen: false,
        };

        let first = provider.falcon_secret_key().await.unwrap();
        let second = provider.falcon_secret_key().await.unwrap();
        assert_eq!(
            first.public_key().to_commitment(),
            second.public_key().to_commitment()
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn tolerates_surrounding_whitespace_in_file() {
        let dir = temp_dir("trim");
        let ecdsa = EcdsaSecretKey::new();
        let ecdsa_path = dir.join("ecdsa");
        write_secret(
            &ecdsa_path,
            format!("  {}\n", hex::encode(ecdsa.to_bytes())),
        );
        let provider = FileSecretProvider {
            falcon_secret_path: dir.join("falcon"),
            ecdsa_secret_path: Some(ecdsa_path),
            autogen: false,
        };

        assert_eq!(
            provider.ecdsa_secret_key().await.unwrap().to_bytes(),
            ecdsa.to_bytes()
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn missing_file_is_configuration_error() {
        let provider = FileSecretProvider {
            falcon_secret_path: PathBuf::from("/nonexistent/guardian/ack-falcon"),
            ecdsa_secret_path: Some(PathBuf::from("/nonexistent/guardian/ack-ecdsa")),
            autogen: false,
        };
        assert!(matches!(
            provider.falcon_secret_key().await,
            Err(GuardianError::ConfigurationError(_))
        ));
    }

    #[tokio::test]
    async fn invalid_hex_is_configuration_error() {
        let dir = temp_dir("badhex");
        let falcon_path = dir.join("falcon");
        write_secret(&falcon_path, "nothex!!");
        let provider = FileSecretProvider {
            falcon_secret_path: falcon_path,
            ecdsa_secret_path: None,
            autogen: false,
        };

        let err = provider.falcon_secret_key().await.unwrap_err();
        assert!(
            matches!(err, GuardianError::ConfigurationError(message) if message.contains("hex"))
        );
        std::fs::remove_dir_all(dir).ok();
    }

    // The ECDSA file is only consulted by the in-memory backend; an aws-kms
    // backend never calls `ecdsa_secret_key`, so an unset path must not block
    // loading the Falcon key, yet must surface a clear error if consulted.
    #[tokio::test]
    async fn unset_ecdsa_path_loads_falcon_but_errors_only_when_ecdsa_consulted() {
        let dir = temp_dir("ecdsa_opt");
        let falcon = FalconSecretKey::new();
        let falcon_path = dir.join("falcon");
        write_hex(&falcon_path, &falcon.to_bytes());
        let provider = FileSecretProvider {
            falcon_secret_path: falcon_path,
            ecdsa_secret_path: None,
            autogen: false,
        };

        assert_eq!(
            provider.falcon_secret_key().await.unwrap().to_bytes(),
            falcon.to_bytes()
        );
        let err = provider.ecdsa_secret_key().await.unwrap_err();
        assert!(matches!(
            err,
            GuardianError::ConfigurationError(message)
                if message.contains(ENV_ACK_ECDSA_SECRET_PATH)
        ));
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_group_or_world_accessible_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("perms");
        let falcon = FalconSecretKey::new();
        let falcon_path = dir.join("falcon");
        std::fs::write(&falcon_path, hex::encode(falcon.to_bytes())).unwrap();
        std::fs::set_permissions(&falcon_path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let provider = FileSecretProvider {
            falcon_secret_path: falcon_path,
            ecdsa_secret_path: None,
            autogen: false,
        };

        let err = provider.falcon_secret_key().await.unwrap_err();
        assert!(
            matches!(err, GuardianError::ConfigurationError(message) if message.contains("0600"))
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn missing_file_without_autogen_is_an_error() {
        let dir = temp_dir("no-autogen");
        let provider = FileSecretProvider {
            falcon_secret_path: dir.join("falcon"),
            ecdsa_secret_path: None,
            autogen: false,
        };

        let error = provider.falcon_secret_key().await.unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Failed to inspect ack secret file"),
            "a missing file must stay an error when autogen is off, got: {error}"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn autogen_mints_and_persists_a_falcon_key() {
        let dir = temp_dir("autogen-falcon");
        let path = dir.join("nested").join("falcon");
        let provider = FileSecretProvider {
            falcon_secret_path: path.clone(),
            ecdsa_secret_path: None,
            autogen: true,
        };

        let minted = provider.falcon_secret_key().await.unwrap();
        assert!(path.exists(), "autogen must create the key file");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "generated key file must be 0600");
        }

        // A second provider over the same path must load the stored key, not mint a
        // new one - the on-chain ack commitment depends on that.
        let reopened = FileSecretProvider {
            falcon_secret_path: path,
            ecdsa_secret_path: None,
            autogen: true,
        };
        assert_eq!(
            reopened
                .falcon_secret_key()
                .await
                .unwrap()
                .public_key()
                .to_commitment(),
            minted.public_key().to_commitment()
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn autogen_mints_and_persists_an_ecdsa_key() {
        let dir = temp_dir("autogen-ecdsa");
        let path = dir.join("ecdsa");
        let provider = FileSecretProvider {
            falcon_secret_path: dir.join("falcon"),
            ecdsa_secret_path: Some(path.clone()),
            autogen: true,
        };

        let minted = provider.ecdsa_secret_key().await.unwrap();
        let reopened = FileSecretProvider {
            falcon_secret_path: dir.join("falcon"),
            ecdsa_secret_path: Some(path),
            autogen: true,
        };
        assert_eq!(
            reopened.ecdsa_secret_key().await.unwrap().to_bytes(),
            minted.to_bytes()
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn autogen_does_not_overwrite_an_existing_key() {
        let dir = temp_dir("autogen-existing");
        let path = dir.join("falcon");
        let existing = FalconSecretKey::new();
        write_hex(&path, &existing.to_bytes());
        let provider = FileSecretProvider {
            falcon_secret_path: path,
            ecdsa_secret_path: None,
            autogen: true,
        };

        assert_eq!(
            provider.falcon_secret_key().await.unwrap().to_bytes(),
            existing.to_bytes()
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn autogen_still_requires_the_ecdsa_path() {
        let dir = temp_dir("autogen-no-ecdsa-path");
        let provider = FileSecretProvider {
            falcon_secret_path: dir.join("falcon"),
            ecdsa_secret_path: None,
            autogen: true,
        };

        let error = provider.ecdsa_secret_key().await.unwrap_err();
        assert!(
            error.to_string().contains(ENV_ACK_ECDSA_SECRET_PATH),
            "an unset ecdsa path is a configuration error, not something to mint, got: {error}"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn publishing_over_an_existing_file_keeps_the_first_key() {
        let dir = temp_dir("publish-race");
        let path = dir.join("falcon");
        let first = FalconSecretKey::new();
        let second = FalconSecretKey::new();

        write_new_secret_file(&path, SecretBytes::new(first.to_bytes())).unwrap();
        write_new_secret_file(&path, SecretBytes::new(second.to_bytes())).unwrap();

        let provider = FileSecretProvider {
            falcon_secret_path: path.clone(),
            ecdsa_secret_path: None,
            autogen: false,
        };
        assert_eq!(
            provider.falcon_secret_key().await.unwrap().to_bytes(),
            first.to_bytes(),
            "the second write must not replace the published key"
        );
        assert!(
            std::fs::read_dir(&dir).unwrap().all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("tmp")),
            "the temporary file must not be left behind"
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn autogen_flag_rejects_junk() {
        assert!(!parse_autogen(None).unwrap());
        assert!(!parse_autogen(Some("false")).unwrap());
        assert!(parse_autogen(Some("true")).unwrap());
        assert!(parse_autogen(Some(" TRUE ")).unwrap());
        assert!(parse_autogen(Some("yes")).is_err());
    }
}
