use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// Default network key (SHA-256 of "egregore-network-v1").
/// Different keys create isolated networks.
///
/// WARNING: This key is public and shared across all default deployments.
/// For production use, set a unique network_key in your config.
pub const DEFAULT_NETWORK_KEY: &str = "egregore-network-v1";

/// A single hook definition. Each hook can be a subprocess, a webhook, or both.
/// Each hook has its own filter and timeout.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookEntry {
    /// Human-readable label for logging.
    #[serde(default)]
    pub name: Option<String>,
    /// Path to executable called when a message arrives.
    /// Message JSON is passed on stdin.
    pub on_message: Option<PathBuf>,
    /// URL to POST message JSON to when a message arrives.
    pub webhook_url: Option<String>,
    /// Timeout in seconds for hook/webhook execution (default: 30).
    pub timeout_secs: Option<u64>,
    /// Maximum number of retry attempts for failed/timed-out hooks (default: 0 = no retries).
    #[serde(default)]
    pub max_retries: u32,
    /// Delay in seconds between retry attempts (default: 5).
    #[serde(default = "default_retry_delay_secs")]
    pub retry_delay_secs: u64,
    /// Enable idempotency tracking to prevent duplicate hook execution (default: false).
    /// When enabled, completed hooks are tracked and skipped on replay/restart.
    #[serde(default)]
    pub idempotent: bool,
}

fn default_retry_delay_secs() -> u64 {
    5
}

impl HookEntry {
    /// Returns true if this entry has at least one actionable handler.
    pub fn is_active(&self) -> bool {
        self.on_message.is_some() || self.webhook_url.is_some()
    }

    /// Returns a unique identifier for this hook based on name or path/URL.
    /// Used as part of the idempotency key.
    pub fn unique_id(&self) -> String {
        if let Some(ref name) = self.name {
            return name.clone();
        }
        if let Some(ref path) = self.on_message {
            return format!("subprocess:{}", path.display());
        }
        if let Some(ref url) = self.webhook_url {
            return format!("webhook:{}", url);
        }
        "unknown".to_string()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub data_dir: PathBuf,
    pub port: u16,
    pub gossip_port: u16,
    pub gossip_interval_secs: u64,
    pub network_key: String,
    pub peers: Vec<String>,
    pub lan_discovery: bool,
    pub discovery_port: u16,
    /// Multiple hook configurations for event-driven handlers.
    #[serde(default)]
    pub hooks: Vec<HookEntry>,
    /// Enable persistent push-based connections.
    /// When enabled, after initial replication, connections attempt to
    /// negotiate persistent mode for real-time message propagation.
    #[serde(default)]
    pub push_enabled: bool,
    /// Maximum number of persistent connections to maintain.
    #[serde(default = "default_max_persistent_connections")]
    pub max_persistent_connections: usize,
    /// Initial delay for reconnection attempts (in seconds).
    #[serde(default = "default_reconnect_initial_secs")]
    pub reconnect_initial_secs: u64,
    /// Maximum delay for reconnection attempts (in seconds).
    #[serde(default = "default_reconnect_max_secs")]
    pub reconnect_max_secs: u64,
    /// Enable credit-based flow control for push connections.
    /// When enabled, peers exchange credit grants to manage backpressure.
    #[serde(default = "default_flow_control_enabled")]
    pub flow_control_enabled: bool,
    /// Initial credits granted to each peer connection.
    #[serde(default = "default_flow_initial_credits")]
    pub flow_initial_credits: u32,
    /// Maximum messages per second per peer (0 = unlimited).
    #[serde(default = "default_flow_rate_limit")]
    pub flow_rate_limit_per_second: u32,
    /// Enable mDNS/Bonjour peer discovery (works across tailnets).
    #[serde(default)]
    pub mdns_discovery: bool,
    /// mDNS service type for peer discovery.
    #[serde(default = "default_mdns_service")]
    pub mdns_service: String,
    /// Enable automatic retention cleanup.
    #[serde(default)]
    pub retention_enabled: bool,
    /// Interval in seconds between retention cleanup runs (default: 3600 = 1 hour).
    #[serde(default = "default_retention_interval_secs")]
    pub retention_interval_secs: u64,
    /// How long to keep tombstones in seconds (default: 604800 = 7 days).
    #[serde(default = "default_tombstone_max_age_secs")]
    pub tombstone_max_age_secs: u64,
}

fn default_retention_interval_secs() -> u64 {
    3600 // 1 hour
}

fn default_tombstone_max_age_secs() -> u64 {
    604800 // 7 days
}

fn default_mdns_service() -> String {
    "_egregore._tcp.local.".to_string()
}

fn default_max_persistent_connections() -> usize {
    32
}

fn default_reconnect_initial_secs() -> u64 {
    5
}

fn default_reconnect_max_secs() -> u64 {
    300
}

fn default_flow_control_enabled() -> bool {
    true
}

fn default_flow_initial_credits() -> u32 {
    100
}

fn default_flow_rate_limit() -> u32 {
    100 // 100 messages per second per peer
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("./data"),
            port: 7654,
            gossip_port: 7655,
            gossip_interval_secs: 300,
            network_key: DEFAULT_NETWORK_KEY.to_string(),
            peers: Vec::new(),
            lan_discovery: false,
            discovery_port: 7656,
            hooks: Vec::new(),
            push_enabled: true,
            max_persistent_connections: default_max_persistent_connections(),
            reconnect_initial_secs: default_reconnect_initial_secs(),
            reconnect_max_secs: default_reconnect_max_secs(),
            flow_control_enabled: default_flow_control_enabled(),
            flow_initial_credits: default_flow_initial_credits(),
            flow_rate_limit_per_second: default_flow_rate_limit(),
            mdns_discovery: false,
            mdns_service: default_mdns_service(),
            retention_enabled: false,
            retention_interval_secs: default_retention_interval_secs(),
            tombstone_max_age_secs: default_tombstone_max_age_secs(),
        }
    }
}

impl Config {
    pub fn identity_dir(&self) -> PathBuf {
        self.data_dir.join("identity")
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("egregore.db")
    }

    /// Build a FlowControlConfig from these settings.
    pub fn flow_control_config(&self) -> crate::gossip::flow_control::FlowControlConfig {
        crate::gossip::flow_control::FlowControlConfig {
            initial_credits: self.flow_initial_credits,
            rate_limit_per_second: self.flow_rate_limit_per_second,
            credits_enabled: self.flow_control_enabled,
            ..Default::default()
        }
    }

    /// Returns the default config file path for a given data directory.
    pub fn config_file_path(data_dir: &Path) -> PathBuf {
        data_dir.join("config.yaml")
    }

    /// Load config from a YAML file. Returns None if the file does not exist.
    pub fn load_from_file(path: &Path) -> anyhow::Result<Option<Config>> {
        if !path.exists() {
            return Ok(None);
        }
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config file: {}", path.display()))?;
        let config: Config = serde_yaml::from_str(&contents)
            .with_context(|| format!("failed to parse config file: {}", path.display()))?;
        Ok(Some(config))
    }

    /// Write a default config file with documentation comments.
    /// Returns an error if the file already exists (to prevent accidental overwrite).
    pub fn write_default_config(path: &Path) -> anyhow::Result<()> {
        if path.exists() {
            anyhow::bail!(
                "config file already exists: {}. Remove it first to regenerate.",
                path.display()
            );
        }
        let template = include_str!("config_template.yaml");
        std::fs::write(path, template)
            .with_context(|| format!("failed to write config file: {}", path.display()))?;
        Ok(())
    }

    /// Validate the config for obvious errors.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.port == self.gossip_port {
            anyhow::bail!("port and gossip_port must differ ({} == {})", self.port, self.gossip_port);
        }
        if self.port == self.discovery_port || self.gossip_port == self.discovery_port {
            anyhow::bail!(
                "discovery_port ({}) must differ from port ({}) and gossip_port ({})",
                self.discovery_port, self.port, self.gossip_port
            );
        }
        for hook in &self.hooks {
            if let Some(ref path) = hook.on_message {
                if !path.exists() {
                    tracing::warn!(
                        hook_name = ?hook.name,
                        path = %path.display(),
                        "hook script does not exist"
                    );
                }
            }
            if let Some(ref url) = hook.webhook_url {
                if !url.starts_with("http://") && !url.starts_with("https://") {
                    anyhow::bail!("webhook_url must start with http:// or https://: {}", url);
                }
            }
        }
        Ok(())
    }

    pub fn network_key_bytes(&self) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(self.network_key.as_bytes());
        hasher.finalize().into()
    }

    /// Check if using the default (public) network key.
    ///
    /// Returns true if the network key matches DEFAULT_NETWORK_KEY.
    /// Production deployments should use a unique key.
    pub fn is_default_network_key(&self) -> bool {
        self.network_key == DEFAULT_NETWORK_KEY
    }

    /// 8-byte discriminator derived from the network key.
    /// Double-hashed (SHA-256 of SHA-256) so broadcasting it doesn't
    /// reveal the SHS network key itself.
    pub fn network_key_discriminator(&self) -> [u8; 8] {
        use sha2::{Digest, Sha256};
        let first = Sha256::digest(self.network_key.as_bytes());
        let second = Sha256::digest(first);
        let mut out = [0u8; 8];
        out.copy_from_slice(&second[..8]);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discriminator_deterministic() {
        let config = Config::default();
        let d1 = config.network_key_discriminator();
        let d2 = config.network_key_discriminator();
        assert_eq!(d1, d2);
    }

    #[test]
    fn discriminator_differs_per_network() {
        let c1 = Config::default();
        let c2 = Config {
            network_key: "other-network-key".to_string(),
            ..Config::default()
        };
        assert_ne!(c1.network_key_discriminator(), c2.network_key_discriminator());
    }

    #[test]
    fn discriminator_differs_from_network_key_bytes() {
        let config = Config::default();
        let nk = config.network_key_bytes();
        let disc = config.network_key_discriminator();
        // Discriminator should NOT be a prefix of the network key
        assert_ne!(&nk[..8], &disc[..]);
    }

    #[test]
    fn hook_entry_is_active() {
        let empty = HookEntry::default();
        assert!(!empty.is_active());

        let with_path = HookEntry {
            on_message: Some(PathBuf::from("/bin/true")),
            ..Default::default()
        };
        assert!(with_path.is_active());

        let with_url = HookEntry {
            webhook_url: Some("https://example.com".to_string()),
            ..Default::default()
        };
        assert!(with_url.is_active());
    }

    #[test]
    fn yaml_round_trip() {
        let config = Config {
            peers: vec!["192.168.1.100:7655".to_string()],
            hooks: vec![
                HookEntry {
                    name: Some("test-hook".to_string()),
                    on_message: Some(PathBuf::from("./hooks/test.sh")),
                    timeout_secs: Some(10),
                    ..Default::default()
                },
                HookEntry {
                    name: Some("webhook".to_string()),
                    webhook_url: Some("https://example.com/hook".to_string()),
                    ..Default::default()
                },
            ],
            ..Config::default()
        };

        let yaml = serde_yaml::to_string(&config).unwrap();
        let parsed: Config = serde_yaml::from_str(&yaml).unwrap();

        assert_eq!(parsed.peers.len(), 1);
        assert_eq!(parsed.hooks.len(), 2);
        assert_eq!(parsed.hooks[0].name.as_deref(), Some("test-hook"));
        assert_eq!(parsed.hooks[1].webhook_url.as_deref(), Some("https://example.com/hook"));
    }

    #[test]
    fn yaml_empty_hooks_defaults() {
        let yaml = "data_dir: ./data\nport: 7654\ngossip_port: 7655\ngossip_interval_secs: 300\nnetwork_key: test\npeers: []\nlan_discovery: false\ndiscovery_port: 7656\n";
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(config.hooks.is_empty());
    }

    #[test]
    fn validate_port_conflict() {
        let config = Config {
            port: 7654,
            gossip_port: 7654,
            ..Config::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_bad_webhook_url() {
        let config = Config {
            hooks: vec![HookEntry {
                webhook_url: Some("not-a-url".to_string()),
                ..Default::default()
            }],
            ..Config::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_good_config() {
        let config = Config {
            hooks: vec![HookEntry {
                webhook_url: Some("https://example.com/hook".to_string()),
                ..Default::default()
            }],
            ..Config::default()
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn is_default_network_key_detection() {
        let default_config = Config::default();
        assert!(default_config.is_default_network_key());

        let custom_config = Config {
            network_key: "my-private-network-2024".to_string(),
            ..Config::default()
        };
        assert!(!custom_config.is_default_network_key());
    }
}
