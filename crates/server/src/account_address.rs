//! Account ids as people read and type them: bech32 addresses of the server's network.

use std::sync::OnceLock;

use miden_protocol::account::AccountId;
use miden_protocol::address::NetworkId;

use crate::error::GuardianError;
use crate::metadata::MidenNetworkType;
use crate::metadata::network::is_evm_account_id;
use crate::network::NetworkType;

static LOG_NETWORK: OnceLock<NetworkId> = OnceLock::new();

/// Sets the network log lines render account ids for.
pub fn init_log_network(network_type: NetworkType) {
    LOG_NETWORK
        .set(network_id(network_type))
        .expect("the log network is set once");
}

fn network_id(network_type: NetworkType) -> NetworkId {
    MidenNetworkType::from(network_type).to_miden_network_id()
}

/// Stored form of a 0x hex or this network's bech32 account id; EVM ids pass through.
pub fn parse_input(network_type: NetworkType, raw: String) -> Result<String, GuardianError> {
    if is_evm_account_id(&raw) {
        return Ok(raw);
    }
    let (account_id, network) = AccountId::parse(&raw).map_err(|error| {
        GuardianError::InvalidInput(format!(
            "invalid account id '{raw}': {error}; expected a bech32 address or 0x hex"
        ))
    })?;
    let expected = network_id(network_type);
    match network {
        Some(network) if network != expected => Err(GuardianError::InvalidInput(format!(
            "account address '{raw}' is for network '{network}', this server runs '{expected}'"
        ))),
        _ => Ok(account_id.to_hex()),
    }
}

/// Bech32 once the log network is set; EVM ids and unset-network use show the stored id.
pub fn display(account_id: &str) -> String {
    display_for(LOG_NETWORK.get(), account_id)
}

/// A stored account id as people read it on `network_type`.
pub fn display_on(network_type: NetworkType, account_id: &str) -> String {
    display_for(Some(&network_id(network_type)), account_id)
}

fn display_for(network: Option<&NetworkId>, account_id: &str) -> String {
    match (network, AccountId::from_hex(account_id)) {
        (Some(network), Ok(id)) => id.to_bech32(network.clone()),
        _ => account_id.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use miden_protocol::testing::account_id::ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE;

    use super::*;

    fn account() -> AccountId {
        AccountId::try_from(ACCOUNT_ID_REGULAR_PRIVATE_ACCOUNT_UPDATABLE_CODE).unwrap()
    }

    #[test]
    fn bech32_of_the_server_network_becomes_the_stored_hex() {
        let address = account().to_bech32(NetworkId::Testnet);
        assert!(address.starts_with("mtst1"));
        assert_eq!(
            parse_input(NetworkType::MidenTestnet, address).unwrap(),
            account().to_hex()
        );
    }

    #[test]
    fn bech32_of_another_network_is_rejected() {
        let address = account().to_bech32(NetworkId::Devnet);
        let error = parse_input(NetworkType::MidenTestnet, address).unwrap_err();
        assert!(error.to_string().contains("this server runs 'mtst'"));
    }

    #[test]
    fn hex_and_evm_ids_pass_through() {
        let hex = account().to_hex();
        assert_eq!(
            parse_input(NetworkType::MidenTestnet, hex.clone()).unwrap(),
            hex
        );
        assert_eq!(
            parse_input(NetworkType::MidenTestnet, hex.replace("0x", "0X")).unwrap(),
            hex
        );
        let evm = "evm:1:0x0000000000000000000000000000000000000001".to_string();
        assert_eq!(
            parse_input(NetworkType::MidenTestnet, evm.clone()).unwrap(),
            evm
        );
    }

    #[test]
    fn anything_else_is_rejected() {
        for raw in ["", "not-an-address", "mtst1qqqq", "1234", "0xnope"] {
            assert!(
                matches!(
                    parse_input(NetworkType::MidenTestnet, raw.to_string()),
                    Err(GuardianError::InvalidInput(_))
                ),
                "{raw:?} must be rejected"
            );
        }
    }

    #[test]
    fn display_renders_miden_ids_as_bech32_of_the_log_network() {
        let hex = account().to_hex();
        assert_eq!(
            display_for(Some(&NetworkId::Testnet), &hex),
            account().to_bech32(NetworkId::Testnet)
        );
        assert_eq!(
            display_for(Some(&NetworkId::Testnet), "evm:1:0x01"),
            "evm:1:0x01"
        );
        assert_eq!(display_for(None, &hex), hex);
    }
}
