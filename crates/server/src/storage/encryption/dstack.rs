//! Storage encryption key sourced from the dstack guest agent.
//!
//! Inside a dstack CVM the key is derived by the platform and bound to the
//! application's on-chain identity, so it never exists as an operator-readable
//! value the way `GUARDIAN_STORAGE_ENCRYPTION_KEY` does.

use std::collections::HashMap;

use async_trait::async_trait;
use dstack_sdk::dstack_client::DstackClient;

use crate::secret::{FixedKey, SecretBytes};
use crate::storage::encryption::key_provider::{InMemoryKeyProvider, KeyProviderError};

/// Pinned rather than auto-discovered: the SDK consults `DSTACK_SIMULATOR_ENDPOINT`
/// first, so injecting that variable would choose the storage key without
/// touching the approved compose hash.
const SOCKET_PATH: &str = "/var/run/dstack.sock";

const KEY_PURPOSE: &str = "storage-encryption";

/// Seam so derivation is testable without a live guest agent, mirroring
/// `EcdsaSignerBackend` in the ACK backend.
#[async_trait]
pub(crate) trait DstackKeyClient {
    async fn derive_key(&self, path: &str) -> Result<SecretBytes, KeyProviderError>;
}

pub(crate) struct GuestAgentClient {
    inner: DstackClient,
}

impl GuestAgentClient {
    pub(crate) fn new() -> Self {
        Self {
            inner: DstackClient::new(Some(SOCKET_PATH)),
        }
    }
}

#[async_trait]
impl DstackKeyClient for GuestAgentClient {
    async fn derive_key(&self, path: &str) -> Result<SecretBytes, KeyProviderError> {
        let response = self
            .inner
            .get_key(Some(path.to_string()), Some(KEY_PURPOSE.to_string()))
            .await
            .map_err(|e| KeyProviderError::DstackUnavailable(e.to_string()))?;
        Ok(SecretBytes::new(response.decode_key().map_err(|e| {
            KeyProviderError::DstackUnavailable(format!("malformed key hex: {e}"))
        })?))
    }
}

/// The derivation path doubles as the envelope key id, so rotating means
/// deriving at a new path rather than mutating an existing key.
pub(crate) async fn load_dstack_provider<C: DstackKeyClient + ?Sized>(
    client: &C,
    path: &str,
) -> Result<InMemoryKeyProvider, KeyProviderError> {
    let secret = client.derive_key(path).await?;
    let array: [u8; 32] = secret
        .expose_secret()
        .try_into()
        .map_err(|_| KeyProviderError::InvalidKeyLength)?;
    let kid = format!("dstack:{path}");
    let mut keys = HashMap::new();
    keys.insert(kid.clone(), FixedKey::new(array));
    InMemoryKeyProvider::new(kid, keys)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::encryption::key_provider::StorageKeyProvider;

    struct StubClient(Option<Vec<u8>>);

    #[async_trait]
    impl DstackKeyClient for StubClient {
        async fn derive_key(&self, _path: &str) -> Result<SecretBytes, KeyProviderError> {
            match &self.0 {
                Some(bytes) => Ok(SecretBytes::new(bytes.clone())),
                None => Err(KeyProviderError::DstackUnavailable("stub".to_string())),
            }
        }
    }

    #[tokio::test]
    async fn derives_provider_keyed_by_path() {
        let client = StubClient(Some(vec![7u8; 32]));
        let provider = load_dstack_provider(&client, "guardian/storage")
            .await
            .unwrap();
        assert_eq!(provider.active_key_id(), "dstack:guardian/storage");
        assert_eq!(
            provider
                .key("dstack:guardian/storage")
                .unwrap()
                .expose_secret(),
            &[7u8; 32]
        );
    }

    #[tokio::test]
    async fn rejects_wrong_key_length() {
        let client = StubClient(Some(vec![7u8; 16]));
        let err = load_dstack_provider(&client, "guardian/storage")
            .await
            .unwrap_err();
        assert!(matches!(err, KeyProviderError::InvalidKeyLength));
    }

    #[tokio::test]
    async fn propagates_agent_failure() {
        let client = StubClient(None);
        let err = load_dstack_provider(&client, "guardian/storage")
            .await
            .unwrap_err();
        assert!(matches!(err, KeyProviderError::DstackUnavailable(_)));
    }

    #[tokio::test]
    async fn unknown_kid_is_error() {
        let client = StubClient(Some(vec![7u8; 32]));
        let provider = load_dstack_provider(&client, "guardian/storage")
            .await
            .unwrap();
        assert!(provider.key("dstack:other").is_err());
    }
}
