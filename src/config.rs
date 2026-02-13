use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Default network key (SHA-256 of "egregore-network-v1").
/// Different keys create isolated networks.
const DEFAULT_NETWORK_KEY: &str = "egregore-network-v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub data_dir: PathBuf,
    pub port: u16,
    pub gossip_port: u16,
    pub gossip_interval_secs: u64,
    pub network_key: String,
    pub peers: Vec<String>,
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
}
