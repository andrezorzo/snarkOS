// Copyright (C) 2019-2023 Aleo Systems Inc.
// This file is part of the snarkOS library.

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at:
// http://www.apache.org/licenses/LICENSE-2.0

// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#[macro_use]
extern crate tracing;

use snarkos_account::Account;
use snarkos_node_narwhal::{
    helpers::{init_primary_channels, PrimarySender, Storage},
    Primary,
    BFT,
    MAX_GC_ROUNDS,
    MEMORY_POOL_PORT,
};
use snarkos_node_narwhal_committee::Committee;
use snarkos_node_narwhal_ledger_service::MockLedgerService;
use snarkvm::{
    ledger::narwhal::Data,
    prelude::{
        block::Transaction,
        coinbase::{ProverSolution, PuzzleCommitment},
        Field,
        Network,
        Uniform,
    },
};

use ::bytes::Bytes;
use anyhow::{anyhow, ensure, Error, Result};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
    Router,
};
use axum_extra::response::ErasedJson;
use clap::{Parser, ValueEnum};
use indexmap::IndexMap;
use parking_lot::RwLock;
use rand::{Rng, SeedableRng};
use std::{collections::HashMap, net::SocketAddr, path::PathBuf, str::FromStr, sync::Arc};
use tokio::sync::oneshot;
use tracing_subscriber::{
    layer::{Layer, SubscriberExt},
    util::SubscriberInitExt,
};

type CurrentNetwork = snarkvm::prelude::Testnet3;

/**************************************************************************************************/

/// Initializes the logger.
pub fn initialize_logger(verbosity: u8) {
    match verbosity {
        0 => std::env::set_var("RUST_LOG", "info"),
        1 => std::env::set_var("RUST_LOG", "debug"),
        2 | 3 | 4 => std::env::set_var("RUST_LOG", "trace"),
        _ => std::env::set_var("RUST_LOG", "info"),
    };

    // Filter out undesirable logs. (unfortunately EnvFilter cannot be cloned)
    let [filter] = std::array::from_fn(|_| {
        let filter = tracing_subscriber::EnvFilter::from_default_env()
            .add_directive("mio=off".parse().unwrap())
            .add_directive("tokio_util=off".parse().unwrap())
            .add_directive("hyper=off".parse().unwrap())
            .add_directive("reqwest=off".parse().unwrap())
            .add_directive("want=off".parse().unwrap())
            .add_directive("snarkos_node_narwhal::gateway=off".parse().unwrap())
            .add_directive("warp=off".parse().unwrap());

        if verbosity > 3 {
            filter.add_directive("snarkos_node_tcp=trace".parse().unwrap())
        } else {
            filter.add_directive("snarkos_node_tcp=off".parse().unwrap())
        }
    });

    // Initialize tracing.
    let _ = tracing_subscriber::registry()
        .with(tracing_subscriber::fmt::Layer::default().with_target(verbosity > 2).with_filter(filter))
        .try_init();
}

/**************************************************************************************************/

/// Starts the BFT instance.
pub async fn start_bft(
    node_id: u16,
    num_nodes: u16,
    peers: HashMap<u16, SocketAddr>,
) -> Result<(BFT<CurrentNetwork>, PrimarySender<CurrentNetwork>)> {
    // Initialize the primary channels.
    let (sender, receiver) = init_primary_channels();
    // Initialize the components.
    let (storage, account) = initialize_components(node_id, num_nodes)?;
    // Initialize the mock ledger service.
    let ledger = Arc::new(MockLedgerService::new());
    // Initialize the gateway IP and dev mode.
    let (ip, dev) = match peers.get(&node_id) {
        Some(ip) => (Some(*ip), None),
        None => (None, Some(node_id)),
    };
    // Initialize the BFT instance.
    let mut bft = BFT::<CurrentNetwork>::new(account, storage, ledger, ip, dev)?;
    // Run the BFT instance.
    bft.run(sender.clone(), receiver, None).await?;
    // Retrieve the BFT's primary.
    let primary = bft.primary();
    // Keep the node's connections.
    keep_connections(primary, node_id, num_nodes, peers);
    // Handle the log connections.
    log_connections(primary);
    // Handle OS signals.
    handle_signals(primary);
    // Return the BFT instance.
    Ok((bft, sender))
}

/// Starts the primary instance.
pub async fn start_primary(
    node_id: u16,
    num_nodes: u16,
    peers: HashMap<u16, SocketAddr>,
) -> Result<(Primary<CurrentNetwork>, PrimarySender<CurrentNetwork>)> {
    // Initialize the primary channels.
    let (sender, receiver) = init_primary_channels();
    // Initialize the components.
    let (storage, account) = initialize_components(node_id, num_nodes)?;
    // Initialize the mock ledger service.
    let ledger = Arc::new(MockLedgerService::new());
    // Initialize the gateway IP and dev mode.
    let (ip, dev) = match peers.get(&node_id) {
        Some(ip) => (Some(*ip), None),
        None => (None, Some(node_id)),
    };
    // Initialize the primary instance.
    let mut primary = Primary::<CurrentNetwork>::new(account, storage, ledger, ip, dev)?;
    // Run the primary instance.
    primary.run(sender.clone(), receiver, None).await?;
    // Keep the node's connections.
    keep_connections(&primary, node_id, num_nodes, peers);
    // Handle the log connections.
    log_connections(&primary);
    // Handle OS signals.
    handle_signals(&primary);
    // Return the primary instance.
    Ok((primary, sender))
}

/// Initializes the components of the node.
fn initialize_components(node_id: u16, num_nodes: u16) -> Result<(Storage<CurrentNetwork>, Account<CurrentNetwork>)> {
    // Ensure that the node ID is valid.
    ensure!(node_id < num_nodes, "Node ID {node_id} must be less than {num_nodes}");

    // Sample a account.
    let account = Account::new(&mut rand_chacha::ChaChaRng::seed_from_u64(node_id as u64))?;
    println!("\n{account}\n");

    // Initialize a map for the committee members.
    let mut members = IndexMap::with_capacity(num_nodes as usize);
    // Add the validators as members.
    for i in 0..num_nodes {
        // Sample the account.
        let account = Account::new(&mut rand_chacha::ChaChaRng::seed_from_u64(i as u64))?;
        // Add the validator.
        members.insert(account.address(), 1000);
        println!("  Validator {}: {}", i, account.address());
    }
    println!();

    // Initialize the committee.
    let committee = Arc::new(RwLock::new(Committee::<CurrentNetwork>::new(1u64, members)?));
    // Initialize the storage.
    let storage = Storage::new(committee.read().clone(), MAX_GC_ROUNDS);
    // Return the storage and account.
    Ok((storage, account))
}

/// Actively try to keep the node's connections to all nodes.
fn keep_connections(primary: &Primary<CurrentNetwork>, node_id: u16, num_nodes: u16, peers: HashMap<u16, SocketAddr>) {
    let node = primary.clone();
    tokio::task::spawn(async move {
        // Sleep briefly to ensure the other nodes are ready to connect.
        tokio::time::sleep(std::time::Duration::from_millis(100 * node_id as u64)).await;
        // Start the loop.
        loop {
            for i in 0..num_nodes {
                // Initialize the gateway IP.
                let ip = match peers.get(&i) {
                    Some(ip) => *ip,
                    None => SocketAddr::from_str(&format!("127.0.0.1:{}", MEMORY_POOL_PORT + i)).unwrap(),
                };
                // Check if the node is connected.
                if i != node_id && !node.gateway().is_connected(ip) {
                    // Connect to the node.
                    debug!("Connecting to {}...", ip);
                    node.gateway().connect(ip);
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
        }
    });
}

/// Logs the node's connections.
fn log_connections(primary: &Primary<CurrentNetwork>) {
    let node = primary.clone();
    tokio::task::spawn(async move {
        loop {
            let connections = node.gateway().connected_peers().read().clone();
            info!("{} connections", connections.len());
            for connection in connections {
                debug!("  {}", connection);
            }
            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
        }
    });
}

/// Handles OS signals for the node to intercept and perform a clean shutdown.
/// Note: Only Ctrl-C is supported; it should work on both Unix-family systems and Windows.
fn handle_signals(primary: &Primary<CurrentNetwork>) {
    let node = primary.clone();
    tokio::task::spawn(async move {
        match tokio::signal::ctrl_c().await {
            Ok(()) => {
                node.shut_down().await;
                std::process::exit(0);
            }
            Err(error) => error!("tokio::signal::ctrl_c encountered an error: {}", error),
        }
    });
}

/**************************************************************************************************/

/// Fires *fake* unconfirmed solutions at the node.
fn fire_unconfirmed_solutions(sender: &PrimarySender<CurrentNetwork>, node_id: u16, interval_ms: u64) {
    let tx_unconfirmed_solution = sender.tx_unconfirmed_solution.clone();
    tokio::task::spawn(async move {
        // This RNG samples the *same* fake solutions for all nodes.
        let mut shared_rng = rand_chacha::ChaChaRng::seed_from_u64(123456789);
        // This RNG samples *different* fake solutions for each node.
        let mut unique_rng = rand_chacha::ChaChaRng::seed_from_u64(node_id as u64);

        // A closure to generate a commitment and solution.
        fn sample(mut rng: impl Rng) -> (PuzzleCommitment<CurrentNetwork>, Data<ProverSolution<CurrentNetwork>>) {
            // Sample a random fake puzzle commitment.
            // TODO (howardwu): Use a mutex to bring in the real 'proof target' and change this sampling to a while loop.
            let commitment = PuzzleCommitment::<CurrentNetwork>::from_g1_affine(rng.gen());
            // Sample random fake solution bytes.
            let solution = Data::Buffer(Bytes::from((0..1024).map(|_| rng.gen::<u8>()).collect::<Vec<_>>()));
            // Return the ID and solution.
            (commitment, solution)
        }

        // Initialize a counter.
        let mut counter = 0;

        loop {
            // Sample a random fake puzzle commitment and solution.
            let (commitment, solution) =
                if counter % 2 == 0 { sample(&mut shared_rng) } else { sample(&mut unique_rng) };
            // Initialize a callback sender and receiver.
            let (callback, callback_receiver) = oneshot::channel();
            // Send the fake solution.
            if let Err(e) = tx_unconfirmed_solution.send((commitment, solution, callback)).await {
                error!("Failed to send unconfirmed solution: {e}");
            }
            let _ = callback_receiver.await;
            // Increment the counter.
            counter += 1;
            // Sleep briefly.
            tokio::time::sleep(std::time::Duration::from_millis(interval_ms)).await;
        }
    });
}

/// Fires *fake* unconfirmed transactions at the node.
fn fire_unconfirmed_transactions(sender: &PrimarySender<CurrentNetwork>, node_id: u16, interval_ms: u64) {
    let tx_unconfirmed_transaction = sender.tx_unconfirmed_transaction.clone();
    tokio::task::spawn(async move {
        // This RNG samples the *same* fake transactions for all nodes.
        let mut shared_rng = rand_chacha::ChaChaRng::seed_from_u64(123456789);
        // This RNG samples *different* fake transactions for each node.
        let mut unique_rng = rand_chacha::ChaChaRng::seed_from_u64(node_id as u64);

        // A closure to generate an ID and transaction.
        fn sample(
            mut rng: impl Rng,
        ) -> (<CurrentNetwork as Network>::TransactionID, Data<Transaction<CurrentNetwork>>) {
            // Sample a random fake transaction ID.
            let id = Field::<CurrentNetwork>::rand(&mut rng).into();
            // Sample random fake transaction bytes.
            let transaction = Data::Buffer(Bytes::from((0..1024).map(|_| rng.gen::<u8>()).collect::<Vec<_>>()));
            // Return the ID and transaction.
            (id, transaction)
        }

        // Initialize a counter.
        let mut counter = 0;

        loop {
            // Sample a random fake transaction ID and transaction.
            let (id, transaction) = if counter % 2 == 0 { sample(&mut shared_rng) } else { sample(&mut unique_rng) };
            // Initialize a callback sender and receiver.
            let (callback, callback_receiver) = oneshot::channel();
            // Send the fake transaction.
            if let Err(e) = tx_unconfirmed_transaction.send((id, transaction, callback)).await {
                error!("Failed to send unconfirmed transaction: {e}");
            }
            let _ = callback_receiver.await;
            // Increment the counter.
            counter += 1;
            // Sleep briefly.
            tokio::time::sleep(std::time::Duration::from_millis(interval_ms)).await;
        }
    });
}

/**************************************************************************************************/

/// An enum of error handlers for the REST API server.
pub struct RestError(pub String);

impl IntoResponse for RestError {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, format!("Something went wrong: {}", self.0)).into_response()
    }
}

impl From<anyhow::Error> for RestError {
    fn from(err: anyhow::Error) -> Self {
        Self(err.to_string())
    }
}

#[derive(Clone)]
struct NodeState {
    bft: Option<BFT<CurrentNetwork>>,
    primary: Primary<CurrentNetwork>,
}

/// Returns the leader of the previous round, if one was present.
async fn get_leader(State(node): State<NodeState>) -> Result<ErasedJson, RestError> {
    match &node.bft {
        Some(bft) => Ok(ErasedJson::pretty(bft.leader())),
        None => Err(RestError::from(anyhow!("BFT is not enabled"))),
    }
}

/// Returns the current round.
async fn get_current_round(State(node): State<NodeState>) -> Result<ErasedJson, RestError> {
    Ok(ErasedJson::pretty(node.primary.current_round()))
}

/// Returns the certificates for the given round.
async fn get_certificates_for_round(
    State(node): State<NodeState>,
    Path(round): Path<u64>,
) -> Result<ErasedJson, RestError> {
    Ok(ErasedJson::pretty(node.primary.storage().get_certificates_for_round(round)))
}

/// Starts up a local server for monitoring the node.
async fn start_server(bft: Option<BFT<CurrentNetwork>>, primary: Primary<CurrentNetwork>, node_id: u16) {
    // Initialize the routes.
    let router = Router::new()
        .route("/", get(|| async { "Hello, World!" }))
        .route("/leader", get(get_leader))
        .route("/round/current", get(get_current_round))
        .route("/certificates/:round", get(get_certificates_for_round))
        // Pass in the `NodeState` to access state.
        .with_state(NodeState { bft, primary });

    // Construct the IP address and port.
    let addr = format!("127.0.0.1:{}", 3000 + node_id);

    // Run the server.
    info!("Starting the server at '{addr}'...");
    axum::Server::bind(&addr.parse().unwrap())
        .serve(router.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .unwrap();
}

/**************************************************************************************************/

/// The operating mode of the node.
#[derive(Debug, Clone, ValueEnum)]
enum Mode {
    /// Runs the node with the Narwhal memory pool protocol.
    Narwhal,
    /// Runs the node with the Bullshark BFT protocol (on top of Narwhal).
    Bft,
}

/// A simple CLI for the node.
#[derive(Parser, Debug)]
struct Args {
    /// The mode to run the node in.
    #[arg(long)]
    mode: Mode,
    /// The ID of the node.
    #[arg(long, value_name = "ID")]
    id: u16,
    /// The number of nodes in the network.
    #[arg(long, value_name = "N")]
    num_nodes: u16,
    /// If set, the path to the file containing the committee configuration.
    #[arg(long, value_name = "PATH")]
    config: Option<PathBuf>,
    /// Enables the solution cannons, and optionally the interval in ms to run them on.
    #[arg(long, value_name = "INTERVAL_MS")]
    fire_solutions: Option<Option<u64>>,
    /// Enables the transaction cannons, and optionally the interval in ms to run them on.
    #[arg(long, value_name = "INTERVAL_MS")]
    fire_transactions: Option<Option<u64>>,
    /// Enables the solution and transaction cannons, and optionally the interval in ms to run them on.
    #[arg(long, value_name = "INTERVAL_MS")]
    fire_transmissions: Option<Option<u64>>,
}

/// A helper method to parse the peers provided to the CLI.
fn parse_peers(peers_string: String) -> Result<HashMap<u16, SocketAddr>, Error> {
    // Expect list of peers in the form of `node_id=ip:port`, one per line.
    let mut peers = HashMap::new();
    for peer in peers_string.lines() {
        let mut split = peer.split('=');
        let node_id = u16::from_str(split.next().ok_or_else(|| anyhow!("Bad Format"))?)?;
        let addr: String = split.next().ok_or_else(|| anyhow!("Bad Format"))?.parse()?;
        let ip = SocketAddr::from_str(addr.as_str())?;
        peers.insert(node_id, ip);
    }
    Ok(peers)
}

/**************************************************************************************************/

#[tokio::main]
async fn main() -> Result<()> {
    initialize_logger(1);

    let args = Args::parse();

    let peers = match args.config {
        Some(path) => parse_peers(std::fs::read_to_string(path)?)?,
        None => Default::default(),
    };

    // Initialize an optional BFT holder.
    let mut bft_holder = None;

    // Start the node.
    let (primary, sender) = match args.mode {
        Mode::Bft => {
            // Start the BFT.
            let (bft, sender) = start_bft(args.id, args.num_nodes, peers).await?;
            // Set the BFT holder.
            bft_holder = Some(bft.clone());
            // Return the primary and sender.
            (bft.primary().clone(), sender)
        }
        Mode::Narwhal => start_primary(args.id, args.num_nodes, peers).await?,
    };

    const DEFAULT_INTERVAL_MS: u64 = 450;

    // Set the interval in milliseconds for the solution and transaction cannons.
    let (solution_interval_ms, transaction_interval_ms) =
        match (args.fire_transmissions, args.fire_solutions, args.fire_transactions) {
            // Set the solution and transaction intervals to the same value.
            (Some(fire_transmissions), _, _) => (
                Some(fire_transmissions.unwrap_or(DEFAULT_INTERVAL_MS)),
                Some(fire_transmissions.unwrap_or(DEFAULT_INTERVAL_MS)),
            ),
            // Set the solution and transaction intervals to their configured values.
            (None, Some(fire_solutions), Some(fire_transactions)) => (
                Some(fire_solutions.unwrap_or(DEFAULT_INTERVAL_MS)),
                Some(fire_transactions.unwrap_or(DEFAULT_INTERVAL_MS)),
            ),
            // Set only the solution interval.
            (None, Some(fire_solutions), None) => (Some(fire_solutions.unwrap_or(DEFAULT_INTERVAL_MS)), None),
            // Set only the transaction interval.
            (None, None, Some(fire_transactions)) => (None, Some(fire_transactions.unwrap_or(DEFAULT_INTERVAL_MS))),
            // Don't fire any solutions or transactions.
            _ => (None, None),
        };

    // Fire solutions.
    if let Some(interval_ms) = solution_interval_ms {
        fire_unconfirmed_solutions(&sender, args.id, interval_ms);
    }

    // Fire transactions.
    if let Some(interval_ms) = transaction_interval_ms {
        fire_unconfirmed_transactions(&sender, args.id, interval_ms);
    }

    // Start the monitoring server.
    start_server(bft_holder, primary, args.id).await;
    // // Note: Do not move this.
    // std::future::pending::<()>().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_peers_empty() -> Result<(), Error> {
        let peers = parse_peers("".to_owned())?;
        assert_eq!(peers.len(), 0);
        Ok(())
    }

    #[test]
    fn parse_peers_ok() -> Result<(), Error> {
        let s = r#"0=192.168.1.176:5000
1=192.168.1.176:5001
2=192.168.1.176:5002
3=192.168.1.176:5003"#;
        let peers = parse_peers(s.to_owned())?;
        assert_eq!(peers.len(), 4);
        Ok(())
    }

    #[test]
    fn parse_peers_bad_id() -> Result<(), Error> {
        let s = "A=192.168.1.176:5000";
        let peers = parse_peers(s.to_owned());
        assert!(peers.is_err());
        Ok(())
    }

    #[test]
    fn parse_peers_bad_format() -> Result<(), Error> {
        let s = "foo";
        let peers = parse_peers(s.to_owned());
        assert!(peers.is_err());
        Ok(())
    }
}
