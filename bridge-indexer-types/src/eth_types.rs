// Implementations in this module are taken from eth-types crate of rainbow-bridge repository and adapted for local usage.
// The main intention of copying is to avoid heavy dependency from near-sdk. This should be changed in the future.

use crate::args_outcome_types::EthAddress;
use ethabi::{Event, EventParam, Hash, Log, ParamType, RawLog};
use ethereum_types;
use rlp::Decodable as RlpDecodable;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("Invalid RLP data")]
    RlpDecoding(rlp::DecoderError),
    #[error("Invalid Ethereum event log")]
    EthLogDecode(ethabi::Error),
    #[error("Invalid address format")]
    EthAddressDecode,
}

macro_rules! impl_compact_rlp_decode {
    ($name: ident, $size: expr) => {
        impl RlpDecodable for $name {
            fn decode(rlp: &rlp::Rlp) -> Result<Self, rlp::DecoderError> {
                rlp.decoder().decode_value(|bytes| {
                    if bytes.len() == $size {
                        let mut t = [0u8; $size];
                        t.copy_from_slice(bytes);
                        Ok($name(<ethereum_types::$name>::from(t)))
                    } else {
                        Err(rlp::DecoderError::RlpInvalidLength)
                    }
                })
            }
        }
    };
}

#[derive(Default, Clone, Copy, Eq, PartialEq, Debug)]
pub struct H256(pub ethereum_types::H256);
#[derive(Default, Clone, Copy, Eq, PartialEq, Debug)]
pub struct H160(pub ethereum_types::H160);

impl_compact_rlp_decode!(H256, 32);
impl_compact_rlp_decode!(H160, 20);

pub type EthEventParams = Vec<(String, ParamType, bool)>;

pub type LogEntryAddress = H160;

#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct LogEntry {
    pub address: LogEntryAddress,
    pub topics: Vec<H256>,
    pub data: Vec<u8>,
}

impl rlp::Decodable for LogEntry {
    fn decode(rlp: &rlp::Rlp) -> Result<Self, rlp::DecoderError> {
        let result = LogEntry {
            address: rlp.val_at(0usize)?,
            topics: rlp.list_at(1usize)?,
            data: rlp.val_at(2usize)?,
        };
        Ok(result)
    }
}

pub struct EthEvent {
    pub locker_address: EthAddress,
    pub log: Log,
}

impl EthEvent {
    pub fn from_log_entry_data(
        name: &str,
        params: EthEventParams,
        data: &[u8],
    ) -> Result<Self, Error> {
        let event = Event {
            name: name.to_string(),
            inputs: params
                .into_iter()
                .map(|(name, kind, indexed)| EventParam {
                    name,
                    kind,
                    indexed,
                })
                .collect(),
            anonymous: false,
        };
        let log_entry: LogEntry = rlp::decode(data).map_err(Error::RlpDecoding)?;
        let contract_address = EthAddress(log_entry.address.0.0);
        let topics = log_entry
            .topics
            .iter()
            .map(|h| Hash::from(&((h.0).0)))
            .collect();

        let raw_log = RawLog {
            topics,
            data: log_entry.data.clone(),
        };

        let log = event.parse_log(raw_log).map_err(Error::EthLogDecode)?;
        Ok(Self {
            locker_address: contract_address,
            log,
        })
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct EthUnlockedCompactEvent {
    pub token_eth_address: EthAddress,
}

impl EthUnlockedCompactEvent {
    fn event_params() -> EthEventParams {
        vec![
            ("token".to_string(), ParamType::String, false),
            ("sender".to_string(), ParamType::Address, true),
            ("amount".to_string(), ParamType::Uint(256), false),
            ("recipient".to_string(), ParamType::String, false),
            ("tokenEthAddress".to_string(), ParamType::Address, true),
        ]
    }

    pub fn from_log_entry_data(data: &[u8]) -> Result<Self, Error> {
        let event = EthEvent::from_log_entry_data(
            "Withdraw",
            EthUnlockedCompactEvent::event_params(),
            data,
        )?;
        if let Some(address) = event.log.params[4].value.clone().to_address() {
            Ok(EthUnlockedCompactEvent {
                token_eth_address: EthAddress(address.0),
            })
        } else {
            Err(Error::EthAddressDecode)
        }
    }
}
