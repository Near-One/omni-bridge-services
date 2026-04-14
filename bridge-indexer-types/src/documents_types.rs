use bson::oid::ObjectId;
use ethereum_types::H256;
use near_gas::NearGas;
use omni_types::mpc_types::SignatureResponse;
use omni_types::prover_result::{
    DeployTokenMessage, FinTransferMessage, InitTransferMessage, LogMetadataMessage,
};
use omni_types::{
    BasicMetadata, ChainKind, Fee, MetadataPayload, Nonce, OmniAddress, TransferMessagePayload,
    UnifiedTransferId,
};
use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;
use utoipa::ToSchema;

use crate::stream_types::{AccountId, CryptoHash};
use crate::{
    args_outcome_types::{DepositRecipient, EthAddress, TransferMessage, TxFromAurora},
    compact_types::{
        CompactIndexerExecutionOutcomeWithReceipt, CompactIndexerTransactionWithOutcome,
    },
    integers::U128,
};

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub enum ReceiptProcessor {
    FactoryWithdraw,
    FactoryForceWithdraw,
    FactoryDeposit,
    EthConnectorDeposit,
    EthConnectorWithdraw,
    NewEthConnectorWithdraw,
    NewEthConnectorDeposit,
    ENearWithdraw,
    ENearDeposit,
    FastBridgeInit,
    FastBridgeLpUnlock,
    FastBridgeWithdraw,
    FastBridgeUnlock,
    Nep141LockerDeposit,
    Nep141LockerWithdraw,
    AuroraFtOnTransfer,
    AuroraFtTransfer,
    FromAuroraTransfer,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub enum Network {
    Ethereum,
    Near,
    Aurora,
}

#[skip_serializing_none]
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct NearTransaction {
    #[serde(rename = "_id")]
    pub id: Option<ObjectId>,
    pub source_network: Option<Network>,
    pub destination_network: Option<Network>,
    pub block_height: u64,
    pub block_timestamp_nanosec: u64,
    pub tx_hash: CryptoHash,
    pub receipt_id: CryptoHash,
    pub contract_id: AccountId,
    pub signer_id: AccountId,
    pub predecessor_id: AccountId,
    pub version: u32,
    pub receipt_type: ReceiptProcessor,
    #[serde(flatten)]
    pub data: NearTransactionData,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum OmniTransferStatus {
    /// The transfer was initialized on the sender's chain.
    Initialized,
    /// The transfer was signed on NEAR using MPC.
    Signed,
    /// Fast transfer was initiated and received on NEAR, but not yet finalised on recipient's
    /// chain.
    FastFinalisedOnNear,
    /// If the recipient's chain isn't Near then this indicates that the transfer was finalised on
    /// NEAR, but not yet on the recipient's chain.
    FinalisedOnNear,
    /// Recipient received tokens through the fast transfer.
    FastFinalised,
    /// The transfer was finalised  on the recipient's chain.
    Finalised,
    /// The transfer fee was claimed.
    Claimed,
}

#[skip_serializing_none]
#[derive(Deserialize, Serialize, Debug, Clone)]
pub enum OmniTransactionOrigin {
    NearReceipt {
        raw_receipt_id: Option<ObjectId>,
        block_height: u64,
        block_timestamp_nanosec: u64,
        receipt_id: String,
        contract_id: AccountId,
        signer_id: AccountId,
        predecessor_id: AccountId,
        version: u32,
    },
    EVMLog {
        block_number: u64,
        block_timestamp: u64,
        transaction_index: Option<u64>,
        log_index: Option<u64>,
        chain_kind: ChainKind,
    },
    EVMOnNearLog {
        block_number: u64,
        block_timestamp: u64,
        transaction_index: Option<u64>,
        log_index: Option<u64>,
    },
    SolanaTransaction {
        slot: u64,
        // estimated production time, as Unix timestamp (seconds since the Unix epoch) of when the transaction was processed.
        block_time: u64,
        instruction_index: usize,
    },
    UtxoTransaction {
        block_height: u64,
        block_hash: String,
        block_time: u64,
        chain_kind: ChainKind,
    },
    StarknetTransaction {
        block_number: u64,
        block_timestamp: u64,
        event_index: Option<u64>,
    },
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub enum OmniTransferMessage {
    NearFailedTransferMessage(omni_types::TransferMessage),
    NearTransferMessage(omni_types::TransferMessage),
    NearFastTransferMessage {
        fast_transfer: omni_types::FastTransfer,
        /// New transfer id that will be used for signing a transfer message when fast transfer is
        /// done to chain other than Near.
        /// None if the transfer is done to Near.
        new_transfer_id: Option<UnifiedTransferId>,
    },
    NearUtxoTransferMessage {
        utxo_transfer_message: omni_types::UtxoFinTransferMsg,
        token_id: AccountId,
        amount: U128,
        new_transfer_id: Option<UnifiedTransferId>,
    },
    NearSignTransferEvent(NearSignTransferEvent),
    NearClaimFeeEvent(omni_types::TransferMessage),
    EvmInitTransferMessage(InitTransferMessage),
    EvmFinTransferMessage(FinTransferMessage),
    SolanaInitTransfer(SolanaInitTransferMessage),
    SolanaFinTransfer(SolanaFinTransferMessage),
    UtxoSignTransaction {
        destination_chain: ChainKind,
        relayer: AccountId,
    },
    TransferNearToUtxo {
        destination_chain: ChainKind,
        utxo_count: u64,
        sender: AccountId,
        recipient_id: String,
        amount: U128,
        new_transfer_id: Option<UnifiedTransferId>,
    },
    TransferUtxoToNear {
        deposit_msg: DepositMsg,
    },
    UtxoVerifyDeposit {
        details: VerifyDepositDetails,
    },
    UtxoVerifyWithdraw {
        details: VerifyWithdrawDetails,
    },
    UtxoConfirmedTxHash {
        destination_chain: ChainKind,
    },
    StarknetInitTransfer(StarknetInitTransferMessage),
    StarknetFinTransfer(StarknetFinTransferMessage),
}

#[skip_serializing_none]
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct NearSignTransferEvent {
    pub signature: SignatureResponse,
    pub message_payload: TransferMessagePayload,
}

#[skip_serializing_none]
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct SolanaInitTransferMessage {
    pub amount: U128,
    pub fee: Fee,
    pub token: OmniAddress,
    pub recipient: OmniAddress,
    pub sender: OmniAddress,
    pub origin_nonce: Nonce,
    // message and emitter fields are optional only because it was added after we had a version already deployed.
    // If we do data migration in deployed mongo databases we can make them required.
    pub message: Option<String>,
    pub emitter: Option<String>,
    // Note that the sequence is the same as origin_nonce.
}

#[skip_serializing_none]
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct SolanaFinTransferMessage {
    pub amount: U128,
    pub destination_nonce: u64,
    pub fee_recipient: Option<String>,
    // emitter and sequence are optional only because it was added after we had a version already deployed.
    // If we do data migration in deployed mongo databases we can make this required.
    pub emitter: Option<String>,
    pub sequence: Option<u64>,
}

#[skip_serializing_none]
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct StarknetInitTransferMessage {
    pub amount: U128,
    pub fee: Fee,
    pub token: OmniAddress,
    pub recipient: OmniAddress,
    pub sender: OmniAddress,
    pub origin_nonce: Nonce,
    pub message: String,
}

#[skip_serializing_none]
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct StarknetFinTransferMessage {
    pub token: OmniAddress,
    pub amount: U128,
    pub recipient: OmniAddress,
    pub destination_nonce: u64,
    pub fee_recipient: Option<String>,
    pub message: Option<String>,
}

#[derive(Deserialize, Serialize, Debug, Clone, Hash, Eq, PartialEq)]
pub struct CoingeckoTokenId(pub String);

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct OmniTokenInfo {
    // Token id on Coingecko
    pub token_id: Option<CoingeckoTokenId>,
    pub name: String,
    pub symbol: String,
    pub decimals: u32,
    pub usd_price: Option<f64>,
}

#[skip_serializing_none]
#[derive(Default, Deserialize, Serialize, Debug, Clone)]
pub enum OmniEnrichmentData {
    /// None indicates that enrichment wasn't done yet.
    #[default]
    None,
    /// `NotApplicable` indicates enrichment isn't applicable for transaction.
    NotApplicable,
    /// Data cointains enrichment data.
    Data {
        // The token being transferred may not always be available on coingecko in which case we
        // cannot find information about it.
        transferred_token_info: Option<OmniTokenInfo>,
        native_token_info: OmniTokenInfo,
    },
}

impl OmniEnrichmentData {
    fn is_none(&self) -> bool {
        matches!(self, OmniEnrichmentData::None)
    }
}

#[skip_serializing_none]
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct FailedEvent {
    #[serde(rename = "_id")]
    pub id: Option<ObjectId>,

    // Transaction id is a hash that identifies transaction this event was found in.
    // In case of near and evm it's transaction hash. In case of solana - signature.
    pub transaction_id: String,
    pub signer: String,
    pub recipient: String,
    pub chain: ChainKind,
}

#[allow(clippy::large_enum_variant)]
#[skip_serializing_none]
#[derive(Deserialize, Serialize, Debug, Clone)]
pub enum OmniEventData {
    Transaction(OmniTransactionEvent),
    Meta(OmniMetaEvent),
}

#[skip_serializing_none]
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct OmniEvent {
    #[serde(rename = "_id")]
    pub id: ObjectId,

    // Transaction id is a hash that identifies transaction this event was found in.
    // In case of near and evm it's transaction hash. In case of solana - signature.
    pub transaction_id: String,
    pub origin: OmniTransactionOrigin,

    #[serde(flatten)]
    pub event: OmniEventData,
}

impl OmniEvent {
    #[must_use]
    pub fn get_id(&self) -> ObjectId {
        self.id
    }

    #[must_use]
    pub fn get_transaction_id(&self) -> String {
        self.transaction_id.clone()
    }

    #[must_use]
    pub fn get_origin(&self) -> OmniTransactionOrigin {
        self.origin.clone()
    }

    #[must_use]
    pub fn get_transfer_id(&self) -> Option<UnifiedTransferId> {
        match self.event.clone() {
            OmniEventData::Transaction(event) => Some(event.transfer_id),
            OmniEventData::Meta(_) => None,
        }
    }

    #[must_use]
    pub fn get_sender(&self) -> Option<OmniAddress> {
        match &self.event {
            OmniEventData::Transaction(event) => event.sender.clone(),
            OmniEventData::Meta(_) => None,
        }
    }

    #[must_use]
    pub fn get_status(&self) -> Option<OmniTransferStatus> {
        match &self.event {
            OmniEventData::Transaction(event) => Some(event.status),
            OmniEventData::Meta(_) => None,
        }
    }
}

#[skip_serializing_none]
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct OmniTransactionEvent {
    #[serde(flatten)]
    pub transfer_message: OmniTransferMessage,
    // Sender should be the same as in transfer message.
    // Having it separate allows for easier manipulations with mongo (indexing and querying).
    pub sender: Option<OmniAddress>,
    pub transfer_id: UnifiedTransferId,
    pub status: OmniTransferStatus,

    #[serde(default, skip_serializing_if = "OmniEnrichmentData::is_none")]
    pub enrichment_data: OmniEnrichmentData,
}

#[skip_serializing_none]
#[derive(Deserialize, Serialize, Debug, Clone)]
pub enum OmniMetaEventDetails {
    EVMDeployToken(DeployTokenMessage),
    EVMLogMetadata(LogMetadataMessage),
    SolanaDeployToken {
        token: String,
        name: String,
        symbol: String,
        decimals: u8,
        emitter: String,
        sequence: u64,
    },
    SolanaLogMetadata {
        token: String,
        emitter: String,
        sequence: u64,
    },
    NearLogMetadataEvent {
        signature: SignatureResponse,
        metadata_payload: MetadataPayload,
    },
    NearDeployTokenEvent {
        token_id: AccountId,
        token_address: OmniAddress,
        metadata: BasicMetadata,
    },
    NearMigrateTokenEvent {
        old_token_id: AccountId,
        new_token_id: AccountId,
    },
    NearBindTokenEvent {
        token_id: AccountId,
        token_address: OmniAddress,
        decimals: u8,
        origin_decimals: u8,
    },
    EVMOnNearEvent {
        chain: String,
        near_transaction_hash: String,
        sender: String,
        erc20_address: String,
        dest: String,
        amount: String,
        error: String,
    },
    EVMOnNearInternalTransaction {
        chain: String,
        error: String,
    },
    UtxoLogDepositAddress(LogDepositAddress),
    StarknetDeployToken {
        token_address: String,
        near_token_id: String,
        name: String,
        symbol: String,
        decimals: u8,
        origin_decimals: u8,
    },
    StarknetLogMetadata {
        token_address: String,
        name: String,
        symbol: String,
        decimals: u8,
    },
    NearRelayerApplyEvent {
        account_id: AccountId,
        stake: String,
        activate_at: String,
    },
    NearRelayerResignEvent {
        account_id: AccountId,
        stake: String,
    },
    NearRelayerRejectEvent {
        account_id: AccountId,
        stake: String,
    },
}

#[skip_serializing_none]
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct OmniMetaEvent {
    #[serde(flatten)]
    pub details: OmniMetaEventDetails,
}

#[skip_serializing_none]
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct UtxoTrackedAddress {
    #[serde(rename = "_id")]
    pub id: Option<ObjectId>,
    pub chain: ChainKind,
    pub deposit_address: String,
    pub deposit_msg: DepositMsg,
}

#[skip_serializing_none]
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct UtxoPendingTxHash {
    #[serde(rename = "_id")]
    pub id: Option<ObjectId>,
    pub chain: ChainKind,
    pub btc_tx_hash: String,
    pub vout: u32,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub enum NearTransactionData {
    StatusFailed(String),
    ProcessingFailed(String),
    Withdraw(WithdrawTransaction),
    Deposit(DepositTransaction),
    FastBridgeInit(FastBridgeInitTransaction),
    FastBridgeLpUnlock(FastBridgeLpUnlockTransaction),
    FastBridgeWithdraw(FastBridgeWithdrawTransaction),
    FastBridgeUnlock(FastBridgeUnlockTransaction),
    AuroraDeposit(AuroraDepositTransaction),
    AuroraWithdraw(AuroraWithdrawTransaction),
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
#[skip_serializing_none]
#[serde(rename_all = "snake_case")]
pub struct FinancialData {
    pub transferred_tokens: TokensValue,
    pub user_paid_fee: Option<TokensValue>,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct TokensValue {
    pub token_data: TokenData,
    pub normalized_amount: Option<f64>,
    pub usd_value: Option<f64>,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub struct TokenData {
    // Token id on Coingecko
    pub token_id: Option<String>,
    // Platform id on Coingecko
    pub platform_id: String,
    // Token address on the platform
    pub address: Option<String>,
    pub name: String,
    pub symbol: String,
    pub decimals: Option<u8>,
}

#[derive(Deserialize, Serialize, Debug, Clone, Default, PartialEq)]
#[serde(rename_all = "snake_case")]
#[skip_serializing_none]
pub struct EnrichmentData {
    pub financial_data: Option<FinancialData>,
    pub init_txn_hash: Option<String>,
    pub aurora_token_address: Option<String>,
}

impl EnrichmentData {
    pub fn merge(&mut self, other: &EnrichmentData) {
        macro_rules! merge_field {
            ($field:ident) => {
                if other.$field.is_some() {
                    self.$field = other.$field.clone();
                }
            };
        }

        merge_field!(financial_data);
        merge_field!(init_txn_hash);
        merge_field!(aurora_token_address);
    }
}

#[allow(clippy::large_enum_variant)]
#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub enum EnrichmentResult {
    EnrichmentData(EnrichmentData),
    EnrichmentFailed(String),
}

#[skip_serializing_none]
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct WithdrawTransaction {
    pub recipient_address: EthAddress,
    pub transferred_amount: U128,
    pub real_predecessor_id: Option<AccountId>,
    pub near_token_id: Option<AccountId>,
    pub eth_token_address: Option<EthAddress>,
    pub tx_from_aurora: Option<TxFromAurora>,
}

#[skip_serializing_none]
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct DepositTransaction {
    pub recipient: DepositRecipient,
    pub transferred_amount: U128,
    pub proof_info: CompactProof,
    pub real_predecessor_id: AccountId,
    pub near_token_id: Option<AccountId>,
    pub eth_token_address: Option<EthAddress>,
}

#[skip_serializing_none]
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct AuroraDepositTransaction {
    pub recipient_address: EthAddress,
    pub transferred_amount: U128,
    pub real_predecessor_id: AccountId,
    pub near_token_id: AccountId,
    pub aurora_tx: H256,
}

#[skip_serializing_none]
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct AuroraWithdrawTransaction {
    pub recipient_id: AccountId,
    pub transferred_amount: U128,
    pub near_token_id: AccountId,
    pub tx_from_aurora: Option<TxFromAurora>,
    pub aurora_token_address: Option<EthAddress>,
}

#[skip_serializing_none]
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct FastBridgeInitTransaction {
    pub nonce: U128,
    pub sender_id: AccountId,
    pub transfer_message: TransferMessage,
    pub transfer_id: String,
    pub tx_from_aurora: Option<TxFromAurora>,
}

#[skip_serializing_none]
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct FastBridgeLpUnlockTransaction {
    pub nonce: U128,
    pub recipient_id: AccountId,
    pub transfer_message: TransferMessage,
    pub transfer_id: String,
    pub tx_from_aurora: Option<TxFromAurora>,
}

#[skip_serializing_none]
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct FastBridgeWithdrawTransaction {
    pub token_id: AccountId,
    pub amount: U128,
    pub sender_id: Option<AccountId>,
    pub recipient_id: AccountId,
    pub tx_from_aurora: Option<TxFromAurora>,
}

#[skip_serializing_none]
#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct FastBridgeUnlockTransaction {
    pub nonce: U128,
    pub recipient_id: AccountId,
    pub transfer_message: TransferMessage,
    pub tx_from_aurora: Option<TxFromAurora>,
}

#[skip_serializing_none]
#[derive(Deserialize, Serialize, Hash, PartialEq, Eq, Debug, Clone, ToSchema)]
pub struct PostAction {
    #[schema(value_type = String, example = "example.near")]
    pub receiver_id: AccountId,
    #[schema(
        value_type = String,
        format = "uint128",
        example = "1000000000000000000000000"
    )]
    pub amount: U128,
    pub memo: Option<String>,
    pub msg: String,
    #[schema(value_type = String, format = "uint64", example = "3000000000000")]
    pub gas: Option<NearGas>,
}

#[derive(Deserialize, Serialize, Hash, PartialEq, Eq, Debug, Clone, ToSchema)]
pub struct SafeDepositMsg {
    pub msg: String,
}

#[skip_serializing_none]
#[derive(Deserialize, Serialize, Hash, PartialEq, Eq, Debug, Clone)]
pub struct DepositMsg {
    pub recipient_id: AccountId,
    pub post_actions: Option<Vec<PostAction>>,
    pub extra_msg: Option<String>,
    pub safe_deposit: Option<SafeDepositMsg>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct LogDepositAddress {
    pub chain: ChainKind,
    pub deposit_msg: DepositMsg,
    pub path: String,
    pub deposit_address: String,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct VerifyDepositDetails {
    pub recipient_id: AccountId,
    pub mint_amount: U128,
    pub protocol_fee: U128,
    pub relayer_account_id: AccountId,
    pub relayer_fee: U128,
    pub success: bool,
}

impl VerifyDepositDetails {
    pub const EVENT: &str = "verify_deposit_details";
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct VerifyWithdrawDetails {
    pub account_id: AccountId,
    pub burn_amount: U128,
    pub protocol_fee: U128,
    pub refund: U128,
    pub relayer_account_id: AccountId,
    pub relayer_fee: U128,
    pub success: bool,
}

impl VerifyWithdrawDetails {
    pub const EVENT: &str = "verify_withdraw_details";
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct NearEvent<T> {
    pub standard: String,
    pub version: String,
    pub event: String,
    pub data: Vec<T>,
}

/// Trusted relayer events emitted by `omni-utils` as plain JSON logs
/// (not wrapped in NEP-297 `EVENT_JSON:` format).
#[derive(Deserialize, Serialize, Debug, Clone)]
pub enum TrustedRelayerEvent {
    RelayerApplyEvent {
        account_id: AccountId,
        stake: String,
        activate_at: String,
    },
    RelayerResignEvent {
        account_id: AccountId,
        stake: String,
    },
    RelayerRejectEvent {
        account_id: AccountId,
        stake: String,
    },
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct RawReceipt<
    Transaction = CompactIndexerTransactionWithOutcome,
    Receipt = CompactIndexerExecutionOutcomeWithReceipt,
> where
    Transaction: Sized,
    Receipt: Sized,
{
    #[serde(rename = "_id", skip_serializing_if = "Option::is_none")]
    pub id: Option<ObjectId>,
    pub block_height: u64,
    pub block_timestamp_nanosec: u64,
    pub receipt: Receipt,
    pub parent_transaction: Transaction,
    pub parent_receipts: Vec<Receipt>,
    pub version: u32,
}

#[skip_serializing_none]
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CompactProof {
    pub log_index: u64,
    pub receipt_index: u64,
    pub block_height: u64,
    pub transaction_hash: Option<H256>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all = "snake_case")]
pub enum ReceiptProcessingStatus {
    Indexed,
    Processed,
    SkippedProcessing,
    Enriched,
    SkippedEnrichment,
}
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ReceiptProcessingInfo {
    #[serde(rename = "_id")]
    pub id: ObjectId,
    pub status: ReceiptProcessingStatus,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct RawReceiptProjection {
    #[serde(rename = "_id")]
    pub id: Option<ObjectId>,
    pub block_height: Option<u64>,
}

#[derive(Deserialize, Serialize, Debug, Clone)]
pub struct ProcessedReceiptInfoProjection {
    #[serde(rename = "_id")]
    pub id: Option<ObjectId>,
    pub status: Option<ReceiptProcessingStatus>,
}

#[cfg(test)]
mod tests {
    use super::{TokenData, TokensValue};

    #[test]
    fn test_enrichment_result_to_bson() {
        let mock_enrichment_result_1 =
            super::EnrichmentResult::EnrichmentData(super::EnrichmentData {
                financial_data: None,
                init_txn_hash: None,
                aurora_token_address: None,
            });
        let mock_enrichment_result_2 = super::EnrichmentResult::EnrichmentFailed("".to_string());
        let mock_enrichment_result_3 =
            super::EnrichmentResult::EnrichmentData(super::EnrichmentData {
                financial_data: Some(super::FinancialData {
                    transferred_tokens: TokensValue {
                        token_data: TokenData {
                            token_id: Some("test".to_string()),
                            decimals: Some(18),
                            name: "test".to_string(),
                            symbol: "test".to_string(),
                            platform_id: "test".to_string(),
                            address: Some("test".to_string()),
                        },
                        normalized_amount: Some(11111.11),
                        usd_value: Some(11111.11),
                    },
                    user_paid_fee: Some(TokensValue {
                        token_data: TokenData {
                            token_id: Some("test".to_string()),
                            decimals: Some(18),
                            name: "test".to_string(),
                            symbol: "test".to_string(),
                            platform_id: "test".to_string(),
                            address: Some("test".to_string()),
                        },
                        normalized_amount: Some(11111.11),
                        usd_value: Some(11111.11),
                    }),
                }),
                init_txn_hash: Some("test".to_string()),
                aurora_token_address: Some("test".to_string()),
            });
        let _ = bson::to_bson(&mock_enrichment_result_1).unwrap();
        let _ = bson::to_bson(&mock_enrichment_result_2).unwrap();
        let _ = bson::to_bson(&mock_enrichment_result_3).unwrap();
    }

    #[test]
    fn test_receipt_processing_status_to_bson() {
        let _ = bson::to_bson(&super::ReceiptProcessingStatus::Indexed).unwrap();
        let _ = bson::to_bson(&super::ReceiptProcessingStatus::Processed).unwrap();
        let _ = bson::to_bson(&super::ReceiptProcessingStatus::SkippedProcessing).unwrap();
        let _ = bson::to_bson(&super::ReceiptProcessingStatus::Enriched).unwrap();
        let _ = bson::to_bson(&super::ReceiptProcessingStatus::SkippedEnrichment).unwrap();
    }

    #[test]
    fn test_merge_covers_all_fields() {
        let mut base = super::EnrichmentData::default();
        let other = super::EnrichmentData {
            financial_data: Some(super::FinancialData {
                transferred_tokens: TokensValue {
                    token_data: TokenData {
                        token_id: Some("test".to_string()),
                        decimals: Some(18),
                        name: "test".to_string(),
                        symbol: "test".to_string(),
                        platform_id: "test".to_string(),
                        address: Some("test".to_string()),
                    },
                    normalized_amount: Some(11111.11),
                    usd_value: Some(11111.11),
                },
                user_paid_fee: Some(TokensValue {
                    token_data: TokenData {
                        token_id: Some("test".to_string()),
                        decimals: Some(18),
                        name: "test".to_string(),
                        symbol: "test".to_string(),
                        platform_id: "test".to_string(),
                        address: Some("test".to_string()),
                    },
                    normalized_amount: Some(11111.11),
                    usd_value: Some(11111.11),
                }),
            }),
            init_txn_hash: Some("hash".to_string()),
            aurora_token_address: Some("address".to_string()),
        };

        base.merge(&other);

        // This assertion will fail if we add a new field to EnrichmentData and forget to merge it.
        assert_eq!(base, other, "merge function missed some fields");
    }
}
