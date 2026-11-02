use miden_multisig_client::{Asset, MultisigAccount};
use miden_protocol::account::AccountId;
use miden_protocol::address::NetworkId;

/// A hex account id the guardian returned, as the bech32 address people read.
pub fn account_address(account_id_hex: &str, network_id: NetworkId) -> Result<String, String> {
    AccountId::from_hex(account_id_hex)
        .map(|id| id.to_bech32(network_id))
        .map_err(|e| format!("invalid account id '{account_id_hex}': {e}"))
}

pub fn shorten_hex(hex: &str) -> String {
    if hex.len() <= 12 {
        return hex.to_string();
    }

    let prefix = &hex[..6];
    let suffix = &hex[hex.len() - 4..];
    format!("{}...{}", prefix, suffix)
}

pub fn shorten_hex_32(hex: &str) -> String {
    let stripped = hex
        .strip_prefix("0x")
        .or_else(|| hex.strip_prefix("0X"))
        .unwrap_or(hex);
    let truncated_len = stripped.len().min(32);
    format!("0x{}", &stripped[..truncated_len])
}

pub fn print_banner() {
    println!("\n╔═══════════════════════════╗");
    println!("║      Multisig Demo        ║");
    println!("╚═══════════════════════════╝\n");
}

pub fn print_section(title: &str) {
    println!("\n━━━ {} ━━━", title);
}

pub fn print_success(message: &str) {
    println!("✓ {}", message);
}

pub fn print_error(message: &str) {
    println!("✗ Error: {}", message);
}

pub fn print_info(message: &str) {
    println!("ℹ {}", message);
}

pub fn print_warning(message: &str) {
    println!("⚠ {}", message);
}

pub fn print_account_info(account: &MultisigAccount, network_id: NetworkId) {
    print_section("Account Information");
    println!(
        "  Account ID:     {}",
        account.inner().id().to_bech32(network_id)
    );
    println!(
        "  Account Type:   {:?}",
        account.inner().id().account_type()
    );
    println!("  Nonce:          {}", account.nonce());
}

pub fn print_storage_overview(
    account: &MultisigAccount,
    ecdsa_mode: bool,
    guardian_endpoint: &str,
) {
    print_section("Storage Overview");

    match account.threshold() {
        Ok(threshold) => {
            let num_cosigners = account.cosigner_commitments().len();
            println!("  Multisig Config: {}-of-{}", threshold, num_cosigners);
        }
        Err(_) => println!("  Multisig Config: Not available"),
    }

    println!("  Cosigner Commitments:");
    for (i, commitment) in account.cosigner_commitments_hex().iter().enumerate() {
        let display = if ecdsa_mode {
            shorten_hex_32(commitment)
        } else {
            shorten_hex(commitment)
        };
        println!("    [{}] {}", i, display);
    }

    match account.procedure_threshold_overrides() {
        Ok(overrides) if !overrides.is_empty() => {
            println!("  Procedure Threshold Overrides:");
            for (procedure, threshold) in overrides {
                println!("    - {} => {}", procedure, threshold);
            }
        }
        _ => {}
    }

    println!("  GUARDIAN Endpoint: {}", guardian_endpoint);
}

pub fn print_vault(account: &MultisigAccount, network_id: NetworkId) {
    print_section("Vault (Account Balance)");

    let vault = account.inner().vault();
    let assets: Vec<Asset> = vault.assets().collect();

    if assets.is_empty() {
        println!("  (empty)");
        print_info("Tip: Consume notes to add assets to your vault before sending transfers.");
        return;
    }

    for (i, asset) in assets.iter().enumerate() {
        match asset.as_fungible() {
            Some(fungible) => {
                println!(
                    "  [{}] {} tokens (faucet: {})",
                    i + 1,
                    fungible.amount(),
                    fungible.faucet_id().to_bech32(network_id.clone())
                );
            }
            None => {
                println!(
                    "  [{}] NFT (faucet: {})",
                    i + 1,
                    asset.faucet_id().to_bech32(network_id.clone())
                );
            }
        }
    }
}

pub fn print_full_hex(label: &str, hex: &str) {
    println!("{}: {}", label, hex);
}

pub fn print_menu_header() {
    println!("\n┌─────────────────────────────────────────────┐");
    println!("│ Main Menu                                   │");
    println!("└─────────────────────────────────────────────┘");
}

pub fn print_menu_option(key: &str, description: &str, enabled: bool) {
    if enabled {
        println!("  [{}] {}", key, description);
    } else {
        println!("  [{}] {} (disabled)", key, description);
    }
}

pub fn print_waiting(message: &str) {
    println!("\n⏳ {}...", message);
}
