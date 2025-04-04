use std::str::FromStr;

use alloy::rpc::types::Log;

use super::{
    chain::PegInEvent, chain::PegOutBurntEvent, chain::PegOutEvent, chain_adaptor::ChainAdaptor,
};
use alloy::sol_types::SolEvent;
use alloy::{
    eips::BlockNumberOrTag,
    primitives::Address as EvmAddress,
    providers::{Provider, ProviderBuilder, RootProvider},
    rpc::types::Filter,
    sol,
    transports::http::{reqwest::Url, Client, Http},
};
use async_trait::async_trait;
use bitcoin::hashes::Hash;
use bitcoin::{Address, Amount, Denomination, OutPoint, PublicKey, Txid};
use dotenv;

sol!(
    #[derive(Debug)]
    #[allow(missing_docs)]
    #[sol(rpc)]
    interface IBridge {
        struct Outpoint {
            bytes32 txId;
            uint256 vOut;
        }
        event PegOutInitiated(
            address indexed withdrawer,
            string destination_address,
            Outpoint source_outpoint,
            uint256 amount,
            bytes operator_pubKey
        );
        event PegOutBurnt(
            address indexed withdrawer,
            Outpoint source_outpoint,
            uint256 amount,
            bytes operator_pubKey
        );
        event PegInMinted(
            address indexed depositor,
            uint256 amount,
            bytes32 depositorPubKey
        );
    }
);

pub struct EthereumAdaptor {
    bridge_address: EvmAddress,
    bridge_creation_block: u64,
    provider: RootProvider<Http<Client>>,
    to_block: Option<BlockNumberOrTag>,
}

pub struct EthereumInitConfig {
    pub rpc_url: Url,
    pub bridge_address: EvmAddress,
    pub bridge_creation_block: u64,
    pub to_block: Option<BlockNumberOrTag>,
}

impl EthereumAdaptor {
    async fn get_sol_events<T>(&self) -> Result<Vec<Log<T>>, String>
    where
        T: SolEvent,
    {
        let mut filter = Filter::new()
            .from_block(BlockNumberOrTag::Number(self.bridge_creation_block))
            .address(self.bridge_address)
            .event(T::SIGNATURE);
        filter = match self.to_block.is_none() {
            true => filter.to_block(BlockNumberOrTag::Finalized),
            false => filter.to_block(self.to_block.unwrap()),
        };

        let results = self.provider.get_logs(&filter).await;
        if let Err(rpc_error) = results {
            return Err(rpc_error.to_string());
        }
        let logs = results.unwrap();
        let mut sol_events: Vec<Log<T>> = Vec::new();
        for log in logs {
            let decoded = log.log_decode::<T>();
            if let Err(error) = decoded {
                return Err(error.to_string());
            }
            sol_events.push(decoded.unwrap());
        }

        Ok(sol_events)
    }
}

#[async_trait]
impl ChainAdaptor for EthereumAdaptor {
    async fn get_peg_out_init_event(&self) -> Result<Vec<PegOutEvent>, String> {
        let sol_events = self.get_sol_events::<IBridge::PegOutInitiated>().await;
        if sol_events.is_err() {
            return Err(sol_events.unwrap_err().to_string());
        }

        let peg_out_init_events = sol_events
            .unwrap()
            .iter()
            .filter_map(|e| {
                let withdrawer_address = Address::from_str(&e.inner.data.destination_address)
                    .unwrap()
                    .assume_checked();
                let operator_public_key =
                    PublicKey::from_slice(e.inner.data.operator_pubKey.as_ref()).unwrap();
                match withdrawer_address.pubkey_hash() {
                    Some(withdrawer_public_key_hash) => {
                        let mut txid_vec = e.inner.data.source_outpoint.txId.to_vec();
                        txid_vec.reverse();
                        Some(PegOutEvent {
                            withdrawer_chain_address: e.inner.data.withdrawer.to_string(),
                            withdrawer_destination_address: e
                                .inner
                                .data
                                .destination_address
                                .to_string(),
                            withdrawer_public_key_hash,
                            source_outpoint: OutPoint {
                                txid: Txid::from_slice(&txid_vec).unwrap(),
                                vout: e.inner.data.source_outpoint.vOut.to::<u32>(),
                            },
                            amount: Amount::from_str_in(
                                e.inner.data.amount.to_string().as_str(),
                                Denomination::Satoshi,
                            )
                            .unwrap(),
                            operator_public_key,
                            timestamp: u32::try_from(e.block_timestamp.unwrap()).unwrap(),
                            tx_hash: e.transaction_hash.unwrap().to_vec(),
                        })
                    }
                    None => None,
                }
            })
            .collect();

        Ok(peg_out_init_events)
    }

    async fn get_peg_out_burnt_event(&self) -> Result<Vec<PegOutBurntEvent>, String> {
        let sol_events = self.get_sol_events::<IBridge::PegOutBurnt>().await;
        if sol_events.is_err() {
            return Err(sol_events.unwrap_err().to_string());
        }

        let peg_out_burnt_events = sol_events
            .unwrap()
            .iter()
            .map(|e| {
                let operator_public_key =
                    PublicKey::from_slice(e.inner.data.operator_pubKey.as_ref()).unwrap();
                PegOutBurntEvent {
                    withdrawer_chain_address: e.inner.data.withdrawer.to_string(),
                    source_outpoint: OutPoint {
                        txid: Txid::from_slice(e.inner.data.source_outpoint.txId.as_ref()).unwrap(),
                        vout: e.inner.data.source_outpoint.vOut.to::<u32>(),
                    },
                    amount: Amount::from_str_in(
                        e.inner.data.amount.to_string().as_str(),
                        Denomination::Satoshi,
                    )
                    .unwrap(),
                    operator_public_key,
                    timestamp: u32::try_from(e.block_timestamp.unwrap()).unwrap(),
                    tx_hash: e.transaction_hash.unwrap().to_vec(),
                }
            })
            .collect();

        Ok(peg_out_burnt_events)
    }

    async fn get_peg_in_minted_event(&self) -> Result<Vec<PegInEvent>, String> {
        let sol_events = self.get_sol_events::<IBridge::PegInMinted>().await;
        if sol_events.is_err() {
            return Err(sol_events.unwrap_err().to_string());
        }

        let peg_in_minted_events = sol_events
            .unwrap()
            .iter()
            .map(|e| PegInEvent {
                depositor: e.inner.data.depositor.to_string(),
                amount: Amount::from_str_in(
                    e.inner.data.amount.to_string().as_str(),
                    Denomination::Satoshi,
                )
                .unwrap(),
                depositor_pubkey: PublicKey::from_slice(e.inner.data.depositorPubKey.as_ref())
                    .unwrap(),
            })
            .collect();

        Ok(peg_in_minted_events)
    }
}

impl EthereumAdaptor {
    pub fn new(config: Option<EthereumInitConfig>) -> Self {
        if let Some(_config) = config {
            Self::from_config(_config)
        } else {
            dotenv::dotenv().ok();
            let rpc_url_str = dotenv::var("BRIDGE_CHAIN_ADAPTOR_ETHEREUM_RPC_URL")
                .expect("Failed to read BRIDGE_CHAIN_ADAPTOR_ETHEREUM_RPC_URL variable");
            let bridge_address_str = dotenv::var("BRIDGE_CHAIN_ADAPTOR_ETHEREUM_BRIDGE_ADDRESS")
                .expect("Failed to read BRIDGE_CHAIN_ADAPTOR_ETHEREUM_BRIDGE_ADDRESS variable");
            let bridge_creation = dotenv::var("BRIDGE_CHAIN_ADAPTOR_ETHEREUM_BRIDGE_CREATION")
                .expect("Failed to read BRIDGE_CHAIN_ADAPTOR_ETHEREUM_BRIDGE_CREATION variable");
            let to_block = dotenv::var("BRIDGE_CHAIN_ADAPTOR_ETHEREUM_TO_BLOCK");

            let rpc_url = rpc_url_str.parse::<Url>();
            let bridge_address = bridge_address_str.parse::<EvmAddress>();
            Self::from_config(EthereumInitConfig {
                rpc_url: rpc_url.unwrap(),
                bridge_address: bridge_address.unwrap(),
                bridge_creation_block: bridge_creation.parse::<u64>().unwrap(),
                to_block: match to_block {
                    Ok(block) => Some(BlockNumberOrTag::from_str(block.as_str()).unwrap()),
                    Err(_) => Some(BlockNumberOrTag::Finalized),
                },
            })
        }
    }

    fn from_config(config: EthereumInitConfig) -> Self {
        Self {
            bridge_address: config.bridge_address,
            bridge_creation_block: config.bridge_creation_block,
            provider: ProviderBuilder::new().on_http(config.rpc_url),
            to_block: config.to_block,
        }
    }
}
