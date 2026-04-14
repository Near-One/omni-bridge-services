use std::fmt;
use std::str::FromStr;

use crate::integers::U128;
use crate::stream_types::{AccountId, Balance, BorshDeserialize, BorshSerialize, borsh};
use ethereum_types::{H256, U256};
use hex::FromHex;
use serde::{Deserialize, Serialize, de::IntoDeserializer};
use serde_with::skip_serializing_none;

pub type ResultPrefix = [u8; 32];

#[derive(Debug, Clone, BorshDeserialize, BorshSerialize)]
pub struct FactoryFinishWithdrawArgs {
    pub amount: Balance,
    pub recipient: String,
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct NearRecipient {
    pub target: AccountId,
    pub message: Option<String>,
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct FtLockerStorageBalanceClbkArgs {
    pub proof: Proof,
    pub token: AccountId,
    pub recipient: NearRecipient,
    pub amount: Balance,
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct FactoryFinishDepositArgs {
    pub token: String,
    pub new_owner_id: String,
    pub amount: Balance,
    pub proof: Proof,
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct ENearFinishDepositArgs {
    pub new_owner_id: String,
    pub amount: Balance,
    pub proof: Proof,
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct EthConnectorFinishDepositArgs {
    pub new_owner_id: AccountId,
    pub amount: u128,
    pub proof_key: String,
    pub relayer_id: AccountId,
    pub fee: u128,
    pub msg: Option<Vec<u8>>,
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct NewEthConnectorFinishDepositArgs {
    pub new_owner_id: AccountId,
    pub amount: u128,
    pub proof_key: String,
    pub msg: Option<Vec<u8>>,
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize, PartialEq, Eq)]
pub struct EthConnectorTransferArgs {
    pub receiver_id: AccountId,
    pub amount: u128,
    pub memo: Option<String>,
    pub msg: String,
}

#[derive(BorshDeserialize, BorshSerialize, Clone)]
pub struct EthConnectorDepositEthArgs {
    pub proof: Proof,
    pub relayer_eth_account: EthAddress,
}

#[derive(Debug, Clone, BorshSerialize, BorshDeserialize)]
pub struct EthConnectorWithdrawResult {
    pub amount: u128,
    pub recipient_id: EthAddress,
    pub eth_custodian_address: EthAddress,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FtOnTansferArgs {
    pub sender_id: AccountId,
    pub amount: U128,
    pub msg: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FtTansferArgs {
    pub receiver_id: AccountId,
    pub amount: U128,
    pub memo: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FastBridgeWithdrawClbkArgs {
    pub token_id: AccountId,
    pub amount: U128,
    pub sender_id: Option<AccountId>,
    pub recipient_id: AccountId,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FastBridgeWithdrawClbkLegacyArgs {
    pub token_id: AccountId,
    pub amount: U128,
    pub recipient_id: AccountId,
}

impl From<FastBridgeWithdrawClbkLegacyArgs> for FastBridgeWithdrawClbkArgs {
    fn from(args: FastBridgeWithdrawClbkLegacyArgs) -> Self {
        FastBridgeWithdrawClbkArgs {
            token_id: args.token_id,
            amount: args.amount,
            sender_id: None,
            recipient_id: args.recipient_id,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FastBridgeUnlockClbkArgs {
    pub verification_result: bool,
    pub nonce: U128,
}

#[skip_serializing_none]
#[derive(serde::Deserialize, serde::Serialize, Debug, Clone)]
pub struct TxFromAurora {
    pub aurora_signer_address: EthAddress,
    pub aurora_tx_hash: H256,
    pub aurora_to_address: Option<EthAddress>,
}

#[derive(Debug, Eq, PartialEq, BorshSerialize, BorshDeserialize)]
pub enum FactoryResultType {
    Withdraw {
        amount: Balance,
        token: EthAddress,
        recipient: EthAddress,
    },
}

#[derive(Debug, Eq, PartialEq, BorshSerialize, BorshDeserialize)]
pub struct Nep141LockerLock {
    pub prefix: ResultPrefix,
    pub token: String,
    pub amount: Balance,
    pub recipient: EthAddress,
}

#[derive(Debug, Eq, PartialEq, BorshSerialize, BorshDeserialize)]
pub enum ENearResultType {
    MigrateNearToEthereum {
        amount: Balance,
        recipient: EthAddress,
    },
}

#[derive(Debug, Default, BorshDeserialize, BorshSerialize, Clone, Serialize, Deserialize)]
pub struct Proof {
    pub log_index: u64,
    pub log_entry_data: Vec<u8>,
    pub receipt_index: u64,
    pub receipt_data: Vec<u8>,
    pub header_data: Vec<u8>,
    pub proof: Vec<Vec<u8>>,
}

#[derive(Debug, Eq, PartialEq, BorshSerialize, BorshDeserialize, Clone)]
pub struct EthAddress(pub [u8; 20]);

impl<'de> Deserialize<'de> for EthAddress {
    fn deserialize<D>(deserializer: D) -> Result<Self, <D as serde::Deserializer<'de>>::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = <String as Deserialize>::deserialize(deserializer)?;
        s.parse()
            .map_err(|err: ParseEthAddressError| serde::de::Error::custom(err.0))
    }
}

impl Serialize for EthAddress {
    fn serialize<S>(
        &self,
        serializer: S,
    ) -> Result<<S as serde::Serializer>::Ok, <S as serde::Serializer>::Error>
    where
        S: serde::Serializer,
    {
        let hex_str = format!("0x{}", hex::encode(self.0));
        serializer.serialize_str(&hex_str)
    }
}

impl fmt::Display for EthAddress {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let hex_str = format!("0x{}", hex::encode(self.0));
        write!(f, "{hex_str}")
    }
}

impl EthAddress {
    #[must_use]
    pub fn encode(&self) -> String {
        hex::encode(self.0)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ParseEthAddressError(String);

impl FromStr for EthAddress {
    type Err = ParseEthAddressError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = if let Some(stripped) = s.strip_prefix("0x") {
            stripped
        } else {
            s
        };
        let result = Vec::from_hex(s).map_err(|err| ParseEthAddressError(err.to_string()))?;
        Ok(EthAddress(result.try_into().map_err(|err| {
            ParseEthAddressError(format!("Invalid length: {err:?}"))
        })?))
    }
}

#[skip_serializing_none]
#[derive(serde::Deserialize, serde::Serialize, Debug, Clone)]
pub struct DepositRecipient {
    pub target: AccountId,
    pub message: Option<String>,
    pub address_from_message: Option<EthAddress>,
}

impl DepositRecipient {
    /// # Panics
    ///
    /// Will panic if `recipient` is not a valid `AccountId`.
    #[must_use]
    pub fn new(recipient: &str) -> Self {
        if recipient.contains(':') {
            let mut iter = recipient.split(':');
            let target = iter.next().unwrap().parse().unwrap();
            let message = iter.collect::<Vec<&str>>().join(":");

            Self {
                target,
                address_from_message: Self::get_eth_address(&message),
                message: Some(message),
            }
        } else {
            Self {
                target: recipient.parse().unwrap(),
                message: None,
                address_from_message: None,
            }
        }
    }

    #[must_use]
    pub fn get_eth_address(message: &str) -> Option<EthAddress> {
        if message.len() >= 40 {
            let hex_message = &message[message.len() - 40..];
            let res: Result<EthAddress, serde::de::value::Error> =
                <EthAddress as Deserialize>::deserialize(hex_message.into_deserializer());
            res.ok()
        } else {
            None
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TransferDataEthereum {
    pub token_near: AccountId,
    pub token_eth: EthAddress,
    pub amount: U128,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TransferDataNear {
    pub token: AccountId,
    pub amount: U128,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TransferMessage {
    pub valid_till: u64,
    pub transfer: TransferDataEthereum,
    pub fee: TransferDataNear,
    pub recipient: EthAddress,
    pub valid_till_block_height: Option<u64>,
    pub aurora_sender: Option<EthAddress>,
}

impl TransferMessage {
    #[must_use]
    pub fn get_transfer_id(&self, nonce: U256) -> String {
        let amount = U256::from(self.transfer.amount.0);

        let mut be_nonce = [0u8; 32];
        nonce.to_big_endian(&mut be_nonce);
        let mut be_amount = [0u8; 32];

        amount.to_big_endian(&mut be_amount);

        let encoded = [
            self.transfer.token_eth.0.as_slice(),
            self.recipient.0.as_slice(),
            be_nonce.as_slice(),
            be_amount.as_slice(),
        ]
        .concat();

        format!("{:x}", TransferMessage::keccak256(encoded.as_slice()))
    }

    fn keccak256(bytes: &[u8]) -> H256 {
        use tiny_keccak::{Hasher, Keccak};
        let mut output = [0u8; 32];
        let mut hasher = Keccak::v256();
        hasher.update(bytes);
        hasher.finalize(&mut output);
        H256::from_slice(&output)
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "event", content = "data")]
#[serde(rename_all = "snake_case")]
pub enum Event {
    FastBridgeInitTransferEvent {
        nonce: U128,
        sender_id: AccountId,
        transfer_message: TransferMessage,
    },
    FastBridgeUnlockEvent {
        nonce: U128,
        recipient_id: AccountId,
        transfer_message: TransferMessage,
    },
    FastBridgeLpUnlockEvent {
        nonce: U128,
        recipient_id: AccountId,
        transfer_message: TransferMessage,
    },
    FastBridgeDepositEvent {
        sender_id: AccountId,
        token: AccountId,
        amount: U128,
    },
    FastBridgeWithdrawEvent {
        sender_id: Option<AccountId>,
        recipient_id: AccountId,
        token: AccountId,
        amount: U128,
    },
}

pub const STANDARD: &str = "nep297";
pub const VERSION: &str = "1.0.0";
pub const EVENT_JSON_STR: &str = "EVENT_JSON:";
