mod api;

use std::sync::Arc;

use clap::Parser;
use std::path::PathBuf;

use egregore::config::Config;
use egregore::feed::engine::FeedEngine;
use egregore::feed::store::FeedStore;
use egregore::gossip;
use egregore::identity::Identity;

#[derive(Parser)]
#[command(name = "egregore", version, about = "SSB-inspired LLM knowledge sharing")]
struct Cli {
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

    /// Peer addresses (host:port)
    #[arg(long)]
    peer: Vec<String>,

    /// Enable UDP LAN peer discovery
    #[arg(long)]
    lan_discovery: bool,

    /// UDP port for LAN discovery announcements
    #[arg(long, default_value_t = 7656)]
    discovery_port: u16,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "egregore=info".into()),
        )
        .init();

    let cli = Cli::parse();

    let config = Config {
        data_dir: cli.data_dir,
        port: cli.port,
        gossip_port: cli.gossip_port,
        network_key: cli.network_key,
        peers: cli.peer,
        lan_discovery: cli.lan_discovery,
        discovery_port: cli.discovery_port,
        ..Config::default()
    };

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
    let engine = Arc::new(FeedEngine::new(store));

    // Build API
    let state = api::AppState {
        identity: identity.clone(),
        engine: engine.clone(),
        config: Arc::new(config.clone()),
        started_at: std::time::Instant::now(),
    };
    let app = api::router(state);

    // Start HTTP server (localhost only)
    let addr = format!("127.0.0.1:{}", config.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(addr = %addr, "HTTP API listening");

    // Start gossip server
    let gossip_bind = format!("0.0.0.0:{}", config.gossip_port);
    let gossip_net_key = config.network_key_bytes();
    let gossip_identity = identity.clone();
    let gossip_engine = engine.clone();
    tokio::spawn(async move {
        if let Err(e) =
            gossip::server::run_server(gossip_bind, gossip_net_key, gossip_identity, gossip_engine)
                .await
        {
            tracing::error!(error = %e, "gossip server failed");
        }
    });

    // Start gossip sync loop (dynamic: reads DB peers each cycle)
    let sync_peers = config.peers.clone();
    let sync_net_key = config.network_key_bytes();
    let sync_identity = identity.clone();
    let sync_engine = engine.clone();
    let sync_interval = std::time::Duration::from_secs(config.gossip_interval_secs);
    tokio::spawn(async move {
        gossip::client::run_sync_loop(
            sync_peers,
            sync_net_key,
            sync_identity,
            sync_engine,
            sync_interval,
        )
        .await;
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

    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            tokio::signal::ctrl_c().await.ok();
            tracing::info!("shutting down");
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
