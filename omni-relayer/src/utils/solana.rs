use solana_sdk::signature::Keypair;
use solana_sdk::signer::EncodableKey;
use tracing::info;

use omni_types::ChainKind;

use crate::config;

pub fn get_keypair(file: Option<&String>, chain_kind: ChainKind) -> Keypair {
    if let Some(file) = file
        && let Ok(keypair) = Keypair::read_from_file(file)
    {
        info!("Retrieved keypair from file");
        return keypair;
    }

    info!("Retrieving SVM keypair from env");

    Keypair::from_base58_string(&config::get_private_key(chain_kind, None))
}
