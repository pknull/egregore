//! Egregore node — daemon for a single agent's signed feed.
//!
//! Startup: load/generate identity → open SQLite → optionally start HTTP API
//! (127.0.0.1) → start gossip server (default: 127.0.0.1) → start gossip sync loop →
//! optionally start UDP LAN discovery.
//!
//! HTTP serves REST + SSE on localhost only (security boundary = loopback);
//! MCP endpoint is optional.
//! Gossip uses SHS + Box Stream — only peers with the same network key connect.

use std::collections::HashSet;
use std::sync::Arc;

use clap::{CommandFactory, FromArgMatches, Parser};
use std::path::PathBuf;

use egregore::api;
use egregore::config::{Config, HookEntry};
use egregore::feed::engine::FeedEngine;
use egregore::feed::store::FeedStore;
use egregore::gossip;
use egregore::hooks::HookExecutor;
use egregore::identity::Identity;

#[derive(Parser)]
#[command(
    name = "egregore",
    version,
    about = "SSB-inspired LLM knowledge sharing"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to YAML config file (default: <data_dir>/config.yaml)
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Emit machine-readable JSON on supported administrative commands
    #[arg(long, global = true)]
    json: bool,

    /// Suppress normal output on supported administrative commands
    #[arg(long, global = true)]
    quiet: bool,

    /// Data directory for identity and database
    #[arg(long, default_value = "./data", global = true)]
    data_dir: PathBuf,

    /// HTTP API port (localhost only)
    #[arg(long, default_value_t = 7654)]
    port: u16,

    /// Disable HTTP API server (disables REST, SSE, and MCP)
    #[arg(long)]
    no_api: bool,

    /// Disable MCP endpoint (/mcp) on the HTTP API
    #[arg(long)]
    no_mcp: bool,

    /// Gossip TCP port
    #[arg(long, default_value_t = 7655)]
    gossip_port: u16,

    /// IP address to bind gossip server to (default: 127.0.0.1)
    #[arg(long, default_value = "127.0.0.1")]
    gossip_bind: String,

    /// Encrypt private key with passphrase
    #[arg(long)]
    passphrase: bool,

    /// Network key for network isolation
    #[arg(long, default_value = "egregore-network-v1")]
    network_key: String,

    /// Enable strict schema validation (reject unknown content types/schemas)
    #[arg(long)]
    schema_strict: bool,

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

    /// Disable persistent push-based connections (push is enabled by default)
    #[arg(long)]
    no_push: bool,

    /// Maximum number of persistent connections to maintain
    #[arg(long)]
    max_persistent_connections: Option<usize>,

    /// Enable mDNS/Bonjour peer discovery (works across tailnets)
    #[arg(long)]
    mdns: bool,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Check for updates and optionally install the latest version
    Update {
        /// Only check for updates, don't install
        #[arg(long)]
        check: bool,
    },

    /// Follow a public feed author
    Follow {
        /// Public ID to follow
        author: String,
    },

    /// List followed authors
    Follows,

    /// Stop following a public feed author
    Unfollow {
        /// Public ID to unfollow
        author: String,
    },

    /// Manage topic subscriptions
    Topic {
        #[command(subcommand)]
        command: TopicCommand,
    },

    /// Manage consumer groups
    Group {
        #[command(subcommand)]
        command: GroupCommand,
    },

    /// Manage schema registry entries
    Schema {
        #[command(subcommand)]
        command: SchemaCommand,
    },

    /// Manage global retention policy
    Retention {
        #[command(subcommand)]
        command: RetentionCommand,
    },

    /// Manage gossip peers
    Peer {
        #[command(subcommand)]
        command: PeerCommand,
    },

    /// Show local identity information
    Identity {
        /// Include exportable private key material
        #[arg(long)]
        export: bool,
    },
}

#[derive(clap::Subcommand, Debug, Clone)]
enum TopicCommand {
    /// Subscribe to a topic
    Subscribe { name: String },
    /// List topic subscriptions
    List,
    /// Unsubscribe from a topic
    Unsubscribe { name: String },
}

#[derive(clap::Subcommand, Debug, Clone)]
enum GroupCommand {
    /// Create a consumer group
    Create {
        name: String,
        /// Comma-separated member public IDs to join immediately
        #[arg(long)]
        members: Option<String>,
    },
    /// List consumer groups
    List,
    /// Show a consumer group with membership details
    Show { name: String },
    /// Delete a consumer group
    Delete { name: String },
}

#[derive(clap::Subcommand, Debug, Clone)]
enum SchemaCommand {
    /// Register a schema from a JSON file
    Register {
        content_type: String,
        #[arg(long)]
        file: PathBuf,
    },
    /// List registered schemas
    List,
    /// Show a schema by schema ID or content type
    Show { schema: String },
}

#[derive(clap::Subcommand, Debug, Clone)]
enum RetentionCommand {
    /// Set the global retention policy
    Set {
        /// Maximum age such as 30d, 12h, or 1h30m
        #[arg(long)]
        max_age: Option<String>,
        /// Maximum message count to keep globally
        #[arg(long)]
        max_messages: Option<u64>,
    },
    /// Show the global retention policy
    Show,
}

#[derive(clap::Subcommand, Debug, Clone)]
enum PeerCommand {
    /// Add a peer address
    Add { address: String },
    /// List configured peer addresses
    List,
    /// Show detailed peer status
    Status,
}

mod cli_admin;

/// Build the final Config by merging: defaults -> YAML file -> CLI overrides.
fn build_config(cli: &Cli, matches: &clap::ArgMatches) -> anyhow::Result<Config> {
    use clap::parser::ValueSource;

    // Phase 1: data_dir always comes from CLI (needed to locate config file)
    let data_dir = cli.data_dir.clone();

    // Phase 2: Load YAML config if it exists
    let config_path = cli
        .config
        .clone()
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
    if cli.no_api {
        config.api_enabled = false;
    }
    if cli.no_mcp {
        config.mcp_enabled = false;
    }
    if matches.value_source("gossip_port") == Some(ValueSource::CommandLine) {
        config.gossip_port = cli.gossip_port;
    }
    if matches.value_source("gossip_bind") == Some(ValueSource::CommandLine) {
        config.gossip_bind = cli.gossip_bind.clone();
    }
    if matches.value_source("network_key") == Some(ValueSource::CommandLine) {
        config.network_key = cli.network_key.clone();
    }
    if matches.value_source("schema_strict") == Some(ValueSource::CommandLine) {
        config.schema_strict = cli.schema_strict;
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
        ..Default::default()
    };
    if cli_hook.is_active() {
        config.hooks.push(cli_hook);
    }

    // Push configuration (push is enabled by default, --no-push disables)
    if cli.no_push {
        config.push_enabled = false;
    }
    if let Some(max_conns) = cli.max_persistent_connections {
        config.max_persistent_connections = max_conns;
    }

    // mDNS configuration (--mdns enables)
    if cli.mdns {
        config.mdns_discovery = true;
    }

    config.validate()?;

    Ok(config)
}

/// GitHub repository owner for updates.
const REPO_OWNER: &str = "pknull";
/// GitHub repository name for updates.
const REPO_NAME: &str = "egregore";

/// Handle the `update` subcommand.
fn handle_update(check_only: bool) -> anyhow::Result<()> {
    use self_update::cargo_crate_version;

    let current_version = cargo_crate_version!();
    println!("Current version: {}", current_version);

    if check_only {
        // Just check for updates
        let releases = self_update::backends::github::ReleaseList::configure()
            .repo_owner(REPO_OWNER)
            .repo_name(REPO_NAME)
            .build()?
            .fetch()?;

        if let Some(latest) = releases.first() {
            let latest_version = latest.version.trim_start_matches('v');
            if latest_version != current_version {
                println!(
                    "New version available: {} -> {}",
                    current_version, latest_version
                );
                println!("Run `egregore update` to install");
            } else {
                println!("Already up to date");
            }
        } else {
            println!("No releases found");
        }
    } else {
        // Download and install
        println!("Checking for updates...");

        let status = self_update::backends::github::Update::configure()
            .repo_owner(REPO_OWNER)
            .repo_name(REPO_NAME)
            .bin_name("egregore")
            .current_version(current_version)
            .show_download_progress(true)
            .no_confirm(false)
            .build()?
            .update()?;

        match status {
            self_update::Status::UpToDate(v) => {
                println!("Already up to date ({})", v);
            }
            self_update::Status::Updated(v) => {
                println!("Updated to version {}", v);
                println!("Restart egregore to use the new version");
            }
        }
    }

    Ok(())
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

    // Handle subcommands
    if let Some(command) = &cli.command {
        match command {
            Command::Update { check } => {
                return handle_update(*check);
            }
            _ => {}
        }
    }

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

    if let Some(command) = &cli.command {
        let identity_dir = config.identity_dir();
        let identity = if cli.passphrase {
            load_encrypted_identity(&identity_dir)?
        } else {
            Identity::load_or_generate(&identity_dir)?
        };
        let store = FeedStore::open(&config.db_path())?;
        let ctx = cli_admin::CliContext {
            config: config.clone(),
            store,
            identity,
        };
        let output = cli_admin::OutputMode {
            json: cli.json,
            quiet: cli.quiet,
        };
        if cli_admin::handle_command(command, &ctx, output)? {
            return Ok(());
        }
    }

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

    // Validate security configuration (fails fast on insecure defaults)
    if let Err(err) = config.validate_security() {
        tracing::error!("{}", err.trim());
        return Err(anyhow::anyhow!("startup blocked by insecure configuration"));
    }

    // Warn if using the default (public) network key (informational only)
    if let Some(warning) = config.security_warning() {
        tracing::warn!("{}", warning.trim());
    }

    // Warn if discovery is enabled but gossip bound to loopback
    if let Some(warning) = config.discovery_warning() {
        tracing::warn!("{}", warning.trim());
    }

    // Init feed store
    let store = FeedStore::open(&config.db_path())?;

    // Increment generation counter on startup (marks node restart for mesh health)
    match store.increment_generation() {
        Ok(gen) => tracing::info!(generation = gen, "generation counter incremented"),
        Err(e) => tracing::warn!(error = %e, "failed to increment generation counter"),
    }

    let schemas_dir = config.schemas_dir();
    let engine = if config.schema_strict {
        tracing::info!("strict schema validation enabled");
        Arc::new(FeedEngine::with_schemas_dir_strict(store, &schemas_dir))
    } else {
        Arc::new(FeedEngine::with_schemas_dir(store, &schemas_dir))
    };
    tracing::info!(schemas_dir = %schemas_dir.display(), "custom schemas directory");

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
                        tracing::warn!(
                            skipped = n,
                            "hook receiver lagged, some messages were not processed"
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    // Build and optionally start API server.
    let api_server = if config.api_enabled {
        let state = api::AppState {
            identity: identity.clone(),
            engine: engine.clone(),
            config: Arc::new(config.clone()),
            started_at: std::time::Instant::now(),
            mcp_registry: api::mcp_registry::create_registry(),
        };
        let app = if config.mcp_enabled {
            api::router(state)
        } else {
            api::router_with_mcp(state, false)
        };

        // Start HTTP server (localhost only)
        let addr = format!("127.0.0.1:{}", config.port);
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        tracing::info!(
            addr = %addr,
            mcp_enabled = config.mcp_enabled,
            "HTTP API listening"
        );
        Some((listener, app))
    } else {
        tracing::info!("HTTP API disabled (--no-api or api_enabled=false)");
        if config.mcp_enabled {
            tracing::warn!("mcp_enabled=true ignored because API is disabled");
        }
        None
    };

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
    let gossip_bind = format!("{}:{}", config.gossip_bind, config.gossip_port);
    tracing::info!(bind = %gossip_bind, "gossip server starting");
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
        if let Err(e) =
            gossip::server::run_server_with_push(server_config, gossip_engine, server_registry)
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

    // Start mDNS discovery if enabled
    if config.mdns_discovery {
        let mdns_config = config.clone();
        let mdns_identity = identity.clone();
        let mdns_engine = engine.clone();
        tokio::spawn(async move {
            if let Err(e) =
                gossip::mdns::run_mdns_discovery(mdns_config, mdns_identity, mdns_engine).await
            {
                tracing::error!(error = %e, "mDNS discovery failed");
            }
        });
    }

    // Start retention cleanup task if enabled
    if config.retention_enabled {
        let retention_engine = engine.clone();
        let retention_interval = std::time::Duration::from_secs(config.retention_interval_secs);
        let tombstone_max_age = config.tombstone_max_age_secs;
        tracing::info!(
            interval_secs = config.retention_interval_secs,
            "retention cleanup enabled"
        );
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(retention_interval).await;
                let eng = retention_engine.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let cleanup = eng.store().run_retention_cleanup();
                    let tombstones = eng.store().cleanup_tombstones(tombstone_max_age);
                    (cleanup, tombstones)
                })
                .await;

                match result {
                    Ok((Ok(cleanup), Ok(tombstones))) => {
                        if cleanup.total() > 0 || tombstones > 0 {
                            tracing::info!(
                                expired = cleanup.expired,
                                retention = cleanup.retention,
                                compacted = cleanup.compacted,
                                tombstones_cleaned = tombstones,
                                "retention cleanup completed"
                            );
                        }
                    }
                    Ok((Err(e), _)) => {
                        tracing::warn!(error = %e, "retention cleanup failed");
                    }
                    Ok((_, Err(e))) => {
                        tracing::warn!(error = %e, "tombstone cleanup failed");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "retention task panicked");
                    }
                }
            }
        });
    }

    // Clone registry for shutdown handler
    let shutdown_registry = registry.clone();

    if let Some((listener, app)) = api_server {
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
    } else {
        tokio::signal::ctrl_c().await.ok();
        tracing::info!("shutting down");
        if let Some(ref reg) = shutdown_registry {
            reg.close_all().await;
        }
    }

    Ok(())
}

fn load_encrypted_identity(identity_dir: &std::path::Path) -> anyhow::Result<Identity> {
    use ed25519_dalek::SigningKey;
    use egregore::identity::encryption;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_parses_group_create_with_members_and_json_flag() {
        let cli = Cli::try_parse_from([
            "egregore",
            "--json",
            "--data-dir",
            "/tmp/egregore",
            "group",
            "create",
            "workers",
            "--members",
            "@one.ed25519,@two.ed25519",
        ])
        .unwrap();

        assert!(cli.json);
        match cli.command.unwrap() {
            Command::Group {
                command: GroupCommand::Create { name, members },
            } => {
                assert_eq!(name, "workers");
                assert_eq!(members.as_deref(), Some("@one.ed25519,@two.ed25519"));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn cli_parses_identity_export_and_quiet() {
        let cli = Cli::try_parse_from(["egregore", "--quiet", "identity", "--export"]).unwrap();

        assert!(cli.quiet);
        match cli.command.unwrap() {
            Command::Identity { export } => assert!(export),
            other => panic!("unexpected command: {other:?}"),
        }
    }
}
