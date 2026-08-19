use std::collections::BTreeSet;
use std::sync::OnceLock;

use alloy::{
    primitives::Address,
    signers::{k256::ecdsa::SigningKey, local::LocalSigner},
};
use near_primitives::types::AccountId;
use omni_types::{ChainKind, OmniAddress};
use rust_decimal::Decimal;
use serde::Deserialize;

pub enum NearSignerType {
    Omni,
    Fast,
}

pub fn get_private_key(chain_kind: ChainKind, near_signer_type: Option<NearSignerType>) -> String {
    let env_var = match chain_kind {
        ChainKind::Near => match near_signer_type.unwrap() {
            NearSignerType::Omni => "NEAR_OMNI_PRIVATE_KEY",
            NearSignerType::Fast => "NEAR_FAST_PRIVATE_KEY",
        },
        ChainKind::Eth => "ETH_PRIVATE_KEY",
        ChainKind::Base => "BASE_PRIVATE_KEY",
        ChainKind::Arb => "ARB_PRIVATE_KEY",
        ChainKind::Bnb => "BNB_PRIVATE_KEY",
        ChainKind::Pol => "POL_PRIVATE_KEY",
        ChainKind::HyperEvm => "HLEVM_PRIVATE_KEY",
        ChainKind::Abs => "ABS_PRIVATE_KEY",
        ChainKind::Sol => "SOLANA_PRIVATE_KEY",
        ChainKind::Fogo => "FOGO_PRIVATE_KEY",
        ChainKind::Strk => "STARKNET_PRIVATE_KEY",
        ChainKind::Aptos => "APTOS_PRIVATE_KEY",
        ChainKind::Btc | ChainKind::Zcash => unreachable!("No private key for UTXO chains"),
    };

    std::env::var(env_var).unwrap_or_else(|_| panic!("Failed to get `{env_var}` env variable"))
}

pub fn get_relayer_starknet_address() -> String {
    std::env::var("STARKNET_ACCOUNT_ADDRESS")
        .unwrap_or_else(|_| panic!("Failed to get `STARKNET_ACCOUNT_ADDRESS` env variable"))
}

pub fn get_relayer_aptos_account_address() -> String {
    std::env::var("APTOS_ACCOUNT_ADDRESS")
        .unwrap_or_else(|_| panic!("Failed to get `APTOS_ACCOUNT_ADDRESS` env variable"))
}

pub fn get_relayer_evm_address(chain_kind: ChainKind) -> Address {
    let decoded_private_key =
        hex::decode(get_private_key(chain_kind, None)).expect("Failed to decode EVM private key");

    let secret_key = SigningKey::from_slice(&decoded_private_key)
        .expect("Failed to create a `SecretKey` from the provided private key");

    let signer = LocalSigner::from_signing_key(secret_key);

    signer.address()
}

fn replace_mongodb_credentials<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let uri = Option::<String>::deserialize(deserializer)?;

    if let Some(uri) = uri {
        let username = std::env::var("MONGODB_USERNAME").map_err(serde::de::Error::custom)?;
        let password = std::env::var("MONGODB_PASSWORD").map_err(serde::de::Error::custom)?;
        let host = std::env::var("MONGODB_HOST").map_err(serde::de::Error::custom)?;

        Ok(Some(
            uri.replace("MONGODB_USERNAME", &username)
                .replace("MONGODB_PASSWORD", &password)
                .replace("MONGODB_HOST", &host),
        ))
    } else {
        Ok(None)
    }
}

fn replace_rpc_api_key<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let mut url = String::deserialize(deserializer)?;

    for key in ["INFURA_API_KEY", "TATUM_API_KEY", "FASTNEAR_API_KEY"] {
        if let Ok(val) = std::env::var(key) {
            url = url.replace(key, &val);
        }
    }

    Ok(url)
}

fn validate_fee_discount<'de, D>(deserializer: D) -> Result<u8, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let fee_discount = u8::deserialize(deserializer)?;

    if fee_discount > 100 {
        return Err(serde::de::Error::custom(
            "Fee discount should be less than 100",
        ));
    }

    Ok(fee_discount)
}

fn validate_allowlist<'de, D>(deserializer: D) -> Result<Vec<AllowlistRule>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let allowlist = Vec::<AllowlistRule>::deserialize(deserializer)?;

    for rule in &allowlist {
        rule.validate().map_err(serde::de::Error::custom)?;
    }

    Ok(allowlist)
}

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub redis: Redis,
    pub nats: Option<Nats>,
    pub bridge_indexer: BridgeIndexer,
    pub near: Near,
    pub eth: Option<Evm>,
    pub base: Option<Evm>,
    pub arb: Option<Evm>,
    pub bnb: Option<Evm>,
    pub pol: Option<Evm>,
    pub hyperevm: Option<Evm>,
    pub hypercore: Option<HyperCore>,
    pub abs: Option<Evm>,
    pub solana: Option<Solana>,
    pub fogo: Option<Solana>,
    pub starknet: Option<Starknet>,
    pub aptos: Option<Aptos>,
    pub btc: Option<Utxo>,
    pub zcash: Option<Utxo>,
    pub orchard: Option<Orchard>,
    pub wormhole: Wormhole,
    #[serde(default)]
    pub kyt: Kyt,
    #[serde(default, deserialize_with = "validate_allowlist")]
    pub allowlist: Vec<AllowlistRule>,
}

/// Restricts transfers by sender, recipient, or exact sender->recipient pair.
/// Targets one destination chain: the recipient's chain when a recipient is
/// present, else `destination_chain`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AllowlistRule {
    pub sender: Option<OmniAddress>,
    pub recipient: Option<OmniAddress>,
    pub destination_chain: Option<ChainKind>,
}

impl AllowlistRule {
    fn validate(&self) -> Result<(), String> {
        if self.sender.is_none() && self.recipient.is_none() {
            return Err(
                "allowlist rule must specify at least one of `sender` or `recipient`".to_string(),
            );
        }

        match (&self.recipient, self.destination_chain) {
            (None, None) => Err(
                "allowlist rule with no `recipient` must specify `destination_chain`".to_string(),
            ),
            (Some(recipient), Some(destination_chain))
                if recipient.get_chain() != destination_chain =>
            {
                Err(format!(
                    "allowlist rule `destination_chain` ({destination_chain:?}) does not match the `recipient`'s chain ({:?})",
                    recipient.get_chain()
                ))
            }
            _ => Ok(()),
        }
    }

    fn target_chain(&self) -> Option<ChainKind> {
        self.recipient
            .as_ref()
            .map(OmniAddress::get_chain)
            .or(self.destination_chain)
    }

    fn matches(&self, sender: &OmniAddress, recipient: &OmniAddress) -> bool {
        self.sender.as_ref().is_none_or(|s| s == sender)
            && self.recipient.as_ref().is_none_or(|r| r == recipient)
    }
}

fn transfer_allowed(
    allowlist: &[AllowlistRule],
    sender: &OmniAddress,
    recipient: &OmniAddress,
) -> bool {
    let destination_chain = recipient.get_chain();
    let mut destination_restricted = false;

    for rule in allowlist {
        if rule.target_chain() == Some(destination_chain) {
            destination_restricted = true;
            if rule.matches(sender, recipient) {
                return true;
            }
        }
    }

    !destination_restricted
}

/// For paths that know the sender but not the recipient (NEAR->UTXO signing).
/// Rules are matched on the sender field only, so a transfer the init-path
/// check approved is never dropped here.
fn sender_possibly_allowed(
    allowlist: &[AllowlistRule],
    sender: &OmniAddress,
    destination_chain: ChainKind,
) -> bool {
    let mut destination_restricted = false;

    for rule in allowlist {
        if rule.target_chain() == Some(destination_chain) {
            destination_restricted = true;
            if rule.sender.as_ref().is_none_or(|s| s == sender) {
                return true;
            }
        }
    }

    !destination_restricted
}

/// For paths that know the recipient and the sender's chain but not the
/// sender itself (UTXO->NEAR deposits). A rule whose sender lives on another
/// chain can never match, so it doesn't count.
fn recipient_possibly_allowed(
    allowlist: &[AllowlistRule],
    recipient: &OmniAddress,
    sender_chain: ChainKind,
) -> bool {
    let destination_chain = recipient.get_chain();
    let mut destination_restricted = false;
    for rule in allowlist {
        if rule.target_chain() == Some(destination_chain) {
            destination_restricted = true;
            if rule.recipient.as_ref().is_none_or(|r| r == recipient)
                && rule
                    .sender
                    .as_ref()
                    .is_none_or(|s| s.get_chain() == sender_chain)
            {
                return true;
            }
        }
    }
    !destination_restricted
}

/// Returns `true` if at least one rule targets `destination_chain`.
fn destination_is_restricted(allowlist: &[AllowlistRule], destination_chain: ChainKind) -> bool {
    allowlist
        .iter()
        .any(|rule| rule.target_chain() == Some(destination_chain))
}

impl Config {
    pub const fn is_bridge_indexer_enabled(&self) -> bool {
        self.bridge_indexer.mongodb_uri.is_some() && self.bridge_indexer.db_name.is_some()
    }

    pub const fn is_nats_enabled(&self) -> bool {
        self.nats.is_some()
    }

    pub const fn is_bridge_api_enabled(&self) -> bool {
        self.bridge_indexer.api_url.is_some()
    }

    pub fn is_fast_relayer_enabled(&self) -> bool {
        self.near.fast_relayer_enabled
    }

    pub fn is_signing_utxo_transaction_enabled(&self, chain: ChainKind) -> bool {
        let config = match chain {
            ChainKind::Btc => self.btc.as_ref(),
            ChainKind::Zcash => self.zcash.as_ref(),
            ChainKind::Near
            | ChainKind::Eth
            | ChainKind::Base
            | ChainKind::Arb
            | ChainKind::Bnb
            | ChainKind::Pol
            | ChainKind::HyperEvm
            | ChainKind::Abs
            | ChainKind::Sol
            | ChainKind::Fogo
            | ChainKind::Strk
            | ChainKind::Aptos => {
                panic!("Signing utxo transaction is not applicable for {chain:?}")
            }
        };
        config.is_some_and(|utxo| utxo.signing_enabled)
    }

    pub fn utxo_sign_delay_secs(&self, chain: ChainKind) -> u64 {
        let config = match chain {
            ChainKind::Btc => self.btc.as_ref(),
            ChainKind::Zcash => self.zcash.as_ref(),
            ChainKind::Near
            | ChainKind::Eth
            | ChainKind::Base
            | ChainKind::Arb
            | ChainKind::Bnb
            | ChainKind::Pol
            | ChainKind::HyperEvm
            | ChainKind::Abs
            | ChainKind::Sol
            | ChainKind::Fogo
            | ChainKind::Strk
            | ChainKind::Aptos => {
                panic!("Sign delay is not applicable for {chain:?}")
            }
        };
        config.map_or(0, |utxo| utxo.sign_delay_secs)
    }

    pub fn active_utxo_management(&self, chain: ChainKind) -> Option<&ActiveUtxoManagement> {
        let config = match chain {
            ChainKind::Btc => self.btc.as_ref(),
            ChainKind::Zcash => self.zcash.as_ref(),
            ChainKind::Near
            | ChainKind::Eth
            | ChainKind::Base
            | ChainKind::Arb
            | ChainKind::Bnb
            | ChainKind::Pol
            | ChainKind::HyperEvm
            | ChainKind::Abs
            | ChainKind::Sol
            | ChainKind::Fogo
            | ChainKind::Strk
            | ChainKind::Aptos => {
                panic!("Active UTXO management is not applicable for {chain:?}");
            }
        };

        config.and_then(|utxo| utxo.active_utxo_management.as_ref())
    }

    pub fn is_verifying_utxo_withdraw_enabled(&self, chain: ChainKind) -> bool {
        let config = match chain {
            ChainKind::Btc => self.btc.as_ref(),
            ChainKind::Zcash => self.zcash.as_ref(),
            ChainKind::Near
            | ChainKind::Eth
            | ChainKind::Base
            | ChainKind::Arb
            | ChainKind::Bnb
            | ChainKind::Pol
            | ChainKind::HyperEvm
            | ChainKind::Abs
            | ChainKind::Sol
            | ChainKind::Fogo
            | ChainKind::Strk
            | ChainKind::Aptos => {
                panic!("Verifying withdraw is not applicable for {chain:?}")
            }
        };
        config.is_some_and(|utxo| utxo.verifying_withdraw_enabled)
    }

    pub fn is_kyt_enabled() -> bool {
        static ENABLED: OnceLock<bool> = OnceLock::new();
        *ENABLED.get_or_init(|| {
            std::env::var("KYT_API_URL").is_ok() && std::env::var("KYT_API_KEY").is_ok()
        })
    }

    pub fn is_fee_bumping_enabled(&self, chain_kind: ChainKind) -> bool {
        match chain_kind {
            ChainKind::Eth => self
                .eth
                .as_ref()
                .and_then(|evm| evm.fee_bumping.as_ref())
                .is_some(),
            _ => false,
        }
    }

    pub fn is_transfer_allowed(&self, sender: &OmniAddress, recipient: &OmniAddress) -> bool {
        transfer_allowed(&self.allowlist, sender, recipient)
    }

    /// See [`sender_possibly_allowed`].
    pub fn is_sender_possibly_allowed(
        &self,
        sender: &OmniAddress,
        destination_chain: ChainKind,
    ) -> bool {
        sender_possibly_allowed(&self.allowlist, sender, destination_chain)
    }

    /// See [`recipient_possibly_allowed`].
    pub fn is_recipient_possibly_allowed(
        &self,
        recipient: &OmniAddress,
        sender_chain: ChainKind,
    ) -> bool {
        recipient_possibly_allowed(&self.allowlist, recipient, sender_chain)
    }

    pub fn is_destination_restricted(&self, destination_chain: ChainKind) -> bool {
        destination_is_restricted(&self.allowlist, destination_chain)
    }

    pub fn restricted_destination_chains(&self) -> BTreeSet<ChainKind> {
        self.allowlist
            .iter()
            .filter_map(AllowlistRule::target_chain)
            .collect()
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Redis {
    pub url: String,

    pub sleep_time_after_events_process_secs: u64,
    pub query_retry_attempts: u64,
    pub query_retry_sleep_secs: u64,
    pub query_timeout_secs: u64,
    #[allow(dead_code)]
    pub fee_retry_base_secs: Decimal,
    #[allow(dead_code)]
    pub fee_retry_max_sleep_secs: i64,
    #[allow(dead_code)]
    pub keep_transfers_for_secs: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Nats {
    pub url: String,
    pub relayer_subject: String,
    pub omni_consumer: OmniConsumer,
    pub relayer_consumer: RelayerConsumer,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OmniConsumer {
    pub name: String,
    pub stream: String,
    pub subject: String,
    pub max_deliver: i64,
    #[serde(default)]
    pub backoff_secs: Vec<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RelayerConsumer {
    pub name: String,
    pub stream: String,
    pub subject: String,
    pub max_deliver: i64,
    pub ack_wait: u64,
    pub max_backoff_hours: u64,
    pub max_message_age_hours: u64,
    #[serde(default = "default_worker_count")]
    pub worker_count: usize,
    #[serde(default = "default_max_ack_pending")]
    pub max_ack_pending: i64,
}

fn default_worker_count() -> usize {
    1
}

const fn default_max_ack_pending() -> i64 {
    -1
}

#[derive(Debug, Clone, Deserialize)]
pub struct BridgeIndexer {
    pub api_url: Option<String>,

    #[serde(default, deserialize_with = "replace_mongodb_credentials")]
    pub mongodb_uri: Option<String>,
    pub db_name: Option<String>,

    #[serde(default, deserialize_with = "validate_fee_discount")]
    pub fee_discount: u8,
    #[serde(default)]
    pub whitelisted_tokens: Vec<OmniAddress>,
}

impl BridgeIndexer {
    pub fn is_whitelist_active(&self) -> bool {
        !self.whitelisted_tokens.is_empty()
    }

    pub fn is_token_whitelisted(&self, token: &OmniAddress) -> bool {
        self.whitelisted_tokens.contains(token)
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    Testnet,
    Mainnet,
}

impl std::fmt::Display for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Network::Testnet => write!(f, "testnet"),
            Network::Mainnet => write!(f, "mainnet"),
        }
    }
}

impl From<Network> for utxo_utils::address::Network {
    fn from(value: Network) -> Self {
        match value {
            Network::Testnet => utxo_utils::address::Network::Testnet,
            Network::Mainnet => utxo_utils::address::Network::Mainnet,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Near {
    pub network: Network,
    #[serde(deserialize_with = "replace_rpc_api_key")]
    pub rpc_url: String,
    pub omni_bridge_id: AccountId,
    pub mpc_omni_prover_id: Option<AccountId>,
    pub btc_connector: Option<AccountId>,
    pub btc: Option<AccountId>,
    pub zcash_connector: Option<AccountId>,
    pub zcash: Option<AccountId>,
    pub omni_credentials_path: Option<String>,
    pub fast_credentials_path: Option<String>,
    pub sign_without_checking_fee: Option<Vec<OmniAddress>>,
    #[serde(default)]
    pub fast_relayer_enabled: bool,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Kyt {
    /// Delay in seconds before relaying a transfer (anti-laundering).
    /// Gives KYT providers time to index fresh accounts across all origin chains.
    #[serde(default)]
    pub delay_secs: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Evm {
    #[serde(deserialize_with = "replace_rpc_api_key")]
    pub rpc_http_url: String,
    #[serde(deserialize_with = "replace_rpc_api_key")]
    pub rpc_ws_url: String,
    pub omni_bridge_address: Address,
    pub wormhole_address: Option<Address>,
    pub light_client: Option<AccountId>,
    pub block_processing_batch_size: u64,
    pub expected_finalization_time: i64,
    #[serde(default = "u64::max_value")]
    pub safe_confirmations: u64,
    #[serde(default)]
    pub error_selectors_to_remove: Vec<String>,
    pub fee_bumping: Option<FeeBumping>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct HyperCore {
    pub api_url: String,
    pub signature_chain_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FeeBumping {
    pub min_pending_time_seconds: i64,
    pub min_since_last_bump_seconds: i64,
    pub check_interval_seconds: u64,
    pub min_fee_increase_percent: u64,
    pub max_fee_in_wei: u128,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Solana {
    #[serde(deserialize_with = "replace_rpc_api_key")]
    pub rpc_http_url: String,
    #[serde(deserialize_with = "replace_rpc_api_key")]
    pub rpc_ws_url: String,
    pub expected_finalization_time: i64,
    pub program_id: String,
    pub wormhole_id: String,
    pub wormhole_post_message_shim_id: String,
    pub wormhole_post_message_shim_event_authority: String,
    pub deploy_token_emitter_index: usize,
    pub deploy_token_discriminator: Vec<u8>,
    pub init_transfer_sender_index: usize,
    pub init_transfer_token_index: usize,
    pub init_transfer_emitter_index: usize,
    pub init_transfer_sol_sender_index: usize,
    pub init_transfer_sol_emitter_index: usize,
    pub init_transfer_discriminator: Vec<u8>,
    pub init_transfer_sol_discriminator: Vec<u8>,
    pub finalize_transfer_emitter_index: usize,
    pub finalize_transfer_sol_emitter_index: usize,
    pub finalize_transfer_discriminator: Vec<u8>,
    pub finalize_transfer_sol_discriminator: Vec<u8>,
    pub credentials_path: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Starknet {
    #[serde(deserialize_with = "replace_rpc_api_key")]
    pub rpc_http_url: String,
    pub chain_id: String,
    pub omni_bridge_address: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Aptos {
    #[serde(deserialize_with = "replace_rpc_api_key")]
    pub rpc_http_url: String,
    pub omni_bridge_address: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Utxo {
    #[serde(deserialize_with = "replace_rpc_api_key")]
    pub rpc_http_url: String,
    pub light_client: AccountId,
    pub signing_enabled: bool,
    pub verifying_withdraw_enabled: bool,
    #[serde(default = "default_lc_polling_interval_secs")]
    pub lc_polling_interval_secs: u64,
    #[serde(default)]
    pub sign_delay_secs: u64,
    #[serde(default)]
    pub active_utxo_management: Option<ActiveUtxoManagement>,
    /// Percent of the user's `max_gas_fee` actually offered to the UTXO
    /// selector. The remainder is reserved for fee bumping. 0..=100.
    #[serde(default = "default_max_gas_fee_percent")]
    pub max_gas_fee_percent: u8,
    /// Extra headroom (sats) above `min_change_amount` to leave in the
    /// change output, so a later RBF can bump the fee without driving
    /// change below the contract minimum. Only honoured on the
    /// anchor-fill path (`max_gas_fee` set).
    #[serde(default = "default_change_reserve")]
    pub change_reserve: u128,
}

const fn default_lc_polling_interval_secs() -> u64 {
    30
}

const fn default_max_gas_fee_percent() -> u8 {
    75
}

const fn default_change_reserve() -> u128 {
    5000
}

#[derive(Debug, Clone, Deserialize)]
pub struct ActiveUtxoManagement {
    /// When `current_utxos_num` on the UTXO connector contract exceeds this
    /// threshold, the service calls `active_utxo_management` to consolidate.
    pub utxo_count_threshold: u32,
    /// How often to poll `get_metadata` for the current UTXO count.
    #[serde(default = "default_active_utxo_polling_interval_secs")]
    pub polling_interval_secs: u64,
    /// Optional fixed fee rate (sats/kvB) to pass to `active_utxo_management`.
    /// If unset, the connector falls back to the on-chain fee rate.
    #[serde(default)]
    pub fixed_fee_rate: Option<u64>,
    /// Optional cap on the number of UTXOs consumed per consolidation call.
    #[serde(default)]
    pub max_input_number: Option<u8>,
}

const fn default_active_utxo_polling_interval_secs() -> u64 {
    300
}

#[derive(Debug, Clone, Deserialize)]
pub struct Orchard {
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Wormhole {
    pub api_url: String,
    pub solana_chain_id: u64,
    #[serde(default)]
    pub fogo_chain_id: Option<u64>,
}

impl Wormhole {
    pub fn svm_chain_id(&self, chain_kind: ChainKind) -> u64 {
        match chain_kind {
            ChainKind::Sol => self.solana_chain_id,
            ChainKind::Fogo => self
                .fogo_chain_id
                .expect("wormhole.fogo_chain_id must be configured when [fogo] is enabled"),
            _ => panic!("svm_chain_id called for non-SVM chain {chain_kind:?}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    fn near_addr(account_id: &str) -> OmniAddress {
        OmniAddress::Near(AccountId::from_str(account_id).unwrap())
    }

    fn eth_addr(hex_suffix: u8) -> OmniAddress {
        OmniAddress::from_str(&format!(
            "eth:0x00000000000000000000000000000000000000{hex_suffix:02x}"
        ))
        .unwrap()
    }

    fn btc_addr(address: &str) -> OmniAddress {
        OmniAddress::from_str(&format!("btc:{address}")).unwrap()
    }

    fn rule(
        sender: Option<OmniAddress>,
        recipient: Option<OmniAddress>,
        destination_chain: Option<ChainKind>,
    ) -> AllowlistRule {
        AllowlistRule {
            sender,
            recipient,
            destination_chain,
        }
    }

    /// Parses an `[[allowlist]]` section with the same attributes as
    /// `Config::allowlist`.
    fn parse_allowlist(input: &str) -> Result<Vec<AllowlistRule>, toml::de::Error> {
        #[derive(Deserialize)]
        struct Wrapper {
            #[serde(default, deserialize_with = "validate_allowlist")]
            allowlist: Vec<AllowlistRule>,
        }
        toml::from_str::<Wrapper>(input).map(|wrapper| wrapper.allowlist)
    }

    #[test]
    fn empty_allowlist_allows_every_transfer() {
        let allowlist: Vec<AllowlistRule> = Vec::new();
        assert!(transfer_allowed(
            &allowlist,
            &near_addr("alice.near"),
            &eth_addr(1)
        ));
        assert!(transfer_allowed(
            &allowlist,
            &eth_addr(2),
            &near_addr("bob.near")
        ));
    }

    #[test]
    fn sender_only_rule_restricts_destination() {
        let allowlist = vec![rule(
            Some(near_addr("alice.near")),
            None,
            Some(ChainKind::Eth),
        )];
        // Alice may send to any recipient on Eth; other senders are dropped.
        assert!(transfer_allowed(
            &allowlist,
            &near_addr("alice.near"),
            &eth_addr(1)
        ));
        assert!(transfer_allowed(
            &allowlist,
            &near_addr("alice.near"),
            &eth_addr(2)
        ));
        assert!(!transfer_allowed(
            &allowlist,
            &near_addr("bob.near"),
            &eth_addr(1)
        ));
    }

    #[test]
    fn recipient_only_rule_restricts_destination() {
        let allowlist = vec![rule(None, Some(eth_addr(1)), None)];
        // Any sender may send to the listed recipient; other recipients are dropped.
        assert!(transfer_allowed(
            &allowlist,
            &near_addr("alice.near"),
            &eth_addr(1)
        ));
        assert!(transfer_allowed(
            &allowlist,
            &near_addr("bob.near"),
            &eth_addr(1)
        ));
        assert!(!transfer_allowed(
            &allowlist,
            &near_addr("alice.near"),
            &eth_addr(2)
        ));
    }

    #[test]
    fn paired_rule_requires_both_fields_to_match() {
        let allowlist = vec![rule(Some(near_addr("alice.near")), Some(eth_addr(1)), None)];
        assert!(transfer_allowed(
            &allowlist,
            &near_addr("alice.near"),
            &eth_addr(1)
        ));
        assert!(!transfer_allowed(
            &allowlist,
            &near_addr("alice.near"),
            &eth_addr(2)
        ));
        assert!(!transfer_allowed(
            &allowlist,
            &near_addr("bob.near"),
            &eth_addr(1)
        ));
    }

    #[test]
    fn any_matching_rule_allows_transfer() {
        // OR across rules: a sender-only rule and a recipient-only rule both
        // target Eth; matching either one is enough.
        let allowlist = vec![
            rule(Some(near_addr("alice.near")), None, Some(ChainKind::Eth)),
            rule(None, Some(eth_addr(9)), None),
        ];
        assert!(transfer_allowed(
            &allowlist,
            &near_addr("alice.near"),
            &eth_addr(1)
        ));
        assert!(transfer_allowed(
            &allowlist,
            &near_addr("bob.near"),
            &eth_addr(9)
        ));
        assert!(!transfer_allowed(
            &allowlist,
            &near_addr("bob.near"),
            &eth_addr(1)
        ));
    }

    #[test]
    fn unlisted_destinations_stay_unrestricted() {
        // Only Eth is restricted; transfers to other chains are unaffected.
        let allowlist = vec![
            rule(Some(near_addr("alice.near")), None, Some(ChainKind::Eth)),
            rule(None, Some(eth_addr(1)), None),
        ];
        assert!(transfer_allowed(
            &allowlist,
            &eth_addr(2),
            &near_addr("bob.near")
        ));
    }

    #[test]
    fn destination_restriction_detection() {
        let allowlist = vec![
            rule(Some(near_addr("alice.near")), None, Some(ChainKind::Eth)),
            rule(None, Some(btc_addr("bc1qtest")), None),
        ];
        // Restricted via explicit destination_chain and via the recipient's chain.
        assert!(destination_is_restricted(&allowlist, ChainKind::Eth));
        assert!(destination_is_restricted(&allowlist, ChainKind::Btc));
        assert!(!destination_is_restricted(&allowlist, ChainKind::Near));
        assert!(!destination_is_restricted(&[], ChainKind::Eth));
    }

    #[test]
    fn restricted_destination_chains_collects_target_chains() {
        let allowlist = [
            rule(Some(near_addr("alice.near")), None, Some(ChainKind::Eth)),
            rule(None, Some(btc_addr("bc1qtest")), None),
        ];
        let chains: BTreeSet<ChainKind> = allowlist
            .iter()
            .filter_map(AllowlistRule::target_chain)
            .collect();
        assert_eq!(chains, BTreeSet::from([ChainKind::Eth, ChainKind::Btc]));
    }

    #[test]
    fn sender_possibly_allowed_when_recipient_unknown() {
        let allowlist = vec![rule(
            Some(near_addr("alice.near")),
            Some(btc_addr("bc1qtest")),
            None,
        )];
        assert!(sender_possibly_allowed(
            &allowlist,
            &near_addr("alice.near"),
            ChainKind::Btc
        ));
        assert!(!sender_possibly_allowed(
            &allowlist,
            &near_addr("bob.near"),
            ChainKind::Btc
        ));
        // A recipient-only rule leaves every sender possibly allowed.
        let allowlist = vec![rule(None, Some(btc_addr("bc1qtest")), None)];
        assert!(sender_possibly_allowed(
            &allowlist,
            &near_addr("bob.near"),
            ChainKind::Btc
        ));
        // Unrestricted chains are unaffected.
        assert!(sender_possibly_allowed(
            &allowlist,
            &near_addr("bob.near"),
            ChainKind::Eth
        ));
    }

    #[test]
    fn recipient_possibly_allowed_when_sender_unknown_but_on_known_chain() {
        // A sender on the deposit's chain cannot be disproved, so the rule
        // counts for its recipient.
        let allowlist = vec![rule(
            Some(btc_addr("bc1qsender")),
            Some(near_addr("bob.near")),
            None,
        )];
        assert!(recipient_possibly_allowed(
            &allowlist,
            &near_addr("bob.near"),
            ChainKind::Btc
        ));
        assert!(!recipient_possibly_allowed(
            &allowlist,
            &near_addr("carol.near"),
            ChainKind::Btc
        ));
        // A sender on a different chain can never match a UTXO deposit.
        let allowlist = vec![rule(Some(eth_addr(1)), None, Some(ChainKind::Near))];
        assert!(!recipient_possibly_allowed(
            &allowlist,
            &near_addr("carol.near"),
            ChainKind::Btc
        ));
        // A recipient-only rule (no sender) applies regardless of origin.
        let allowlist = vec![rule(None, Some(near_addr("bob.near")), None)];
        assert!(recipient_possibly_allowed(
            &allowlist,
            &near_addr("bob.near"),
            ChainKind::Btc
        ));
        assert!(!recipient_possibly_allowed(
            &allowlist,
            &near_addr("carol.near"),
            ChainKind::Btc
        ));
        // Unrestricted destination chains are unaffected.
        assert!(recipient_possibly_allowed(
            &allowlist,
            &eth_addr(2),
            ChainKind::Btc
        ));
    }

    #[test]
    fn rule_with_unknown_field_fails_to_parse() {
        let err = parse_allowlist(
            "[[allowlist]]\nrecipeint = \"eth:0x0000000000000000000000000000000000000001\"\nsender = \"near:frolik.near\"\ndestination_chain = \"Eth\"",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("recipeint"), "unexpected error: {err}");
    }

    #[test]
    fn rule_with_no_sender_or_recipient_fails_to_parse() {
        let err = parse_allowlist("[[allowlist]]\ndestination_chain = \"Eth\"")
            .unwrap_err()
            .to_string();
        assert!(err.contains("sender"), "unexpected error: {err}");
    }

    #[test]
    fn sender_only_rule_without_destination_chain_fails_to_parse() {
        let err = parse_allowlist("[[allowlist]]\nsender = \"near:frolik.near\"")
            .unwrap_err()
            .to_string();
        assert!(err.contains("destination_chain"), "unexpected error: {err}");
    }

    #[test]
    fn recipient_destination_chain_mismatch_fails_to_parse() {
        let err = parse_allowlist(
            "[[allowlist]]\nrecipient = \"eth:0x0000000000000000000000000000000000000001\"\ndestination_chain = \"HlEvm\"",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("does not match"), "unexpected error: {err}");
    }

    #[test]
    fn allowlist_section_deserializes_from_toml() {
        // Sender-only; `HyperEvm` serializes as "HlEvm" (rename), "hlevm" alias
        // also accepted.
        let sender_only = parse_allowlist(
            "[[allowlist]]\nsender = \"near:frolik.near\"\ndestination_chain = \"hlevm\"",
        )
        .unwrap();
        assert_eq!(sender_only.len(), 1);
        assert_eq!(sender_only[0].sender, Some(near_addr("frolik.near")));
        assert_eq!(sender_only[0].recipient, None);
        assert_eq!(sender_only[0].destination_chain, Some(ChainKind::HyperEvm));

        // Recipient-only; destination chain derived from the recipient.
        let recipient_only = parse_allowlist(
            "[[allowlist]]\nrecipient = \"eth:0x0000000000000000000000000000000000000001\"",
        )
        .unwrap();
        assert_eq!(recipient_only[0].recipient, Some(eth_addr(1)));
        assert_eq!(recipient_only[0].target_chain(), Some(ChainKind::Eth));

        // Paired; explicit destination chain matching the recipient is accepted.
        let paired = parse_allowlist(
            "[[allowlist]]\nsender = \"near:alice.near\"\nrecipient = \"eth:0x0000000000000000000000000000000000000001\"\ndestination_chain = \"Eth\"",
        )
        .unwrap();
        assert_eq!(paired[0].sender, Some(near_addr("alice.near")));
        assert_eq!(paired[0].recipient, Some(eth_addr(1)));

        // Omitted section => empty (allow-all).
        assert!(parse_allowlist("").unwrap().is_empty());
    }
}
