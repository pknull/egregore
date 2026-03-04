use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

/// Placeholder network key that forces users to set their own.
/// The node will refuse to start with this value.
pub const DEFAULT_NETWORK_KEY: &str = "CHANGE_ME";

// Default port assignments (arbitrary, not IANA registered)
const DEFAULT_HTTP_PORT: u16 = 7654;
const DEFAULT_GOSSIP_PORT: u16 = 7655;
const DEFAULT_DISCOVERY_PORT: u16 = 7656;

// Timing defaults
const DEFAULT_GOSSIP_INTERVAL_SECS: u64 = 300; // 5 minutes
const DEFAULT_HOOK_RETRY_DELAY_SECS: u64 = 5;
const DEFAULT_RECONNECT_INITIAL_SECS: u64 = 5;
const DEFAULT_RECONNECT_MAX_SECS: u64 = 300; // 5 minutes
const DEFAULT_RETENTION_INTERVAL_SECS: u64 = 3600; // 1 hour
const DEFAULT_TOMBSTONE_MAX_AGE_SECS: u64 = 604_800; // 7 days

// Connection/flow defaults
const DEFAULT_MAX_PERSISTENT_CONNECTIONS: usize = 32;
const DEFAULT_FLOW_INITIAL_CREDITS: u32 = 100;
const DEFAULT_FLOW_RATE_LIMIT: u32 = 100; // messages per second per peer

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
    DEFAULT_HOOK_RETRY_DELAY_SECS
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
    /// Enable HTTP API server (REST + SSE + optional MCP endpoint).
    #[serde(default = "default_api_enabled")]
    pub api_enabled: bool,
    /// Enable MCP endpoint (/mcp) on HTTP API.
    #[serde(default = "default_mcp_enabled")]
    pub mcp_enabled: bool,
    pub port: u16,
    pub gossip_port: u16,
    pub gossip_interval_secs: u64,
    pub network_key: String,
    /// Enable strict schema validation.
    /// When true, unknown content types/schemas are rejected at publish/ingest.
    #[serde(default)]
    pub schema_strict: bool,
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
    /// IP address to bind gossip server to (default: 127.0.0.1).
    ///
    /// Set to "0.0.0.0" to accept connections from external peers.
    /// Default is loopback-only for security.
    #[serde(default = "default_gossip_bind")]
    pub gossip_bind: String,
}

fn default_retention_interval_secs() -> u64 {
    DEFAULT_RETENTION_INTERVAL_SECS
}

fn default_tombstone_max_age_secs() -> u64 {
    DEFAULT_TOMBSTONE_MAX_AGE_SECS
}

fn default_gossip_bind() -> String {
    "127.0.0.1".to_string()
}

fn default_mdns_service() -> String {
    "_egregore._tcp.local.".to_string()
}

fn default_max_persistent_connections() -> usize {
    DEFAULT_MAX_PERSISTENT_CONNECTIONS
}

fn default_reconnect_initial_secs() -> u64 {
    DEFAULT_RECONNECT_INITIAL_SECS
}

fn default_reconnect_max_secs() -> u64 {
    DEFAULT_RECONNECT_MAX_SECS
}

fn default_flow_control_enabled() -> bool {
    true
}

fn default_flow_initial_credits() -> u32 {
    DEFAULT_FLOW_INITIAL_CREDITS
}

fn default_flow_rate_limit() -> u32 {
    DEFAULT_FLOW_RATE_LIMIT
}

fn default_api_enabled() -> bool {
    true
}

fn default_mcp_enabled() -> bool {
    true
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("./data"),
            api_enabled: true,
            mcp_enabled: true,
            port: DEFAULT_HTTP_PORT,
            gossip_port: DEFAULT_GOSSIP_PORT,
            gossip_interval_secs: DEFAULT_GOSSIP_INTERVAL_SECS,
            network_key: DEFAULT_NETWORK_KEY.to_string(),
            schema_strict: false,
            peers: Vec::new(),
            lan_discovery: false,
            discovery_port: DEFAULT_DISCOVERY_PORT,
            hooks: Vec::new(),
            push_enabled: true,
            max_persistent_connections: DEFAULT_MAX_PERSISTENT_CONNECTIONS,
            reconnect_initial_secs: DEFAULT_RECONNECT_INITIAL_SECS,
            reconnect_max_secs: DEFAULT_RECONNECT_MAX_SECS,
            flow_control_enabled: true,
            flow_initial_credits: DEFAULT_FLOW_INITIAL_CREDITS,
            flow_rate_limit_per_second: DEFAULT_FLOW_RATE_LIMIT,
            mdns_discovery: false,
            mdns_service: default_mdns_service(),
            retention_enabled: false,
            retention_interval_secs: DEFAULT_RETENTION_INTERVAL_SECS,
            tombstone_max_age_secs: DEFAULT_TOMBSTONE_MAX_AGE_SECS,
            gossip_bind: default_gossip_bind(),
        }
    }
}

/// Check if a URL points to an internal/private address (SSRF risk).
///
/// Returns true for localhost, loopback, private IP ranges, and link-local addresses.
fn is_internal_url(url: &str) -> bool {
    use std::net::IpAddr;

    // Parse URL to extract host
    let host = match url::Url::parse(url) {
        Ok(parsed) => match parsed.host_str() {
            Some(h) => h.to_lowercase(),
            None => return false,
        },
        Err(_) => return false,
    };

    // Check for localhost variants
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }

    // Check for internal hostnames
    if host.ends_with(".local") || host.ends_with(".internal") {
        return true;
    }

    // Try to parse as IP address
    if let Ok(ip) = host.parse::<IpAddr>() {
        return match ip {
            IpAddr::V4(v4) => {
                v4.is_loopback()           // 127.0.0.0/8
                    || v4.is_private()     // 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16
                    || v4.is_link_local()  // 169.254.0.0/16
                    || v4.is_unspecified() // 0.0.0.0
            }
            IpAddr::V6(v6) => {
                v6.is_loopback()       // ::1
                    || v6.is_unspecified() // ::
                                           // Note: is_unique_local() and is_unicast_link_local() are unstable
            }
        };
    }

    false
}

impl Config {
    pub fn identity_dir(&self) -> PathBuf {
        self.data_dir.join("identity")
    }

    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("egregore.db")
    }

    /// Directory for custom schema definitions (.json files).
    pub fn schemas_dir(&self) -> PathBuf {
        self.data_dir.join("schemas")
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
        if self.api_enabled {
            if self.port == self.gossip_port {
                anyhow::bail!(
                    "port and gossip_port must differ ({} == {})",
                    self.port,
                    self.gossip_port
                );
            }
            if self.port == self.discovery_port || self.gossip_port == self.discovery_port {
                anyhow::bail!(
                    "discovery_port ({}) must differ from port ({}) and gossip_port ({})",
                    self.discovery_port,
                    self.port,
                    self.gossip_port
                );
            }
        } else if self.gossip_port == self.discovery_port {
            anyhow::bail!(
                "discovery_port ({}) must differ from gossip_port ({})",
                self.discovery_port,
                self.gossip_port
            );
        }
        if !self.api_enabled && self.mcp_enabled {
            tracing::warn!("mcp_enabled=true has no effect when api_enabled=false");
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
                // SSRF protection: warn about internal/private URLs
                if is_internal_url(url) {
                    tracing::warn!(
                        hook_name = ?hook.name,
                        url = %url,
                        "webhook URL points to internal/private address - potential SSRF risk"
                    );
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

    /// Check for security warnings at startup.
    ///
    /// Returns None - the placeholder key now blocks startup entirely.
    /// Kept for API compatibility but no longer produces warnings.
    pub fn security_warning(&self) -> Option<String> {
        None
    }

    /// Check if gossip_bind is a loopback address.
    fn is_loopback_bind(&self) -> bool {
        self.gossip_bind.starts_with("127.")
            || self.gossip_bind == "localhost"
            || self.gossip_bind.starts_with("::1")
            || self.gossip_bind == "[::1]"
    }

    /// Check for configuration warnings at startup.
    ///
    /// Returns a warning if discovery is enabled but gossip_bind is loopback.
    /// Discovered peers won't be able to connect back.
    pub fn discovery_warning(&self) -> Option<String> {
        let discovery_enabled = self.lan_discovery || self.mdns_discovery;

        if !discovery_enabled || !self.is_loopback_bind() {
            return None;
        }

        let methods: Vec<&str> = [
            self.lan_discovery.then_some("lan_discovery"),
            self.mdns_discovery.then_some("mdns_discovery"),
        ]
        .into_iter()
        .flatten()
        .collect();

        Some(format!(
            r#"WARNING: Discovery enabled but gossip bound to loopback.

You have enabled {} but gossip_bind is "{}".
Discovered peers will not be able to connect to your node.

To accept external connections, set:
  gossip_bind: "0.0.0.0"
"#,
            methods.join(" and "),
            self.gossip_bind
        ))
    }

    /// Validate security configuration at startup.
    ///
    /// Returns an error if using the placeholder network key.
    /// Users must explicitly set a network_key to start the node.
    pub fn validate_security(&self) -> Result<(), String> {
        if !self.is_default_network_key() {
            return Ok(());
        }

        Err(r#"ERROR: Network key not configured.

Your node is using the placeholder network key "CHANGE_ME".
You must set a network_key before the node can start.

Add to your config.yaml:
  network_key: "my-network-name"

Choose a key that identifies your network:
  - For a private network: use a unique name (e.g., "my-home-lab")
  - For a team: use your team/org name (e.g., "acme-corp-internal")
  - For public federation: use a shared key others can join (e.g., "egregore-public-v1")

Nodes with the same network_key can discover and replicate with each other.
Nodes with different keys are completely isolated.
"#.to_string())
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
        assert_ne!(
            c1.network_key_discriminator(),
            c2.network_key_discriminator()
        );
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
            schema_strict: true,
            mcp_enabled: false,
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
        assert!(parsed.schema_strict);
        assert!(parsed.api_enabled);
        assert!(!parsed.mcp_enabled);
        assert_eq!(parsed.hooks.len(), 2);
        assert_eq!(parsed.hooks[0].name.as_deref(), Some("test-hook"));
        assert_eq!(
            parsed.hooks[1].webhook_url.as_deref(),
            Some("https://example.com/hook")
        );
    }

    #[test]
    fn yaml_empty_hooks_defaults() {
        let yaml = "data_dir: ./data\nport: 7654\ngossip_port: 7655\ngossip_interval_secs: 300\nnetwork_key: test\npeers: []\nlan_discovery: false\ndiscovery_port: 7656\n";
        let config: Config = serde_yaml::from_str(yaml).unwrap();
        assert!(!config.schema_strict);
        assert!(config.api_enabled);
        assert!(config.mcp_enabled);
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
    fn validate_allows_port_conflict_when_api_disabled() {
        let config = Config {
            api_enabled: false,
            port: 7654,
            gossip_port: 7654,
            ..Config::default()
        };
        assert!(config.validate().is_ok());
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

    #[test]
    fn security_warning_returns_none() {
        // security_warning no longer produces warnings - validation blocks instead
        let config = Config::default();
        assert!(config.security_warning().is_none());

        let custom = Config {
            network_key: "my-private-network".to_string(),
            ..Config::default()
        };
        assert!(custom.security_warning().is_none());
    }

    #[test]
    fn default_gossip_bind_is_loopback() {
        let config = Config::default();
        assert_eq!(config.gossip_bind, "127.0.0.1");
    }

    #[test]
    fn discovery_warning_when_lan_discovery_with_loopback() {
        let config = Config {
            lan_discovery: true,
            gossip_bind: "127.0.0.1".to_string(),
            ..Config::default()
        };
        let warning = config.discovery_warning();
        assert!(warning.is_some());
        assert!(warning.unwrap().contains("lan_discovery"));
    }

    #[test]
    fn discovery_warning_when_mdns_discovery_with_loopback() {
        let config = Config {
            mdns_discovery: true,
            gossip_bind: "127.0.0.1".to_string(),
            ..Config::default()
        };
        let warning = config.discovery_warning();
        assert!(warning.is_some());
        assert!(warning.unwrap().contains("mdns_discovery"));
    }

    #[test]
    fn no_discovery_warning_with_external_bind() {
        let config = Config {
            lan_discovery: true,
            mdns_discovery: true,
            gossip_bind: "0.0.0.0".to_string(),
            ..Config::default()
        };
        assert!(config.discovery_warning().is_none());
    }

    #[test]
    fn no_discovery_warning_when_discovery_disabled() {
        let config = Config {
            lan_discovery: false,
            mdns_discovery: false,
            gossip_bind: "127.0.0.1".to_string(),
            ..Config::default()
        };
        assert!(config.discovery_warning().is_none());
    }

    // ============ SECURITY VALIDATION TESTS ============

    #[test]
    fn validate_security_passes_with_custom_key() {
        let config = Config {
            network_key: "my-private-network".to_string(),
            ..Config::default()
        };
        assert!(config.validate_security().is_ok());
    }

    #[test]
    fn validate_security_fails_with_placeholder_key() {
        let config = Config::default(); // Uses CHANGE_ME placeholder
        let result = config.validate_security();
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("CHANGE_ME"));
        assert!(err.contains("network_key"));
    }

    #[test]
    fn validate_security_error_contains_instructions() {
        let config = Config::default();
        let err = config.validate_security().unwrap_err();
        // Should explain what to do
        assert!(err.contains("network_key:"));
        assert!(err.contains("my-network-name"));
    }

    #[test]
    fn default_network_key_is_placeholder() {
        assert_eq!(DEFAULT_NETWORK_KEY, "CHANGE_ME");
        let config = Config::default();
        assert_eq!(config.network_key, "CHANGE_ME");
    }
}
