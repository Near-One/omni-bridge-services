use std::collections::HashMap;

use anyhow::{Context, Result};
use evm_bridge_client::{EvmBridgeClient, EvmBridgeClientBuilder};
use hypercore_bridge_client::{
    HyperCoreBridgeClient, HyperCoreBridgeClientBuilder, HyperliquidNetwork,
};
use light_client::{LightClient, LightClientBuilder};
use near_bridge_client::{NearBridgeClientBuilder, UTXOChainAccounts};
use near_crypto::InMemorySigner;
use omni_connector::{OmniConnector, OmniConnectorBuilder};
use omni_types::{ChainKind, mpc_types::MpcFinality};
use solana_bridge_client::{SolanaBridgeClient, SolanaBridgeClientBuilder};
use solana_client::nonblocking::rpc_client::RpcClient;
use starknet_bridge_client::{StarknetBridgeClient, StarknetBridgeClientBuilder};
use tracing::{info, warn};
use utxo_bridge_client::{AuthOptions, UTXOBridgeClient};
use wormhole_bridge_client::{WormholeBridgeClient, WormholeBridgeClientBuilder};

use crate::config::{self};

pub mod active_utxo_manager;
#[cfg(any(feature = "nats-ingestion", feature = "mongo-ingestion"))]
mod event_handlers;
pub mod evm_fee_bumping;
#[cfg(feature = "mongo-ingestion")]
pub mod mongo_ingestion;
#[cfg(feature = "native-indexers")]
pub mod native_indexers;
#[cfg(feature = "nats-ingestion")]
pub mod nats_ingestion;
pub mod utxo_lc_poller;

#[macro_export]
macro_rules! skip_fail {
    ($res:expr, $msg:expr, $dur:expr) => {
        match $res {
            Ok(val) => val,
            Err(err) => {
                error!("{}: {}", $msg, err);
                tokio::time::sleep(tokio::time::Duration::from_secs($dur)).await;
                continue;
            }
        }
    };
}

fn build_utxo_bridges(
    config: &config::Config,
    near_signer: &InMemorySigner,
) -> HashMap<ChainKind, UTXOChainAccounts> {
    let mut utxo_bridges = HashMap::new();

    for (chain, connector, token) in [
        (
            ChainKind::Btc,
            config.near.btc_connector.as_ref(),
            config.near.btc.as_ref(),
        ),
        (
            ChainKind::Zcash,
            config.near.zcash_connector.as_ref(),
            config.near.zcash.as_ref(),
        ),
    ] {
        utxo_bridges.insert(
            chain,
            UTXOChainAccounts {
                utxo_chain_connector: connector.cloned(),
                utxo_chain_token: token.cloned(),
                satoshi_relayer: Some(near_signer.account_id.clone()),
            },
        );
    }

    utxo_bridges
}

fn build_near_bridge_client(
    config: &config::Config,
    near_signer: &InMemorySigner,
) -> Result<near_bridge_client::NearBridgeClient> {
    NearBridgeClientBuilder::default()
        .endpoint(Some(config.near.rpc_url.clone()))
        .private_key(Some(near_signer.secret_key.to_string()))
        .signer(Some(near_signer.account_id.clone()))
        .omni_bridge_id(Some(config.near.omni_bridge_id.clone()))
        .mpc_omni_prover_id(config.near.mpc_omni_prover_id.clone())
        .utxo_bridges(build_utxo_bridges(config, near_signer))
        .bridge_indexer_api_url(
            config
                .bridge_indexer
                .api_url
                .as_ref()
                .map(|url| url.parse().unwrap()),
        )
        .build()
        .context("Failed to build NearBridgeClient")
}

fn build_evm_bridge_client(
    config: &config::Config,
    chain_kind: ChainKind,
    mpc_finalities: Option<&HashMap<ChainKind, MpcFinality>>,
) -> Result<Option<EvmBridgeClient>> {
    let evm = match chain_kind {
        ChainKind::Eth => &config.eth,
        ChainKind::Base => &config.base,
        ChainKind::Arb => &config.arb,
        ChainKind::Bnb => &config.bnb,
        ChainKind::Pol => &config.pol,
        ChainKind::HyperEvm => &config.hyperevm,
        ChainKind::Abs => &config.abs,
        ChainKind::Near
        | ChainKind::Sol
        | ChainKind::Fogo
        | ChainKind::Strk
        | ChainKind::Btc
        | ChainKind::Zcash => {
            unreachable!("Function `build_evm_bridge_client` supports only EVM chains")
        }
    };

    let evm_finality = mpc_finalities
        .as_ref()
        .and_then(|mpc_finalities| mpc_finalities.get(&chain_kind).cloned())
        .and_then(|mpc_finality| {
            if let MpcFinality::Evm(evm_finality) = mpc_finality {
                Some(evm_finality)
            } else {
                None
            }
        });

    evm.as_ref()
        .map(|evm| {
            EvmBridgeClientBuilder::default()
                .endpoint(Some(evm.rpc_http_url.clone()))
                .private_key(Some(crate::config::get_private_key(chain_kind, None)))
                .omni_bridge_address(Some(evm.omni_bridge_address.to_string()))
                .wormhole_core_address(evm.wormhole_address.map(|address| address.to_string()))
                .mpc_finality(evm_finality)
                .build()
                .context(format!("Failed to build EvmBridgeClient ({chain_kind:?})"))
        })
        .transpose()
}

fn build_hypercore_bridge_client(
    config: &config::Config,
) -> Result<Option<HyperCoreBridgeClient>> {
    let Some(hypercore) = config.hypercore.as_ref() else {
        return Ok(None);
    };
    let hyperevm = config.hyperevm.as_ref().context(
        "`hypercore` config section requires `hyperevm` to be configured (its RPC is used for `CoreReceived` polling)",
    )?;

    let network = match config.near.network {
        config::Network::Mainnet => HyperliquidNetwork::Mainnet,
        config::Network::Testnet => HyperliquidNetwork::Testnet,
    };

    HyperCoreBridgeClientBuilder::default()
        .network(network)
        .api_url(Some(hypercore.api_url.clone()))
        .hyperevm_rpc_url(Some(hyperevm.rpc_http_url.clone()))
        .private_key(Some(crate::config::get_private_key(
            ChainKind::HyperEvm,
            None,
        )))
        .build()
        .context("Failed to build HyperCoreBridgeClient")
        .map(Some)
}

fn build_svm_bridge_client(
    svm: Option<&config::Solana>,
    chain_kind: ChainKind,
) -> Result<Option<SolanaBridgeClient>> {
    svm.map(|svm| {
        SolanaBridgeClientBuilder::default()
            .chain(Some(chain_kind))
            .client(Some(RpcClient::new(svm.rpc_http_url.clone())))
            .program_id(Some(svm.program_id.parse()?))
            .wormhole_core(Some(svm.wormhole_id.parse()?))
            .wormhole_post_message_shim_program_id(Some(svm.wormhole_post_message_shim_id.parse()?))
            .wormhole_post_message_shim_event_authority(Some(
                svm.wormhole_post_message_shim_event_authority.parse()?,
            ))
            .keypair(Some(crate::utils::solana::get_keypair(
                svm.credentials_path.as_ref(),
                chain_kind,
            )))
            .build()
            .with_context(|| format!("Failed to build {chain_kind:?} bridge client"))
    })
    .transpose()
}

fn build_starknet_bridge_client(
    config: &config::Config,
    mpc_finalities: Option<&HashMap<ChainKind, MpcFinality>>,
) -> Result<Option<StarknetBridgeClient>> {
    let starknet_finality = mpc_finalities
        .as_ref()
        .and_then(|mpc_finalities| mpc_finalities.get(&ChainKind::Strk).cloned())
        .and_then(|mpc_finality| {
            if let MpcFinality::Starknet(starknet_finality) = mpc_finality {
                Some(starknet_finality)
            } else {
                None
            }
        });

    config
        .starknet
        .as_ref()
        .map(|starknet| {
            StarknetBridgeClientBuilder::default()
                .endpoint(Some(starknet.rpc_http_url.clone()))
                .private_key(Some(crate::config::get_private_key(ChainKind::Strk, None)))
                .account_address(Some(crate::config::get_relayer_starknet_address()))
                .omni_bridge_address(Some(starknet.omni_bridge_address.clone()))
                .chain_id(Some(starknet.chain_id.clone()))
                .mpc_finality(starknet_finality)
                .build()
                .context("Failed to build StarknetBridgeClient")
        })
        .transpose()
}

fn build_utxo_bridge_client<C: utxo_bridge_client::types::UTXOChain>(
    config: &config::Config,
    chain: ChainKind,
) -> Result<Option<UTXOBridgeClient<C>>> {
    let utxo = match chain {
        ChainKind::Btc => &config.btc,
        ChainKind::Zcash => &config.zcash,
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
        | ChainKind::Strk => {
            anyhow::bail!("Chain {chain:?} is not supported for building UTXO bridge client")
        }
    };

    Ok(utxo
        .as_ref()
        .map(|utxo| UTXOBridgeClient::new(utxo.rpc_http_url.clone(), AuthOptions::None)))
}

fn build_wormhole_bridge_client(config: &config::Config) -> Result<WormholeBridgeClient> {
    WormholeBridgeClientBuilder::default()
        .endpoint(Some(config.wormhole.api_url.clone()))
        .build()
        .context("Failed to build WormholeBridgeClient")
}

fn build_light_client(config: &config::Config, chain: ChainKind) -> Result<Option<LightClient>> {
    let light_client = match chain {
        ChainKind::Eth => config.eth.as_ref().and_then(|eth| eth.light_client.clone()),
        ChainKind::Btc => config.btc.as_ref().map(|btc| btc.light_client.clone()),
        ChainKind::Zcash => config
            .zcash
            .as_ref()
            .map(|zcash| zcash.light_client.clone()),
        ChainKind::Near
        | ChainKind::Base
        | ChainKind::Arb
        | ChainKind::Bnb
        | ChainKind::Pol
        | ChainKind::HyperEvm
        | ChainKind::Abs
        | ChainKind::Sol
        | ChainKind::Fogo
        | ChainKind::Strk => {
            anyhow::bail!("Chain {chain:?} is not supported for building light client")
        }
    };

    light_client
        .as_ref()
        .map(|light_client| {
            LightClientBuilder::default()
                .endpoint(Some(config.near.rpc_url.clone()))
                .chain(Some(chain))
                .light_client_id(Some(light_client.clone()))
                .build()
                .context("Failed to build EthLightClient")
        })
        .transpose()
}

pub async fn build_omni_connector(
    config: &config::Config,
    near_signer: &InMemorySigner,
) -> Result<OmniConnector> {
    info!("Building Omni connector");

    let near_bridge_client = build_near_bridge_client(config, near_signer)?;
    let mpc_finalities = match near_bridge_client.get_mpc_finalities().await {
        Ok(mpc_finalities) => Some(mpc_finalities),
        Err(err) => {
            warn!("Failed to fetch mpc finalities: {err:?}");
            None
        }
    };
    let eth_bridge_client =
        build_evm_bridge_client(config, ChainKind::Eth, mpc_finalities.as_ref())?;
    let base_bridge_client =
        build_evm_bridge_client(config, ChainKind::Base, mpc_finalities.as_ref())?;
    let arb_bridge_client =
        build_evm_bridge_client(config, ChainKind::Arb, mpc_finalities.as_ref())?;
    let bnb_bridge_client =
        build_evm_bridge_client(config, ChainKind::Bnb, mpc_finalities.as_ref())?;
    let pol_bridge_client =
        build_evm_bridge_client(config, ChainKind::Pol, mpc_finalities.as_ref())?;
    let hyperevm_bridge_client =
        build_evm_bridge_client(config, ChainKind::HyperEvm, mpc_finalities.as_ref())?;
    let abs_bridge_client =
        build_evm_bridge_client(config, ChainKind::Abs, mpc_finalities.as_ref())?;
    let hypercore_bridge_client = build_hypercore_bridge_client(config)?;
    let solana_bridge_client = build_svm_bridge_client(config.solana.as_ref(), ChainKind::Sol)?;
    let fogo_bridge_client = build_svm_bridge_client(config.fogo.as_ref(), ChainKind::Fogo)?;
    let starknet_bridge_client = build_starknet_bridge_client(config, mpc_finalities.as_ref())?;
    let btc_bridge_client = build_utxo_bridge_client(config, ChainKind::Btc)?;
    let zcash_bridge_client = build_utxo_bridge_client(config, ChainKind::Zcash)?;
    let wormhole_bridge_client = build_wormhole_bridge_client(config)?;
    let eth_light_client = build_light_client(config, ChainKind::Eth)?;
    let btc_light_client = build_light_client(config, ChainKind::Btc)?;
    let zcash_light_client = build_light_client(config, ChainKind::Zcash)?;

    let omni_connector = OmniConnectorBuilder::default()
        .network(Some(config.near.network.into()))
        .near_bridge_client(Some(near_bridge_client))
        .eth_bridge_client(eth_bridge_client)
        .base_bridge_client(base_bridge_client)
        .arb_bridge_client(arb_bridge_client)
        .bnb_bridge_client(bnb_bridge_client)
        .pol_bridge_client(pol_bridge_client)
        .hyperevm_bridge_client(hyperevm_bridge_client)
        .abs_bridge_client(abs_bridge_client)
        .hypercore_bridge_client(hypercore_bridge_client)
        .solana_bridge_client(solana_bridge_client)
        .fogo_bridge_client(fogo_bridge_client)
        .starknet_bridge_client(starknet_bridge_client)
        .wormhole_bridge_client(Some(wormhole_bridge_client))
        .btc_bridge_client(btc_bridge_client)
        .zcash_bridge_client(zcash_bridge_client)
        .enable_orchard(config.orchard.as_ref().map(|orchard| orchard.enabled))
        .eth_light_client(eth_light_client)
        .btc_light_client(btc_light_client)
        .zcash_light_client(zcash_light_client)
        .build()
        .context("Failed to build OmniConnector")?;

    Ok(omni_connector)
}
