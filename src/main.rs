//! Egregore node — daemon for a single agent's signed feed.
//!
//! Startup: load/generate identity → open SQLite → start HTTP API (127.0.0.1)
//! → start gossip server (0.0.0.0) → start gossip sync loop → optionally
//! start UDP LAN discovery.
//!
//! HTTP serves REST + MCP on localhost only (security boundary = loopback).
//! Gossip uses SHS + Box Stream — only peers with the same network key connect.

mod api;

use std::collections::HashSet;
use std::sync::Arc;

use clap::{CommandFactory, FromArgMatches, Parser};
use std::path::PathBuf;

use egregore::config::{Config, HookEntry};
use egregore::feed::engine::FeedEngine;
use egregore::feed::store::FeedStore;
use egregore::gossip;
use egregore::hooks::HookExecutor;
use egregore::identity::Identity;

#[derive(Parser)]
#[command(name = "egregore", version, about = "SSB-inspired LLM knowledge sharing")]
struct Cli {
    /// Path to YAML config file (default: <data_dir>/config.yaml)
    #[arg(long)]
    config: Option<PathBuf>,

    /// Data directory for identity and database
    #[arg(long, default_value = "./data")]
    data_dir: PathBuf,

    /// HTTP API port (localhost only)
    #[arg(long, default_value_t = 7654)]
    port: u16,

    /// Gossip TCP port
    #[arg(long, default_value_t = 7655)]
    gossip_port: u16,

    /// Encrypt private key with passphrase
    #[arg(long)]
    passphrase: bool,

    /// Network key for network isolation
    #[arg(long, default_value = "egregore-network-v1")]
    network_key: String,

    /// Peer addresses (host:port). Repeatable. Appends to config file peers.
    #[arg(long)]
    peer: Vec<String>,

    /// Enable UDP LAN peer discovery
    #[arg(long)]
    lan_discovery: bool,

    /// UDP port for LAN discovery announcements
    #[arg(long, default_value_t = 7656)]
    discovery_port: u16,

    /// Gossip sync interval in seconds
    #[arg(long)]
    gossip_interval_secs: Option<u64>,

    /// Path to hook script called when messages arrive (use config file for multiple hooks)
    #[arg(long)]
    hook_on_message: Option<PathBuf>,

    /// URL to POST message JSON when messages arrive (use config file for multiple hooks)
    #[arg(long)]
    hook_webhook_url: Option<String>,

    /// Hook timeout in seconds
    #[arg(long)]
    hook_timeout_secs: Option<u64>,

    /// Generate a default config.yaml in data-dir and exit
    #[arg(long)]
    init_config: bool,

    /// Enable persistent push-based connections for real-time message propagation
    #[arg(long)]
    push_enabled: bool,

    /// Maximum number of persistent connections to maintain
    #[arg(long)]
    max_persistent_connections: Option<usize>,
}

/// Build the final Config by merging: defaults -> YAML file -> CLI overrides.
fn build_config(cli: &Cli, matches: &clap::ArgMatches) -> anyhow::Result<Config> {
    use clap::parser::ValueSource;

    // Phase 1: data_dir always comes from CLI (needed to locate config file)
    let data_dir = cli.data_dir.clone();

    // Phase 2: Load YAML config if it exists
    let config_path = cli.config.clone()
        .unwrap_or_else(|| Config::config_file_path(&data_dir));

    let mut config = match Config::load_from_file(&config_path)? {
        Some(file_config) => {
            tracing::info!(path = %config_path.display(), "loaded config file");
            file_config
        }
        None => {
            tracing::debug!(path = %config_path.display(), "no config file found, using defaults");
            Config::default()
        }
    };

    // Phase 3: CLI overrides — only apply values the user explicitly passed
    // data_dir: if the user explicitly passed --data-dir, use CLI value.
    // Otherwise keep the YAML value (which defaults to "./data" if unset).
    if matches.value_source("data_dir") == Some(ValueSource::CommandLine) {
        config.data_dir = data_dir;
    }

    if matches.value_source("port") == Some(ValueSource::CommandLine) {
        config.port = cli.port;
    }
    if matches.value_source("gossip_port") == Some(ValueSource::CommandLine) {
        config.gossip_port = cli.gossip_port;
    }
    if matches.value_source("network_key") == Some(ValueSource::CommandLine) {
        config.network_key = cli.network_key.clone();
    }
    if matches.value_source("discovery_port") == Some(ValueSource::CommandLine) {
        config.discovery_port = cli.discovery_port;
    }
    if matches.value_source("lan_discovery") == Some(ValueSource::CommandLine) {
        config.lan_discovery = cli.lan_discovery;
    }
    if let Some(interval) = cli.gossip_interval_secs {
        config.gossip_interval_secs = interval;
    }

    // Peers: CLI --peer appends to config file peers (deduplicated)
    if !cli.peer.is_empty() {
        config.peers.extend(cli.peer.iter().cloned());
        let mut seen = HashSet::new();
        config.peers.retain(|p| seen.insert(p.clone()));
    }

    // CLI hook flags create one additional hook entry appended to config file hooks
    let cli_hook = HookEntry {
        name: Some("cli".to_string()),
        on_message: cli.hook_on_message.clone(),
        webhook_url: cli.hook_webhook_url.clone(),
        timeout_secs: cli.hook_timeout_secs,
    };
    if cli_hook.is_active() {
        config.hooks.push(cli_hook);
    }

    // Push configuration
    if matches.value_source("push_enabled") == Some(ValueSource::CommandLine) {
        config.push_enabled = cli.push_enabled;
    }
    if let Some(max_conns) = cli.max_persistent_connections {
        config.max_persistent_connections = max_conns;
    }

    config.validate()?;

    Ok(config)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "egregore=info".into()),
        )
        .init();

    let mut matches = Cli::command().get_matches();
    let cli = Cli::from_arg_matches_mut(&mut matches)?;

    // Handle --init-config
    if cli.init_config {
        std::fs::create_dir_all(&cli.data_dir)?;
        let config_path = Config::config_file_path(&cli.data_dir);
        Config::write_default_config(&config_path)?;
        println!("Config written to {}", config_path.display());
        return Ok(());
    }

    let config = build_config(&cli, &matches)?;

    // Ensure data directory exists
    std::fs::create_dir_all(&config.data_dir)?;

    // Load or generate identity
    let identity_dir = config.identity_dir();
    let identity = if cli.passphrase {
        load_encrypted_identity(&identity_dir)?
    } else {
        Identity::load_or_generate(&identity_dir)?
    };

    tracing::info!(
        identity = %identity.public_id(),
        port = config.port,
        gossip_port = config.gossip_port,
        "egregore starting"
    );

    // Init feed store
    let store = FeedStore::open(&config.db_path())?;

    // Increment generation counter on startup (marks node restart for mesh health)
    match store.increment_generation() {
        Ok(gen) => tracing::info!(generation = gen, "generation counter incremented"),
        Err(e) => tracing::warn!(error = %e, "failed to increment generation counter"),
    }

    let engine = Arc::new(FeedEngine::new(store));

    // Start hook executor if configured
    if let Some(executor) = HookExecutor::new(config.hooks.clone()) {
        let hook_count = config.hooks.iter().filter(|h| h.is_active()).count();
        tracing::info!(hook_count, "hook executor enabled");
        for hook in &config.hooks {
            if hook.is_active() {
                tracing::info!(
                    name = ?hook.name,
                    on_message = ?hook.on_message,
                    webhook = ?hook.webhook_url,
                    "registered hook"
                );
            }
        }
        let mut hook_rx = engine.subscribe();
        tokio::spawn(async move {
            loop {
                match hook_rx.recv().await {
                    Ok(msg) => executor.execute(&msg).await,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "hook receiver lagged, some messages were not processed");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    // Build API
    let state = api::AppState {
        identity: identity.clone(),
        engine: engine.clone(),
        config: Arc::new(config.clone()),
        started_at: std::time::Instant::now(),
        mcp_registry: api::mcp_registry::create_registry(),
    };
    let app = api::router(state);

    // Start HTTP server (localhost only)
    let addr = format!("127.0.0.1:{}", config.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(addr = %addr, "HTTP API listening");

    // Create connection registry for push-based replication
    let registry = if config.push_enabled {
        let reg = Arc::new(gossip::registry::ConnectionRegistry::new(
            config.max_persistent_connections,
        ));
        tracing::info!(
            max_connections = config.max_persistent_connections,
            "push-based replication enabled"
        );

        // Start push manager to broadcast messages to persistent connections
        let push_manager = gossip::push::PushManager::new(reg.clone(), engine.clone());
        tokio::spawn(async move {
            push_manager.run().await;
        });

        Some(reg)
    } else {
        None
    };

    // Start gossip server
    let gossip_bind = format!("0.0.0.0:{}", config.gossip_port);
    let gossip_net_key = config.network_key_bytes();
    let gossip_identity = identity.clone();
    let gossip_engine = engine.clone();
    let server_registry = registry.clone();
    let server_push_enabled = config.push_enabled;
    let server_max_conns = config.max_persistent_connections;
    tokio::spawn(async move {
        let server_config = gossip::server::ServerConfig {
            bind_addr: gossip_bind,
            network_key: gossip_net_key,
            identity: gossip_identity,
            push_enabled: server_push_enabled,
            max_persistent_connections: server_max_conns,
        };
        if let Err(e) = gossip::server::run_server_with_push(
            server_config,
            gossip_engine,
            server_registry,
            None,
        )
        .await
        {
            tracing::error!(error = %e, "gossip server failed");
        }
    });

    // Start gossip sync loop (dynamic: reads DB peers each cycle)
    let mut sync_config = gossip::client::SyncConfig::new(
        config.peers.clone(),
        config.network_key_bytes(),
        identity.clone(),
        std::time::Duration::from_secs(config.gossip_interval_secs),
        config.push_enabled,
    );
    sync_config.backoff_initial = std::time::Duration::from_secs(config.reconnect_initial_secs);
    sync_config.backoff_max = std::time::Duration::from_secs(config.reconnect_max_secs);

    let sync_engine = engine.clone();
    let sync_registry = registry.clone();
    tokio::spawn(async move {
        gossip::client::run_sync_loop_with_push(sync_config, sync_engine, sync_registry).await;
    });

    // Start LAN discovery if enabled
    if config.lan_discovery {
        let disc_config = config.clone();
        let disc_identity = identity.clone();
        let disc_engine = engine.clone();
        tokio::spawn(async move {
            if let Err(e) =
                gossip::discovery::run_discovery(disc_config, disc_identity, disc_engine).await
            {
                tracing::error!(error = %e, "LAN discovery failed");
            }
        });
    }

    // Clone registry for shutdown handler
    let shutdown_registry = registry.clone();

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::signal::ctrl_c().await.ok();
            tracing::info!("shutting down");

            // Gracefully close all persistent connections
            if let Some(ref reg) = shutdown_registry {
                reg.close_all().await;
            }
        })
        .await?;

    Ok(())
}

fn load_encrypted_identity(identity_dir: &std::path::Path) -> anyhow::Result<Identity> {
    use egregore::identity::encryption;
    use ed25519_dalek::SigningKey;

    let encrypted_path = identity_dir.join("secret.key.enc");

    if encrypted_path.exists() {
        // Decrypt existing
        let passphrase = prompt_passphrase("Enter passphrase: ")?;
        let encrypted = encryption::load_encrypted(&encrypted_path)?;
        let secret_bytes = encryption::decrypt_key(&encrypted, &passphrase)?;
        let signing_key = SigningKey::from_bytes(&secret_bytes);
        Ok(Identity { signing_key })
    } else {
        // Generate new and encrypt
        let identity = Identity::generate();
        let passphrase = prompt_passphrase("Set passphrase for new identity: ")?;
        let encrypted = encryption::encrypt_key(&identity.secret_bytes(), &passphrase)?;
        std::fs::create_dir_all(identity_dir)?;
        encryption::save_encrypted(&encrypted, &encrypted_path)?;
        // Save public key
        let pub_path = identity_dir.join("public.key");
        std::fs::write(pub_path, identity.public_id().0.as_bytes())?;
        Ok(identity)
    }
}

fn prompt_passphrase(prompt: &str) -> anyhow::Result<String> {
    Ok(rpassword::prompt_password(prompt)?)
}
