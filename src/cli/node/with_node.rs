use std::net::SocketAddr;
use std::time::Duration;

use structopt::StructOpt;

use witnet_config::config::Config;
use witnet_node as node;

use super::json_rpc_client as rpc;
use witnet_data_structures::chain::PublicKeyHash;

pub fn exec_cmd(command: Command, mut config: Config) -> Result<(), failure::Error> {
    match command {
        Command::Block { node, hash } => {
            rpc::get_block(node.unwrap_or(config.jsonrpc.server_address), hash)
        }
        Command::BlockChain { node, epoch, limit } => {
            rpc::get_blockchain(node.unwrap_or(config.jsonrpc.server_address), epoch, limit)
        }
        Command::GetBalance { node, pkh } => {
            rpc::get_balance(node.unwrap_or(config.jsonrpc.server_address), pkh)
        }
        Command::GetPkh { node } => rpc::get_pkh(node.unwrap_or(config.jsonrpc.server_address)),
        Command::Output { node, pointer } => {
            rpc::get_output(node.unwrap_or(config.jsonrpc.server_address), pointer)
        }
        Command::Send {
            node,
            pkh,
            value,
            fee,
        } => rpc::send_vtt(
            node.unwrap_or(config.jsonrpc.server_address),
            pkh,
            value,
            fee,
        ),
        Command::Raw { node } => rpc::raw(node.unwrap_or(config.jsonrpc.server_address)),
        Command::ShowConfig => {
            // TODO: Implementation requires to make Config serializable
            Ok(())
        }
        Command::Run(params) => {
            if let Some(addr) = params.addr {
                config.connections.server_addr = addr;
            }

            if let Some(limit) = params.outbound_limit {
                config.connections.outbound_limit = limit;
            }

            if let Some(period) = params.bootstrap_peers_period_seconds {
                config.connections.bootstrap_peers_period = Duration::from_secs(period);
            }

            if let Some(db) = params.db {
                config.storage.db_path = db;
            }

            config.connections.known_peers.extend(params.known_peers);

            node::actors::node::run(config, || {
                // FIXME(#72): decide what to do when interrupt signals are received
                ctrlc::set_handler(move || {
                    node::actors::node::close();
                })
                .expect("Error setting handler for both SIGINT (Ctrl+C) and SIGTERM (kill)");
            })
        }
    }
}

#[derive(Debug, StructOpt)]
pub enum Command {
    #[structopt(name = "server", about = "Run a Witnet node server.", alias = "run")]
    Run(ConfigParams),
    #[structopt(
        name = "raw",
        about = "Send raw JSON-RPC requests, read from stdin one line at a time"
    )]
    Raw {
        /// Socket address of the Witnet node to query.
        #[structopt(short = "n", long = "node")]
        node: Option<SocketAddr>,
    },
    #[structopt(name = "blockchain", about = "Find blockchain hashes ")]
    BlockChain {
        /// Socket address of the Witnet node to query.
        #[structopt(short = "n", long = "node")]
        node: Option<SocketAddr>,
        /// First epoch from which to show block hashes.
        #[structopt(long = "epoch", default_value = "0")]
        epoch: u32,
        /// Max number of epochs for which to show block hashes.
        #[structopt(long = "limit", default_value = "100")]
        limit: u32,
    },
    #[structopt(name = "block", about = "Find a block by its hash ")]
    Block {
        /// Socket address of the Witnet node to query.
        #[structopt(short = "n", long = "node")]
        node: Option<SocketAddr>,
        #[structopt(name = "hash", help = "SHA-256 block hash in hex format")]
        hash: String,
    },
    #[structopt(name = "getBalance", about = "Get total balance of the node")]
    GetBalance {
        /// Socket address of the Witnet node to query.
        #[structopt(short = "n", long = "node")]
        node: Option<SocketAddr>,
        /// Public key hash for which to get balance. If omitted, defaults to the node pkh.
        #[structopt(long = "pkh")]
        pkh: Option<PublicKeyHash>,
    },
    #[structopt(name = "getPkh", about = "Get the public key hash of the node")]
    GetPkh {
        /// Socket address of the Witnet node to query.
        #[structopt(short = "n", long = "node")]
        node: Option<SocketAddr>,
    },
    #[structopt(name = "output", about = "Find an output of a transaction ")]
    Output {
        /// Socket address of the Witnet node to query.
        #[structopt(short = "n", long = "node")]
        node: Option<SocketAddr>,
        #[structopt(
            name = "pointer",
            help = "Output pointer of the transaction, that is: <transaction id>:<output index>"
        )]
        pointer: String,
    },
    #[structopt(name = "send", about = "Create a value transfer transaction")]
    Send {
        /// Socket address of the Witnet node to query.
        #[structopt(short = "n", long = "node")]
        node: Option<SocketAddr>,
        /// Public key hash of the destination
        #[structopt(long = "pkh")]
        pkh: PublicKeyHash,
        /// Value
        #[structopt(long = "value")]
        value: u64,
        /// Fee
        #[structopt(long = "fee")]
        fee: u64,
    },
    #[structopt(
        name = "show-config",
        about = "Dump the loaded config in Toml format to stdout."
    )]
    ShowConfig,
}

#[derive(Debug, StructOpt)]
pub struct ConfigParams {
    /// Socket address for the node server
    #[structopt(short = "l", long = "listen")]
    addr: Option<SocketAddr>,
    /// Initially known peers for the node.
    #[structopt(long = "peer")]
    known_peers: Vec<SocketAddr>,
    /// Max number of connections to other peers this node (as a client) maintains.
    #[structopt(long = "out-limit")]
    outbound_limit: Option<u16>,
    /// Period of the bootstrap peers task (in seconds).
    #[structopt(long = "peers-period")]
    bootstrap_peers_period_seconds: Option<u64>,
    #[structopt(long = "db", raw(help = "NODE_DB_HELP"))]
    db: Option<std::path::PathBuf>,
}

static NODE_DB_HELP: &str = r#"Path to the node database. If not specified will use '.witnet-rust-mainnet' for mainnet, or '.witnet-rust-testnet-N' for testnet number N."#;
