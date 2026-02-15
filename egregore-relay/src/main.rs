mod api;
mod config;
mod eviction;

use std::sync::Arc;
use std::time::Instant;

use clap::Parser;
use std::path::PathBuf;

use egregore::config::Config;
use egregore::feed::engine::FeedEngine;
use egregore::feed::store::FeedStore;
use egregore::gossip;
use egregore::identity::Identity;

use config::RelayConfig;

#[derive(Parser)]
#[command(name = "egregore-relay", version, about = "Relay server for Egregore network")]
struct Cli {
    /// Data directory for identity and database
    #[arg(long, default_value = "./relay-data")]
    data_dir: PathBuf,

    /// HTTP API port
    #[arg(long, default_value_t = 7660)]
    port: u16,

    /// Gossip TCP port
    #[arg(long, default_value_t = 7661)]
    gossip_port: u16,

    /// Network key for network isolation
    #[arg(long, default_value = "egregore-network-v1")]
    network_key: String,

    /// Maximum registered peers (0 = unlimited)
    #[arg(long, default_value_t = 10_000)]
    max_peers: usize,

    /// Message TTL in days (0 = keep forever)
    #[arg(long, default_value_t = 30)]
    ttl_days: u32,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "egregore_relay=info,egregore=info".into()),
        )
        .init();

    let cli = Cli::parse();

    let relay_config = RelayConfig {
        max_peers: cli.max_peers,
        ttl_days: cli.ttl_days,
    };

    let config = Config {
        data_dir: cli.data_dir,
        port: cli.port,
        gossip_port: cli.gossip_port,
        network_key: cli.network_key,
        peers: Vec::new(),
        ..Config::default()
    };

    std::fs::create_dir_all(&config.data_dir)?;

    let identity = Identity::load_or_generate(&config.identity_dir())?;

    tracing::info!(
        identity = %identity.public_id(),
        port = config.port,
        gossip_port = config.gossip_port,
        ttl_days = relay_config.ttl_days,
        "egregore-relay starting"
    );

    let store = FeedStore::open(&config.db_path())?;
    let engine = Arc::new(FeedEngine::new(store));

    // Start eviction loop
    if relay_config.ttl_days > 0 {
        let eviction_engine = engine.clone();
        let ttl_days = relay_config.ttl_days;
        tokio::spawn(async move {
            eviction::run_eviction_loop(eviction_engine, ttl_days).await;
        });
    }

    // Build relay API
    let state = api::RelayState {
        identity: identity.clone(),
        engine: engine.clone(),
        config: Arc::new(config.clone()),
        relay_config: Arc::new(relay_config),
        started_at: Instant::now(),
    };
    let app = api::router(state);

    // Start gossip server with authorization
    let gossip_bind = format!("0.0.0.0:{}", config.gossip_port);
    let gossip_net_key = config.network_key_bytes();
    let gossip_identity = identity.clone();
    let gossip_engine = engine.clone();
    let auth_engine = engine.clone();
    let authorize: gossip::server::AuthorizeFn =
        Arc::new(move |vk: &ed25519_dalek::VerifyingKey| {
            let pub_id = egregore::identity::PublicId::from_verifying_key(vk);
            auth_engine
                .store()
                .is_peer_authorized(&pub_id)
                .unwrap_or_else(|e| {
                    tracing::warn!(error = %e, peer = %pub_id.0, "authorization check failed");
                    false
                })
        });
    tokio::spawn(async move {
        if let Err(e) = gossip::server::run_server_with_auth(
            gossip_bind,
            gossip_net_key,
            gossip_identity,
            gossip_engine,
            Some(authorize),
        )
        .await
        {
            tracing::error!(error = %e, "gossip server failed");
        }
    });

    // Start HTTP server (bind to all interfaces for relay)
    let addr = format!("0.0.0.0:{}", config.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(addr = %addr, "relay HTTP API listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
            tracing::info!("shutting down relay");
        })
        .await?;

    Ok(())
}
