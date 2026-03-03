mod analysis;
mod api;
mod bsc;
mod config;
mod network;
mod oracle;
mod types;
mod ui;

use crate::{
    api::{ApiTransaction, AppState, create_router},
    bsc::{bsc_chain_spec, boot_nodes, head, BscHandshake},
    config::{load_config, load_or_generate_key, setup_logging},
    network::{EthP2PHandler, spawn_block_poller},
    oracle::GasOracle,
    types::UiUpdate,
    ui::run_ui,
};
use dashmap::DashMap;
use anyhow::Result;
use futures_util::StreamExt;
use reth_chainspec::ChainSpec;
use reth_network::transactions::NetworkTransactionEvent;
use alloy_primitives::{B256, B512};
use reth_discv4::{Discv4ConfigBuilder, NatResolver};
use reth_network::{
    EthNetworkPrimitives, NetworkConfigBuilder, NetworkEventListenerProvider, NetworkManager,
    PeersConfig, PeersInfo, config::SecretKey as RethSecretKey,
};
use reth_network_api::PeerId;
use reth_primitives::{Block, Head, TransactionSigned};
use reth_provider::noop::NoopProvider;
use reth_tasks::TaskManager;
use secp256k1::Secp256k1;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::{
    signal,
    sync::{
        RwLock, broadcast,
        mpsc::{self},
    },
};
use tracing::{error, info, trace, warn};

#[tokio::main]
async fn main() -> Result<()> {
    let app_config = load_config()?;
    setup_logging(app_config.debug_logging);
    info!("🚀 Starting BSC P2P Mempool Crawler...");
    println!("Loaded configuration: {:?}", app_config);

    let (tx_broadcaster, _) = broadcast::channel::<String>(100);
    let gas_oracle = Arc::new(GasOracle::new());

    let app_state = Arc::new(AppState {
        tx_broadcaster: tx_broadcaster.clone(),
        gas_oracle: gas_oracle.clone(),
    });

    let secret_key: RethSecretKey = load_or_generate_key(app_config.node_key_file.clone())?;
    let secp = Secp256k1::new();
    let public_key = secret_key.public_key(&secp);
    let serialized_pk_bytes = public_key.serialize_uncompressed();
    let our_peer_id: PeerId = B512::from_slice(&serialized_pk_bytes[1..65]);
    info!("🔑 Our Peer ID: {}", our_peer_id);

    // Use BSC chain specification
    let chain_spec: Arc<ChainSpec> = bsc_chain_spec();
    info!("⛓️ Using Chain Spec: BSC Mainnet (Chain ID: {})", chain_spec.chain);

    // Use BSC bootnodes
    let bootnodes = boot_nodes();
    info!("🌳 Using {} BSC bootnodes", bootnodes.len());

    let tokio_handle = tokio::runtime::Handle::current();
    let task_manager = TaskManager::new(tokio_handle);
    let executor = task_manager.executor();
    info!("Task executor created.");

    let (tx_event_sender, mut tx_event_receiver) =
        mpsc::unbounded_channel::<NetworkTransactionEvent>();
    let (decoded_tx_sender, mut decoded_tx_receiver) =
        mpsc::unbounded_channel::<Arc<TransactionSigned>>();
    let (db_writer_tx, mut db_writer_rx) = mpsc::unbounded_channel::<analysis::TxAnalysisResult>();
    let (ui_tx, ui_rx) = mpsc::unbounded_channel::<UiUpdate>();
    let (block_sender, mut block_receiver) = mpsc::unbounded_channel::<Block>();
    let public_tx_cache = Arc::new(RwLock::new(HashSet::<(B256, Instant)>::new()));
    let block_db_writer_tx = db_writer_tx.clone();
    let peers = Arc::new(DashMap::new());

    // Configure Discv4 with BSC bootnodes
    let mut discv4_builder = Discv4ConfigBuilder::default();
    discv4_builder.add_boot_nodes(bootnodes.clone());
    discv4_builder.lookup_interval(Duration::from_millis(500)); // Faster discovery for BSC
    info!("🔍 Discv4 behaviour configured for BSC.");

    let peers_config = PeersConfig::default()
        .with_max_outbound(app_config.max_peers_outbound)
        .with_max_inbound(app_config.max_peers_inbound);

    // Build network config with BSC handshake
    let config_builder = NetworkConfigBuilder::new(secret_key)
        .listener_addr(app_config.p2p_listen_addr)
        .discovery_addr(app_config.discv4_listen_addr)
        .boot_nodes(bootnodes.clone())
        .set_head(head())
        .with_pow() // BSC uses PoSA, but we use with_pow for compatibility
        .eth_rlpx_handshake(Arc::new(BscHandshake::default()))
        .peer_config(peers_config);

    let client = NoopProvider::eth(chain_spec.clone());
    let network_config = config_builder.build(client);

    // Set discovery after building
    let network_config = network_config.set_discovery_v4(discv4_builder.build());

    info!(
        "🔧 Network configured. RLPx TCP listening on {}. Discovery UDP listening on {}.",
        app_config.p2p_listen_addr, app_config.discv4_listen_addr
    );

    let mut network_manager = NetworkManager::new(network_config).await?;
    network_manager.set_transactions(tx_event_sender);
    let network_handle = network_manager.handle().clone();
    info!(
        "🌐 Network Manager created. Initial peer count: {}",
        network_handle.num_connected_peers()
    );

    let mut events = network_handle.event_listener();

    let initial_head = head();
    let event_handler = EthP2PHandler::new(
        chain_spec.clone(),
        network_handle.clone(),
        peers.clone(),
        initial_head,
        decoded_tx_sender.clone(),
        ui_tx.clone(),
    );
    let handler_arc = Arc::new(event_handler);

    let task_executor = &executor;

    let handler_clone_for_events = Arc::clone(&handler_arc);
    task_executor.spawn(Box::pin(async move {
        info!(target: "crawler::events", "EVENT HANDLER TASK STARTED");
        loop {
            if let Some(event) = events.next().await {
                trace!(target: "crawler::events", ?event, "Received network event object.");
                if let Err(e) = handler_clone_for_events
                    .handle_network_event_wrapper(event)
                    .await
                {
                    error!(target: "crawler::events", "Error handling network event: {}", e);
                }
            } else {
                warn!(target: "crawler::events", "Network event stream finished unexpectedly!");
                break;
            }
        }
    }));
    info!("Spawned Peer Event Handler task.");

    let handler_clone_for_tx = Arc::clone(&handler_arc);
    task_executor.spawn(Box::pin(async move {
        info!(target: "crawler::tx", "TX HANDLER TASK STARTED");
        loop {
            if let Some(event) = tx_event_receiver.recv().await {
                if let Err(e) = handler_clone_for_tx.handle_transaction_event(event).await {
                    error!(target: "crawler::tx", "Error handling transaction event: {}", e);
                }
            } else {
                warn!(target: "crawler::tx", "Transaction event stream finished unexpectedly!");
                break;
            }
        }
    }));
    info!("Spawned Transaction Event Handler task.");

    let cache_clone = public_tx_cache.clone();
    task_executor.spawn(Box::pin(async move {
        info!(target: "crawler::processor", "Starting decoded transaction processor task...");
        while let Some(tx_signed_arc) = decoded_tx_receiver.recv().await {
            cache_clone
                .write()
                .await
                .insert((*tx_signed_arc.hash(), Instant::now()));

            let analysis_result = analysis::analyze_transaction(&tx_signed_arc);
            if db_writer_tx.send(analysis_result).is_err() {
                error!(target: "crawler::processor", "Failed to send tx to DB writer.");
                break;
            }
        }
    }));
    info!("Spawned Decoded Transaction Processor task.");

    let writer_ui_tx = ui_tx.clone();
    let broadcaster = tx_broadcaster.clone();
    task_executor.spawn(Box::pin(async move {
        info!(target: "crawler::tx-writer", "Starting transaction writer task...");
        while let Some(tx) = db_writer_rx.recv().await {
            let api_tx = ApiTransaction {
                hash: tx.hash.to_string(),
                tx_type: tx.tx_type as i16,
                sender: tx.sender.map(|s| s.to_string()),
                receiver: tx.receiver.map(|r| r.to_string()),
                value_wei: tx.value.to_string(),
                gas_limit: tx.gas_limit as i64,
                gas_price_or_max_fee_wei: tx.gas_price_or_max_fee.map(|p| p.to_string()),
                max_priority_fee_wei: tx.max_priority_fee.map(|p| p.to_string()),
                input_len: tx.input_len as i32,
                first_seen_at: tx.first_seen_at,
                is_private: tx.is_private,
            };

            if let Ok(tx_json) = serde_json::to_string(&api_tx) {
                if broadcaster.send(tx_json).is_err() {
                    trace!(target: "crawler::broadcaster", "No active WebSocket clients to broadcast to");
                }
            }

            if writer_ui_tx.send(UiUpdate::NewTx(Box::new(tx))).is_err() {
                error!(target: "crawler::tx-writer", "Failed to send tx update to UI: receiver dropped.");
            }
        }
    }));
    info!("Spawned Transaction Writer task.");

    let network_manager_handle = task_executor.spawn(Box::pin(async move {
        info!(target: "crawler::netmgr", "Starting core network task...");
        network_manager.await;
        error!(target: "crawler::netmgr", "Core network task finished unexpectedly!");
    }));
    info!("Spawned Core Network task.");

    let ui_task_handle = task_executor.spawn(Box::pin(async move {
        info!(target: "crawler::ui", "UI TASK STARTED");
        if let Err(e) = run_ui(ui_rx).await {
            error!(target: "crawler::ui", "UI task error: {}", e);
        }
        info!(target: "crawler::ui", "UI task finished.");
    }));
    info!("Spawned UI task.");

    let oracle_collector_rx = tx_broadcaster.subscribe();
    let collector_oracle_instance = gas_oracle.clone();
    task_executor.spawn(Box::pin(async move {
        collector_oracle_instance
            .run_collector(oracle_collector_rx)
            .await;
    }));

    let calculator_oracle_instance = gas_oracle.clone();
    task_executor.spawn(Box::pin(async move {
        calculator_oracle_instance.run_calculator().await;
    }));
    info!("Spawned Gas Oracle tasks.");

    spawn_block_poller(
        &executor,
        network_handle.clone(),
        peers.clone(),
        block_sender.clone(),
        head().number,
    );
    info!("Block poller spawned!");

    let block_processor_cache = public_tx_cache.clone();

    task_executor.spawn(Box::pin(async move {
        info!(target: "crawler::block-processor", "Starting block processor task...");

        while let Some(block) = block_receiver.recv().await {
            info!(target: "crawler::block-processor", "⚙️ Processing new block #{}", block.header.number);
            let mut cache = block_processor_cache.write().await;
            let now = Instant::now();

            let stale_threshold = Duration::from_secs(60);
            cache.retain(|(_hash, seen_at)| now.duration_since(*seen_at) < stale_threshold);

            for tx in block.body.transactions {
                if !cache.iter().any(|(h, _)| h == tx.hash()) {
                    info!(target: "crawler::block-processor", "🕵️ Found private transaction: {}", tx.hash());

                    let mut analysis_result = analysis::analyze_transaction(&tx);
                    analysis_result.is_private = true;

                    if block_db_writer_tx.send(analysis_result).is_err() {
                        error!(target: "crawler::block-processor", "Failed to send private tx to DB writer.");
                    }
                }
            }
        }
    }));
    info!("Spawned Block Processor task.");

    task_executor.spawn(Box::pin(async move {
        let app_router = create_router(app_state);
        let listener = tokio::net::TcpListener::bind("0.0.0.0:8000").await.unwrap();
        info!(target: "crawler::api", "✅ API server listening on http://0.0.0.0:8000");
        axum::serve(listener, app_router).await.unwrap();
    }));
    info!("Spawned API Server task.");

    info!("✅ Crawler is running! Press Ctrl+C to shut down.");
    signal::ctrl_c().await?;
    info!("🛑 Ctrl+C received, initiating shutdown...");

    if let Err(e) = ui_tx.send(UiUpdate::Shutdown) {
        error!(target: "crawler::main", "Failed to send shutdown signal to UI task: {}", e);
    }

    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    drop(task_manager);

    let _ = tokio::join!(ui_task_handle, network_manager_handle);

    info!("Shutdown complete.");
    Ok(())
}
