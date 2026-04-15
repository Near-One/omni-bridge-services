use bson::oid::ObjectId;
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

use crate::integers::U128;
use crate::stream_types::AccountId;

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
