use std::{borrow::Cow, collections::BTreeMap, path::Path, str::FromStr, time::Duration};

use ark_bn254::g1::G1Affine;
use ark_std::{test_rng, UniformRand};

use bitcoin::hashes::hash160::Hash;
use bitcoin::{
    block::{Header, Version},
    hex::{Case::Lower, DisplayHex},
    Address, Amount, BlockHash, CompactTarget, Network, OutPoint, Transaction, TxMerkleNode,
};
use bitcoin::{PubkeyHash, PublicKey, Txid};

use bitvm::chunk::api::type_conversion_utils::RawProof;
use bitvm::chunk::api::{NUM_HASH, NUM_PUBS, NUM_U256};
use bitvm::signatures::wots_api;
use bridge::client::chain::chain::PegOutEvent;
use bridge::client::data_store::local_file::TEST_DATA_DIRECTORY_NAME;
use bridge::proof::get_proof;
use bridge::{
    client::client::BitVMClient,
    graphs::{
        base::{BaseGraph, REWARD_MULTIPLIER, REWARD_PRECISION},
        peg_in::PegInGraph,
        peg_out::PegOutGraph,
    },
    utils::{num_blocks_per_network, read_disk_cache, write_disk_cache},
};

use colored::Colorize;
use rand::{RngCore, SeedableRng};
use tokio::time::sleep;

// Test environment config file and its variables
const TEST_ENV_FILE: &str = ".env.test";
const REGTEST_BLOCK_TIME: &str = "REGTEST_BLOCK_TIME";

fn load_u32_env_var_from_file(var: &str, file_name: &str) -> u32 {
    dotenv::from_filename(file_name)
        .expect(format!("Please create a {file_name} file with the {var} variable").as_str());
    dotenv::var(var)
        .expect(format!("{var} variable missing in {file_name}").as_str())
        .parse()
        .expect(format!("Could not parse {var} specified in {file_name}").as_str())
}

/// Returns expected block time for the given network in seconds.
fn network_block_time(network: Network) -> u32 {
    match network {
        Network::Regtest => load_u32_env_var_from_file(REGTEST_BLOCK_TIME, TEST_ENV_FILE),
        _ => 35, // Testnet, signet. This value is for Alpen signet. See https://mempool0713bb23.devnet-annapurna.stratabtc.org/
    }
}

/// Provides a safe waiting duration in seconds for transaction confirmation on the specified network.
/// This duration must be at least as long as the expected block time for that network.
/// Returns network block time + 1 second to avoid race conditions.
fn tx_wait_time(network: Network) -> u64 {
    (network_block_time(network) + 1).into()
}

pub const TX_RELAY_FEE_CHECK_FAIL_MSG: &str =
    "Output sum should be equal to initial amount, check MIN_RELAY_FEE_* definitions?";

pub fn check_tx_output_sum(input_amount_without_relay_fee: u64, tx: &Transaction) {
    assert_eq!(
        input_amount_without_relay_fee,
        tx.output.iter().map(|o| o.value.to_sat()).sum::<u64>(),
        "{TX_RELAY_FEE_CHECK_FAIL_MSG}"
    );
}

pub fn get_reward_amount(initial_amount: u64) -> u64 {
    initial_amount * REWARD_MULTIPLIER / REWARD_PRECISION
}

const DURATION_COLOR: &str = "\x1b[30;46m"; // Black on cyan background
const RESET_COLOR: &str = "\x1b[0m";

async fn wait_with_message(timeout: Duration, message: &str) {
    println!(
        "Waiting {DURATION_COLOR}{:?}{RESET_COLOR}{}...",
        timeout, message
    );
    sleep(timeout).await;
}

pub async fn wait_for_confirmation_with_message(network: Network, message: Option<&str>) {
    let timeout = Duration::from_secs(tx_wait_time(network));
    let message = format!(" for {}", message.unwrap_or("tx confirmation"));

    wait_with_message(timeout, message.as_str()).await;
}

pub async fn wait_for_confirmation(network: Network) {
    wait_for_confirmation_with_message(network, None).await;
}

pub async fn wait_for_timelock_expiry(network: Network, timelock_name: Option<&str>) {
    // Note that the extra 1 second from tx_wait_time() compounds here. Normally this will not be an issue.
    // You'll just wait a couple seconds longer than the required number of blocks. However, if you need to
    // wait for an exact number of seconds, consider using a simple sleep (or adding a sister helper function).
    let tx_wait_time = tx_wait_time(network);
    let timeout = Duration::from_secs(
        tx_wait_time * num_blocks_per_network(network, 0) as u64 + tx_wait_time,
    );
    let message = format!(
        " for{} timelock to expire",
        match timelock_name {
            Some(timelock_name) => format!(" {}", timelock_name),
            None => String::new(),
        }
    );

    wait_with_message(timeout, message.as_str()).await;
}

pub async fn generate_stub_outpoint(
    client: &BitVMClient,
    funding_utxo_address: &Address,
    input_value: Amount,
) -> OutPoint {
    let funding_utxo = client
        .get_initial_utxo(funding_utxo_address.clone(), input_value)
        .await
        .unwrap_or_else(|| {
            panic!(
                "Fund {:?} with {} sats at {}",
                funding_utxo_address,
                input_value.to_sat(),
                client.esplora.url(),
            );
        });
    OutPoint {
        txid: funding_utxo.txid,
        vout: funding_utxo.vout,
    }
}

pub async fn generate_stub_outpoints(
    client: &BitVMClient,
    funding_utxo_address: &Address,
    input_value: Amount,
) -> Vec<OutPoint> {
    let funding_utxos = client
        .get_initial_utxos(funding_utxo_address.clone(), input_value)
        .await
        .unwrap_or_else(|| {
            panic!(
                "Fund {:?} with {} sats at {}",
                funding_utxo_address,
                input_value.to_sat(),
                client.esplora.url(),
            );
        });
    funding_utxos
        .iter()
        .map(|utxo| OutPoint {
            txid: utxo.txid,
            vout: utxo.vout,
        })
        .collect()
}

pub async fn verify_funding_inputs(client: &BitVMClient, funding_inputs: &Vec<(&Address, Amount)>) {
    let mut inputs_to_fund: Vec<(&Address, Amount)> = vec![];

    for funding_input in funding_inputs {
        if client
            .get_initial_utxo(funding_input.0.clone(), funding_input.1)
            .await
            .is_none()
        {
            inputs_to_fund.push((funding_input.0, funding_input.1));
        }
    }

    for input_to_fund in inputs_to_fund.clone() {
        println!(
            "Fund {:?} with {} sats at {}",
            input_to_fund.0,
            input_to_fund.1.to_sat(),
            client.esplora.url(),
        );
    }
    if !inputs_to_fund.is_empty() {
        panic!("You need to fund {} addresses first.", inputs_to_fund.len());
    }
}

pub fn find_peg_in_graph(client: &BitVMClient, peg_in_graph_id: &str) -> Option<PegInGraph> {
    let peg_in_graph = client
        .data()
        .peg_in_graphs
        .iter()
        .find(|&graph| graph.id().eq(peg_in_graph_id));

    peg_in_graph.cloned()
}

pub fn find_peg_out_graph(client: &BitVMClient, peg_out_graph_id: &str) -> Option<PegOutGraph> {
    let peg_out_graph = client
        .data()
        .peg_out_graphs
        .iter()
        .find(|&graph| graph.id().eq(&peg_out_graph_id));

    peg_out_graph.cloned()
}

pub fn find_peg_in_graph_by_peg_out(
    client: &BitVMClient,
    peg_out_graph_id: &str,
) -> Option<PegInGraph> {
    let peg_out_graph = find_peg_out_graph(client, peg_out_graph_id);
    match peg_out_graph {
        Some(peg_out_graph) => find_peg_in_graph(client, &peg_out_graph.peg_in_graph_id),
        None => None,
    }
}

pub fn get_superblock_header() -> Header {
    Header {
        version: Version::from_consensus(0x200d2000),
        prev_blockhash: BlockHash::from_str(
            "000000000000000000027c9f5b07f21e39ba31aa4d900d519478bdac32f4a15d",
        )
        .unwrap(),
        merkle_root: TxMerkleNode::from_str(
            "0064b0d54f20412756ba7ce07b0594f3548b06f2dad5cfeaac2aca508634ed19",
        )
        .unwrap(),
        time: 1729251961,
        bits: CompactTarget::from_hex("0x17030ecd").unwrap(),
        nonce: 0x400e345c,
    }
}

pub fn get_default_peg_out_event() -> PegOutEvent {
    PegOutEvent {
        withdrawer_chain_address: "".to_string(),
        withdrawer_destination_address: "".to_string(),
        withdrawer_public_key_hash: PubkeyHash::from_raw_hash(
            Hash::from_str("0e6719ac074b0e3cac76d057643506faa1c266b3").unwrap(),
        ),
        source_outpoint: OutPoint {
            txid: Txid::from_str(
                "0e6719ac074b0e3cac76d057643506faa1c266b322aa9cf4c6f635fe63b14327",
            )
            .unwrap(),
            vout: 0,
        },
        amount: Amount::from_sat(0),
        operator_public_key: PublicKey::from_str(
            "03484db4a2950d63da8455a1b705b39715e4075dd33511d0c7e3ce308c93449deb",
        )
        .unwrap(),
        timestamp: 0,
        tx_hash: vec![],
    }
}

pub fn random_hex<'a>(size: usize) -> Cow<'a, str> {
    let mut buffer = vec![0u8; size];
    let mut rng = rand::rngs::OsRng;
    rand::RngCore::fill_bytes(&mut rng, &mut buffer);
    Cow::Owned(buffer.to_hex_string(Lower))
}

const INTERMEDIATE_VARIABLES_FILE_NAME: &str = "intermediates.bin";
const TEST_CACHE_DIRECTORY_NAME: &str = "test_cache";

pub fn get_intermediate_variables_cached() -> BTreeMap<String, usize> {
    let intermediate_variables_cache_path = Path::new(TEST_DATA_DIRECTORY_NAME)
        .join(TEST_CACHE_DIRECTORY_NAME)
        .join(INTERMEDIATE_VARIABLES_FILE_NAME);
    let intermediate_variables = if intermediate_variables_cache_path.exists() {
        read_disk_cache(&intermediate_variables_cache_path)
            .inspect_err(|e| {
                eprintln!(
                    "Failed to read intermediate variables cache after validates its existence: {}",
                    e
                );
            })
            .ok()
    } else {
        None
    };

    intermediate_variables.unwrap_or_else(|| {
        println!("Generating new intermediate variables...");
        let mut intermediate_variables: BTreeMap<String, usize> = BTreeMap::new();
        for i in 0..NUM_PUBS {
            intermediate_variables.insert(format!("{}", i), 32);
        }
        for i in 0..NUM_U256 {
            intermediate_variables.insert(format!("{}", i + NUM_PUBS), 32);
        }
        for i in 0..NUM_HASH {
            intermediate_variables.insert(
                format!("{}", i + NUM_PUBS + NUM_U256),
                wots_api::HASH_LEN as usize,
            );
        }

        write_disk_cache(&intermediate_variables_cache_path, &intermediate_variables).unwrap();
        intermediate_variables
    })
}

pub fn get_valid_proof() -> RawProof {
    get_proof()
}

pub fn invalidate_proof(valid_proof: &RawProof) -> RawProof {
    let mut invalid_proof = valid_proof.clone();
    let mut rng = ark_std::rand::rngs::StdRng::seed_from_u64(test_rng().next_u64());
    invalid_proof.proof.a = G1Affine::rand(&mut rng);

    invalid_proof
}

pub fn print_tx_broadcasted(tx_name: &str, txid: Txid) {
    println!("Broadcasted {} with txid: {txid}", tx_name.bold().green(),);
}
