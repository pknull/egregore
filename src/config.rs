use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Default network key (SHA-256 of "egregore-network-v1").
/// Different keys create isolated networks.
const DEFAULT_NETWORK_KEY: &str = "egregore-network-v1";

/// Hook configuration for event-driven message handling.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HookConfig {
    /// Path to executable called when a message arrives.
    /// Message JSON is passed on stdin.
    pub on_message: Option<PathBuf>,
    /// URL to POST message JSON to when a message arrives.
    /// Can be used alongside on_message (both will fire).
    pub webhook_url: Option<String>,
    /// Optional content type filter (e.g., "query", "insight").
    /// If set, hooks only fire for messages matching this type.
    pub filter_content_type: Option<String>,
    /// Timeout in seconds for hook/webhook execution (default: 30).
    pub timeout_secs: Option<u64>,
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
    /// Hook configuration for event-driven handlers.
    #[serde(default)]
    pub hooks: HookConfig,
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
            hooks: HookConfig::default(),
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

    pub fn network_key_bytes(&self) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(self.network_key.as_bytes());
        hasher.finalize().into()
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
}
