// Copyright © 2021 Translucence Research, Inc. All rights reserved.

use crate::config::executable_name;
use crate::routes::{dispatch_url, RouteBinding, UrlSegmentType, UrlSegmentValue};
use async_std::sync::{Arc, RwLock};
use async_std::task;
use async_trait::async_trait;
use futures_util::StreamExt;
use phaselock::{
    error::PhaseLockError, event::EventType, message::Message, networking::w_network::WNetwork,
    traits::storage::memory_storage::MemoryStorage, PhaseLock, PhaseLockConfig, PubKey,
};
use rand_xoshiro::{rand_core::SeedableRng, Xoshiro256StarStar};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::json;
use std::collections::hash_map::{Entry, HashMap};
use std::fs::File;
use std::io::{prelude::*, Read};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;
use structopt::StructOpt;
use tagged_base64::TaggedBase64;
use threshold_crypto as tc;
use tide_websockets::{
    async_tungstenite::tungstenite::protocol::frame::coding::CloseCode, Message::Close, WebSocket,
    WebSocketConnection,
};
use toml::Value;
use tracing::debug;
use tracing::{event, Level};
use zerok_lib::{
    node::*, ElaboratedBlock, ElaboratedTransaction, MultiXfrRecordSpec, MultiXfrTestState,
};

mod config;
mod disco;
mod ip;
mod routes;

const STATE_SEED: [u8; 32] = [0x7au8; 32];
const TRANSACTION_COUNT: u64 = 3;

#[derive(Debug, StructOpt)]
#[structopt(
    name = "Multi-machine concensus",
    about = "Simulates consensus among multiple machines"
)]
struct NodeOpt {
    /// Path to the node configuration file.
    #[structopt(
        long = "config",
        short = "c",
        default_value = ""      // See fn default_config_path().
    )]
    config: String,

    /// Whether to generate and store public keys for all nodes.
    ///
    /// Public keys will be stored under `examples/multi_machine/src`, file names starting
    /// with `pk_`.
    ///
    /// Skip this option if public key files already exist.
    #[structopt(long = "generate_keys", short = "g")]
    generate_keys: bool,

    /// Id of the current node.
    ///
    /// If the node ID is 0, it will propose and try to add transactions.
    ///
    /// Skip this option if only want to generate public key files.
    #[structopt(long = "id", short = "i")]
    id: Option<u64>,

    /// Whether the current node should run a full node.
    #[structopt(long = "full", short = "f")]
    full: bool,

    /// Path to assets including web server files.
    #[structopt(
        long = "assets",
        default_value = ""      // See fn default_web_path().
    )]
    web_path: String,

    /// Path to API specification and messages.
    #[structopt(
        long = "api",
        default_value = ""      // See fn default_api_path().
    )]
    api_path: String,
}

/// Gets public key of a node from its public key file.
fn get_public_key(node_id: u64) -> PubKey {
    let path_str = format!("../../examples/multi_machine/src/pk_{}", node_id);
    let path = Path::new(&path_str);
    let mut pk_file = File::open(&path)
        .unwrap_or_else(|_| panic!("Cannot find public key file: {}", path.display()));
    let mut pk_str = String::new();
    pk_file
        .read_to_string(&mut pk_str)
        .unwrap_or_else(|err| panic!("Error while reading public key file: {}", err));
    serde_json::from_str(&pk_str).expect("Error while reading public key")
}

/// Returns the project directory assuming the executable is in a
/// default build location.
///
/// For example, if the executable path is
/// ```
///    ~/tri/systems/system/target/release/multi_machine
/// ```
/// then the project path
/// ```
///    ~/tri/systems/system/examples/multi_machine/
/// ```
// Note: This function will need to be edited if copied to a project
// that is not under examples/ or a sibling directory.
fn project_path() -> PathBuf {
    const EX_DIR: &str = "examples";
    let mut project = PathBuf::from(
        std::env::current_exe()
            .expect("current_exe() returned an error")
            .parent()
            .expect("Unable to find parent directory (1)")
            .parent()
            .expect("Unable to find parent directory (2)")
            .parent()
            .expect("Unable to find parent directory (3)"),
    );
    project.push(EX_DIR);
    project.push(&executable_name());
    project
}

/// Returns "<repo>/public/" where <repo> is
/// derived from the executable path assuming the executable is in
/// two directory levels down and the project directory name
/// can be derived from the executable name.
///
/// For example, if the executable path is
/// ```
///    ~/tri/systems/system/examples/multi_machine/target/release/multi_machine
/// ```
/// then the asset path is
/// ```
///    ~/tri/systems/system/examples/multi_machine/public/
/// ```
fn default_web_path() -> PathBuf {
    const ASSET_DIR: &str = "public";
    let dir = project_path();
    [&dir, Path::new(ASSET_DIR)].iter().collect()
}

/// Returns the default path to the node configuration file.
fn default_config_path() -> PathBuf {
    const CONFIG_FILE: &str = "src/node-config.toml";
    let dir = project_path();
    [&dir, Path::new(CONFIG_FILE)].iter().collect()
}

/// Returns the default path to the node configuration file.
fn default_api_path() -> PathBuf {
    const API_FILE: &str = "api/api.toml";
    let dir = project_path();
    [&dir, Path::new(API_FILE)].iter().collect()
}

/// Reads configuration file path and node id from options
fn get_node_config() -> Value {
    let config_path_str = NodeOpt::from_args().config;
    let path = if config_path_str.is_empty() {
        println!("default config path");
        default_config_path()
    } else {
        println!("command line config path");
        PathBuf::from(&config_path_str)
    };

    // Read node info from node configuration file
    let mut config_file = File::open(&path)
        .unwrap_or_else(|_| panic!("Cannot find node config file: {}", path.display()));
    let mut config_str = String::new();
    config_file
        .read_to_string(&mut config_str)
        .unwrap_or_else(|err| panic!("Error while reading node config file: {}", err));
    toml::from_str(&config_str).expect("Error while reading node config file")
}

/// Gets IP address and port number of a node from node configuration file.
fn get_host(node_config: Value, node_id: u64) -> (String, u16) {
    let node = &node_config["nodes"][node_id.to_string()];
    let ip = node["ip"].as_str().expect("Missing IP info").to_owned();
    let port = node["port"].as_integer().expect("Missing port info") as u16;
    (ip, port)
}

/// Trys to get a networking implementation with the given id and port number.
///
/// Also starts the background task.
async fn get_networking<
    T: Clone + Serialize + DeserializeOwned + Send + Sync + std::fmt::Debug + 'static,
>(
    node_id: u64,
    port: u16,
) -> (WNetwork<T>, PubKey) {
    let pub_key = get_public_key(node_id);
    debug!(?pub_key);
    let network = WNetwork::new(pub_key.clone(), port, None).await;
    if let Ok(n) = network {
        let (c, sync) = futures::channel::oneshot::channel();
        match n.generate_task(c) {
            Some(task) => {
                task.into_iter().for_each(|n| {
                    async_std::task::spawn(n);
                });
                sync.await.expect("sync.await failed");
            }
            None => {
                panic!("Failed to launch networking task");
            }
        }
        return (n, pub_key);
    }
    panic!("Failed to open a port");
}

enum Node {
    Light(LightWeightNode),
    Full(FullNode<'static>),
}

#[async_trait]
impl Validator for Node {
    type Event = PhaseLockEvent;

    async fn submit_transaction(&self, tx: ElaboratedTransaction) -> Result<(), PhaseLockError> {
        match self {
            Node::Light(n) => <LightWeightNode as Validator>::submit_transaction(n, tx).await,
            Node::Full(n) => n.submit_transaction(tx).await,
        }
    }

    async fn start_consensus(&self) {
        match self {
            Node::Light(n) => n.start_consensus().await,
            Node::Full(n) => n.start_consensus().await,
        }
    }

    fn subscribe(&self) -> EventStream<Self::Event> {
        match self {
            Node::Light(n) => n.subscribe(),
            Node::Full(n) => <FullNode as Validator>::subscribe(n),
        }
    }
}

/// Creates the initial state and phaselock for simulation.
async fn init_state_and_phaselock(
    public_keys: tc::PublicKeySet,
    secret_key_share: tc::SecretKeyShare,
    nodes: u64,
    threshold: u64,
    node_id: u64,
    networking: WNetwork<Message<ElaboratedBlock, ElaboratedTransaction, 64>>,
    full_node: bool,
) -> (MultiXfrTestState, Node) {
    // Create the initial state
    let state = MultiXfrTestState::initialize(
        STATE_SEED,
        10,
        10,
        (
            MultiXfrRecordSpec {
                asset_def_ix: 0,
                owner_key_ix: 0,
                asset_amount: 100,
            },
            vec![
                MultiXfrRecordSpec {
                    asset_def_ix: 1,
                    owner_key_ix: 0,
                    asset_amount: 50,
                },
                MultiXfrRecordSpec {
                    asset_def_ix: 0,
                    owner_key_ix: 0,
                    asset_amount: 70,
                },
            ],
        ),
    )
    .unwrap();

    // Create the initial phaselock
    let known_nodes: Vec<_> = (0..nodes).map(get_public_key).collect();

    let config = PhaseLockConfig {
        total_nodes: nodes as u32,
        threshold: threshold as u32,
        max_transactions: 100,
        known_nodes,
        next_view_timeout: 10000,
        timeout_ratio: (11, 10),
        round_start_delay: 1,
    };
    debug!(?config);
    let genesis = ElaboratedBlock::default();
    let (_, phaselock) = PhaseLock::init(
        genesis,
        public_keys,
        secret_key_share,
        node_id,
        config,
        state.validator.clone(),
        networking,
        MemoryStorage::default(),
    )
    .await;
    debug!("phaselock launched");

    let validator = if full_node {
        let node = FullNode::new(
            phaselock,
            state.univ_setup,
            state.validator.clone(),
            state.record_merkle_tree.clone(),
            state.nullifiers.clone(),
            state.unspent_memos(),
        );
        Node::Full(node)
    } else {
        Node::Light(phaselock)
    };

    (state, validator)
}

#[derive(Clone)]
struct Connection {
    id: String,
    wsc: WebSocketConnection,
}

#[derive(Clone)]
struct WebState {
    connections: Arc<RwLock<HashMap<String, Connection>>>,
    web_path: String,
    api: toml::Value,
    query_service: FullNode<'static>,
}

impl WebState {
    async fn add_connection(&self, id: &str, wsc: WebSocketConnection) -> tide::Result<()> {
        event!(Level::DEBUG, "main.rs: Adding connection {}", &id);
        let mut connections = self.connections.write().await;
        let connection = Connection {
            id: id.to_string(),
            wsc,
        };
        connections.insert(id.to_string(), connection);
        Ok(())
    }

    async fn remove_connection(&self, id: &str) -> tide::Result<()> {
        event!(Level::DEBUG, "main.rs: Removing connection {}", id);
        let mut connections = self.connections.write().await;
        connections.remove(id);
        Ok(())
    }

    async fn send_message(&self, id: &str, cmd: &str, message: &str) -> tide::Result<()> {
        let mut connections = self.connections.write().await;
        match connections.entry(id.to_string()) {
            Entry::Vacant(_) => {
                event!(
                    Level::DEBUG,
                    "main.rs:send_message: Vacant {}, {}",
                    id,
                    message
                );
            }
            Entry::Occupied(mut id_connections) => {
                id_connections
                    .get_mut()
                    .wsc
                    .send_json(&json!({"clientId": id, "cmd": cmd, "msg": message }))
                    .await?
            }
        }
        Ok(())
    }

    /// Currently a demonstration of messages with delays to suggest processing time.
    async fn report_transaction_status(&self, id: &str) -> tide::Result<()> {
        task::sleep(Duration::from_secs(2)).await;
        self.send_message(id, "FOO", "Here it is.").await?;
        self.send_message(id, "INIT", "Something something").await?;
        task::sleep(Duration::from_secs(2)).await;
        self.send_message(id, "RECV", "Transaction received")
            .await?;
        task::sleep(Duration::from_secs(2)).await;
        self.send_message(id, "RECV", "Transaction accepted")
            .await?;
        Ok(())
    }
}

async fn landing_page(req: tide::Request<WebState>) -> Result<tide::Body, tide::Error> {
    let mut index_html: PathBuf = PathBuf::from(req.state().web_path.clone());
    index_html.push("index.html");
    Ok(tide::Body::from_file(index_html).await?)
}

/* TODO

Collect error messages for parameters that fail to parse, but only
when there are no literal mismatches

Add comprehensive documentation at /

Add an enum for each entry point so we know how to dispatch
}

 */

fn internal_error(msg: &'static str) -> tide::Error {
    tide::Error::from_str(tide::StatusCode::InternalServerError, msg)
}

async fn entry_page(req: tide::Request<WebState>) -> Result<tide::Response, tide::Error> {
    let first_segment = &req
        .url()
        .path_segments()
        .ok_or_else(|| internal_error("No path segments"))?
        .next()
        .ok_or_else(|| internal_error("Empty path"))?;
    let api = &req.state().api["route"][first_segment];
    let route_patterns = api["PATH"]
        .as_array()
        .ok_or_else(|| internal_error("Invalid PATH type. Expecting array."))?;
    let mut arg_doc: String = api["DOC"]
        .as_str()
        .ok_or_else(|| internal_error("Missing DOC"))?
        .to_string();
    let mut matching_route_count = 0u64;
    let mut matching_route = "";
    let mut bindings: HashMap<&str, HashMap<String, RouteBinding>> = HashMap::new();
    for route_pattern in route_patterns.iter() {
        let mut found_literal_mismatch = false;
        let mut argument_parse_failed = false;
        arg_doc.push_str(&format!(
            "\n\nRoute: {}\n--------------------\n",
            &route_pattern.as_str().unwrap()
        ));
        // The `path_segments()` succeeded above, so `unwrap()` is safe.
        let mut req_segments = req.url().path_segments().unwrap();
        for pat_segment in route_pattern
            .as_str()
            .expect("PATH must be an array of strings")
            .split('/')
        {
            // Each route parameter has an associated type. The lookup
            // will only succeed if the current segment is a parameter
            // placeholder, such as :id. Otherwise, it is assumed to
            // be a literal.
            if let Some(segment_type_value) = &api.get(pat_segment) {
                let segment_type = segment_type_value
                    .as_str()
                    .expect("The path pattern must be a string.");
                let req_segment = req_segments.next().unwrap_or("");
                arg_doc.push_str(&format!(
                    "  Argument: {} as type {} and value: {} ",
                    pat_segment, segment_type, req_segment
                ));
                if let Some(value) = UrlSegmentValue::parse(req_segment, segment_type) {
                    let rb = RouteBinding {
                        parameter: pat_segment.to_string(),
                        ptype: UrlSegmentType::from_str(segment_type).unwrap(),
                        value,
                    };
                    bindings
                        .entry(route_pattern.as_str().unwrap())
                        .or_default()
                        .insert(pat_segment.to_string(), rb);
                    arg_doc.push_str("(Parse succeeded)\n");
                } else {
                    arg_doc.push_str("(Parse failed)\n");
                    argument_parse_failed = true;
                }
            } else {
                // No type information. Assume pat_segment is a literal.
                let req_segment = req_segments.next().unwrap_or("");
                if req_segment != pat_segment {
                    found_literal_mismatch = true;
                    arg_doc.push_str(&format!(
                        "Request segment {} does not match route segment {}.\n",
                        req_segment, pat_segment
                    ));
                }
            }
        }
        if !found_literal_mismatch {
            arg_doc.push_str(&format!(
                "Literals match for {}\n",
                &route_pattern.as_str().unwrap(),
            ));
        }
        let mut length_matches = false;
        if req_segments.next().is_none() {
            arg_doc.push_str(&format!(
                "Length match for {}\n",
                &route_pattern.as_str().unwrap(),
            ));
            length_matches = true;
        }
        if argument_parse_failed {
            arg_doc.push_str(&"Argument parsing failed.\n".to_string());
        } else {
            arg_doc.push_str(&"No argument parsing errors!\n".to_string());
        }
        if !argument_parse_failed && length_matches && !found_literal_mismatch {
            let route_pattern_str = route_pattern.as_str().unwrap();
            arg_doc.push_str(&format!("Route matches request: {}\n", &route_pattern_str));
            matching_route_count += 1;
            matching_route = route_pattern_str;
        } else {
            arg_doc.push_str("Route does not match request.\n");
        }
    }
    match matching_route_count {
        0 => arg_doc.push_str("\nNeed documentation"),
        1 => arg_doc.push_str(&format!(
            "\nCould dispatch: {}\n{:?}\nDispatch results:\n{:?}",
            matching_route,
            bindings.get(&matching_route).unwrap_or(&Default::default()),
            dispatch_url(
                matching_route,
                bindings.get(&matching_route).unwrap_or(&Default::default()),
                &req.state().query_service
            )
            .await?
        )),
        _ => arg_doc.push_str("\nAmbiguity in api.toml"),
    }

    // TODO !corbett set the mime type to text/html and convert the
    // string from markdown to html
    if matching_route_count == 1 {
        Ok(dispatch_url(
            matching_route,
            bindings.get(&matching_route).unwrap_or(&Default::default()),
            &req.state().query_service,
        )
        .await?)
    } else {
        Ok(tide::Response::builder(200).body(arg_doc).build())
    }
}

async fn handle_web_socket(
    req: tide::Request<WebState>,
    mut wsc: WebSocketConnection,
) -> tide::Result<()> {
    event!(Level::DEBUG, "main.rs: id: {}", &req.param("id")?);
    let id = req.param("id").expect("Route must include :id parameter.");
    let state = req.state().clone();
    state.add_connection(id, wsc.clone()).await?;
    state
        .send_message(id, "RPT", "Server says, \"Hi!\"")
        .await?;
    let mut closed = false;
    while let Some(result_message) = wsc.next().await {
        match result_message {
            Ok(message) => {
                event!(Level::DEBUG, "main.rs:WebSocket message: {:?}", message);
                if let Close(Some(cf)) = message {
                    // See https://docs.rs/tungstenite/0.14.0/tungstenite/protocol/frame/coding/enum.CloseCode.html
                    if cf.code == CloseCode::Away {
                        event!(Level::DEBUG, "main.rs:cf Client said goodbye.");
                        closed = true;
                        break;
                    }
                    event!(Level::DEBUG, "main.rs:cf {:?}", &cf.code);
                }
                // Demonstration
                state.report_transaction_status(id).await?;
            }
            Err(err) => {
                event!(Level::ERROR, "WebSocket stream: {:?}", err)
            }
        }
    }
    if !closed {
        event!(Level::ERROR, "main.rs: Client left without saying goodbye.");
    }
    state.remove_connection(id).await?;
    Ok(())
}

/// Initialize the web server.
///
/// `opt_web_path` is the path to the web assets directory. If the path
/// is empty, the default is constructed assuming Cargo is used to
/// build the executable in the customary location.
///
/// `own_id` is the identifier of this instance of the executable. The
/// port the web server listens on is `own_id + 50000`, unless the
/// PORT environment variable is set.
///
// TODO - take the port from the command line instead of the environment.
fn init_web_server(
    opt_web_path: &str,
    own_id: u64,
    query_service: FullNode<'static>,
) -> Result<task::JoinHandle<Result<(), std::io::Error>>, tide::Error> {
    // Take the command line option for the web asset directory path
    // provided it is not empty. Otherwise, construct the default from
    // the executable path.
    let web_path = if opt_web_path.is_empty() {
        default_web_path()
            .into_os_string()
            .into_string()
            .expect("Wut?! Asset path isn't UTF-8")
    } else {
        opt_web_path.to_string()
    };
    println!("Default API: {:?}", default_api_path());
    let api = disco::load_messages(&default_api_path());
    let mut web_server = tide::with_state(WebState {
        connections: Default::default(),
        web_path: web_path.clone(),
        api: api.clone(),
        query_service,
    });

    // Define the routes handled by the web server.
    web_server.at("/public").serve_dir(web_path)?;
    web_server.at("/").get(landing_page);
    web_server
        .at("/:id")
        .with(WebSocket::new(handle_web_socket))
        .get(landing_page);
    web_server
        .at("/transfer/:id/:recipient/:amount")
        .with(WebSocket::new(handle_web_socket))
        .get(landing_page);

    // Add routes from a configuration file.
    println!("Format version: {}", &api["meta"]["FORMAT_VERSION"]);
    if let Some(api_map) = api["route"].as_table() {
        api_map.values().for_each(|v| match &v["PATH"] {
            toml::Value::String(s) => {
                web_server.at(s).get(entry_page);
            }
            toml::Value::Array(a) => {
                for v in a {
                    if let Some(s) = v.as_str() {
                        web_server.at(s).get(entry_page);
                    } else {
                        println!("Oops! Array element: {:?}", v);
                    }
                }
            }
            _ => println!("Expecting a toml::String or toml::Array, but got: {:?}", &v),
        });
    }

    let port = std::env::var("PORT").unwrap_or_else(|_| (50000 + &own_id).to_string());
    let addr = format!("127.0.0.1:{}", port);
    let join_handle = async_std::task::spawn(web_server.listen(addr));
    Ok(join_handle)
}

#[async_std::main]
async fn main() -> Result<(), std::io::Error> {
    tracing_subscriber::fmt().init();

    // Get configuration
    let node_config = get_node_config();

    // Get secret key set
    let seed: u64 = node_config["seed"]
        .as_integer()
        .expect("Missing seed value") as u64;
    let nodes = node_config["nodes"]
        .as_table()
        .expect("Missing nodes info")
        .len() as u64;
    let threshold = ((nodes * 2) / 3) + 1;

    // Generate key sets
    let mut rng = Xoshiro256StarStar::seed_from_u64(seed);
    let secret_keys = tc::SecretKeySet::random(threshold as usize - 1, &mut rng);
    let public_keys = secret_keys.public_keys();

    // Generate public key for each node
    if NodeOpt::from_args().generate_keys {
        for node_id in 0..nodes {
            let pub_key = PubKey::from_secret_key_set_escape_hatch(&secret_keys, node_id);
            let pub_key_str = serde_json::to_string(&pub_key)
                .unwrap_or_else(|err| panic!("Error while serializing the public key: {}", err));
            let mut pk_file =
                File::create(format!("../../examples/multi_machine/src/pk_{}", node_id))
                    .unwrap_or_else(|err| {
                        panic!("Error while creating a public key file: {}", err)
                    });
            pk_file
                .write_all(pub_key_str.as_bytes())
                .unwrap_or_else(|err| {
                    panic!("Error while writing to the public key file: {}", err)
                });
        }
        println!("Public key files created");
    }

    if let Some(own_id) = NodeOpt::from_args().id {
        println!("Current node: {}", own_id);
        let secret_key_share = secret_keys.secret_key_share(own_id);

        // Get networking information
        let (own_network, _) =
            get_networking(own_id, get_host(node_config.clone(), own_id).1).await;
        #[allow(clippy::type_complexity)]
        let mut other_nodes: Vec<(u64, PubKey, String, u16)> = Vec::new();
        for id in 0..nodes {
            if id != own_id {
                let (ip, port) = get_host(node_config.clone(), id);
                let pub_key = get_public_key(id);
                other_nodes.push((id, pub_key, ip, port));
            }
        }

        // Connect the networking implementations
        for (id, pub_key, ip, port) in other_nodes {
            let socket = format!("{}:{}", ip, port);
            while own_network
                .connect_to(pub_key.clone(), &socket)
                .await
                .is_err()
            {
                debug!("  - Retrying");
                async_std::task::sleep(std::time::Duration::from_millis(10_000)).await;
            }
            println!("  - Connected to node {}", id);
        }

        // Wait for the networking implementations to connect
        while (own_network.connection_table_size().await as u64) < nodes - 1 {
            async_std::task::sleep(std::time::Duration::from_millis(10)).await;
        }
        println!("All nodes connected to network");

        // Initialize the state and phaselock
        let (mut state, mut phaselock) = init_state_and_phaselock(
            public_keys,
            secret_key_share,
            nodes,
            threshold,
            own_id,
            own_network,
            NodeOpt::from_args().full,
        )
        .await;
        let mut events = phaselock.subscribe();

        // If we are running a full node, also host a query API to inspect the accumulated state.
        let web_server = if let Node::Full(node) = &phaselock {
            Some(
                init_web_server(&NodeOpt::from_args().web_path, own_id, node.clone())
                    .expect("Failed to initialize web server"),
            )
        } else {
            None
        };

        // Start consensus for each transaction
        for round in 0..TRANSACTION_COUNT {
            println!("Starting round {}", round + 1);

            // Generate a transaction if the node ID is 0
            let mut txn = None;
            if own_id == 0 {
                println!("  - Proposing a transaction");
                let mut transactions = state
                    .generate_transactions(
                        round as usize,
                        vec![(true, 0, 0, 0, 0, -2)],
                        TRANSACTION_COUNT as usize,
                    )
                    .unwrap();
                txn = Some(transactions.remove(0));
                phaselock
                    .submit_transaction(txn.clone().unwrap().3)
                    .await
                    .unwrap();
            }

            // Start consensus
            // Note: wait until the transaction is proposed before starting consensus. Otherwise,
            // the node will never reaches decision.
            // Issue: https://gitlab.com/translucence/systems/system/-/issues/15.
            let mut line = String::new();
            println!("Hit the return key when ready to start the consensus...");
            std::io::stdin().read_line(&mut line).unwrap();
            phaselock.start_consensus().await;
            println!("  - Starting consensus");
            loop {
                println!("Waiting for PhaseLock event");
                let event = events.next().await.expect("PhaseLock unexpectedly closed");

                if let EventType::Decide { block: _, state } = event.event {
                    let commitment = TaggedBase64::new("LEDG", &state.commit())
                        .unwrap()
                        .to_string();
                    println!("  - Current commitment: {}", commitment);
                    break;
                } else {
                    println!("EVENT: {:?}", event);
                }
            }

            // Add the transaction if the node ID is 0
            if let Some((ix, keys_and_memos, sig, t)) = txn {
                println!("  - Adding the transaction");
                let mut blk = ElaboratedBlock::default();
                let (owner_memos, kixs) = {
                    let mut owner_memos = vec![];
                    let mut kixs = vec![];

                    for (kix, memo) in keys_and_memos {
                        kixs.push(kix);
                        owner_memos.push(memo);
                    }
                    (owner_memos, kixs)
                };

                // If we're running a full node, publish the receiver memos.
                if let Node::Full(node) = &mut phaselock {
                    node.post_memos(round, ix as u64, owner_memos.clone(), sig)
                        .await
                        .unwrap();
                }

                state
                    .try_add_transaction(
                        &mut blk,
                        t,
                        round as usize,
                        ix,
                        TRANSACTION_COUNT as usize,
                        owner_memos,
                        kixs,
                    )
                    .unwrap();
                state
                    .validate_and_apply(blk, round as usize, TRANSACTION_COUNT as usize, 0.0)
                    .unwrap();
            }
            println!("  - Round {} completed.", round + 1);
        }
        if let Some(join_handle) = web_server {
            join_handle.await?;
        }
    }
    println!("All rounds completed.");

    Ok(())
}