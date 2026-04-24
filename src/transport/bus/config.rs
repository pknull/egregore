//! `BusConfig` + `BrokerConfigInput` — Phase 2 Wave 1 Step 2.
//!
//! Configuration surface for the bus (NATS JetStream) transport. Attached to
//! `Config::bus` as an `Option<BusConfig>` so pre-Phase-2 deployments parse
//! with `bus = None`.
//!
//! Validation obligations enforced via `Config::validate` in `src/config.rs`:
//!
//! - `url` MUST use TLS (`tls://` or `nats+tls://`) — RFC 0001 §13.5 RL2.
//! - `credentials_path` MUST exist with mode ≤ 0o600 — RFC 0001 §13.5 RL3.
//! - `max_ack_pending` ∈ [1, 65535].
//! - Effective `ack_wait` ∈ [5s, 3600s] — WARN (not error) outside this band
//!   (amendment §C.11 leaves the choice to operators who have measured their
//!   publish latency).
//!
//! Amendment §C.11 binding: if `ack_wait_secs` is `None`, the effective
//! `ack_wait` is derived at startup from
//! `BRIDGE_QUEUE_CAPACITY × expected_publish_latency_ms`, bounded below by
//! 30s. The derivation is exposed via `BusConfig::effective_ack_wait` so
//! both the bootstrap (consumer creation) and validation code paths share
//! the same formula.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Default JetStream stream name — one stream covers all `egregore.feed.>`
/// subjects per the Phase 2 base plan §5.1.
const DEFAULT_STREAM_NAME: &str = "egregore-feed";

/// Bridge queue capacity used to size the `max_ack_pending` default window.
/// This mirrors `composite::direction::BRIDGE_QUEUE_CAPACITY` (Wave 3);
/// duplicating the value here keeps the config module free of the as-yet
/// unimplemented composite module dependency. If the composite's capacity
/// changes, update both constants in lock-step (a Wave 3 to-do).
const BRIDGE_QUEUE_CAPACITY: u32 = 256;

/// Default expected per-destination publish latency in milliseconds — used
/// to derive `ack_wait` when operators have not measured their own
/// environment. See amendment §C.11.
const DEFAULT_EXPECTED_PUBLISH_LATENCY_MS: u64 = 200;

/// Default `max_ack_pending` window. Sized at `2 × BRIDGE_QUEUE_CAPACITY`
/// so a destination can absorb a full queue's worth of in-flight pending
/// acks plus a second queue's worth of fresh ones without stalling the
/// consumer flow.
const DEFAULT_MAX_ACK_PENDING: u32 = 2 * BRIDGE_QUEUE_CAPACITY;

/// Lower bound on `max_ack_pending` accepted by `Config::validate`.
pub const MIN_MAX_ACK_PENDING: u32 = 1;
/// Upper bound on `max_ack_pending` accepted by `Config::validate`.
pub const MAX_MAX_ACK_PENDING: u32 = 65_535;

/// Lower bound on the WARN band for effective `ack_wait` (inclusive).
pub const ACK_WAIT_WARN_LOWER: Duration = Duration::from_secs(5);
/// Upper bound on the WARN band for effective `ack_wait` (inclusive).
pub const ACK_WAIT_WARN_UPPER: Duration = Duration::from_secs(3600);
/// Floor on the derived `ack_wait` when `ack_wait_secs` is unset.
/// 30s per amendment §C.11's worked example.
pub const ACK_WAIT_DERIVED_FLOOR: Duration = Duration::from_secs(30);

fn default_stream_name() -> String {
    DEFAULT_STREAM_NAME.to_string()
}

fn default_max_ack_pending() -> u32 {
    DEFAULT_MAX_ACK_PENDING
}

fn default_expected_publish_latency_ms() -> u64 {
    DEFAULT_EXPECTED_PUBLISH_LATENCY_MS
}

/// Bus transport configuration — optional block on the top-level `Config`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusConfig {
    /// NATS connection URL. MUST use TLS (`tls://...` or `nats+tls://...`).
    pub url: String,

    /// Path to NKey/JWT credentials. MUST exist and be mode ≤ 0o600.
    pub credentials_path: PathBuf,

    /// Stream name. Default: `"egregore-feed"`.
    #[serde(default = "default_stream_name")]
    pub stream_name: String,

    /// Durable consumer name. Auto-derived from node identity when unset.
    #[serde(default)]
    pub consumer_name: Option<String>,

    /// Broker details for Profile field (RFC 0001 §11.2).
    pub broker: BrokerConfigInput,

    /// Max-ack-pending window size. Default: `2 × BRIDGE_QUEUE_CAPACITY = 512`.
    #[serde(default = "default_max_ack_pending")]
    pub max_ack_pending: u32,

    /// `ack_wait` override in seconds. If unset, derived from
    /// `BRIDGE_QUEUE_CAPACITY × expected_publish_latency_ms`, floored at 30s
    /// per amendment §C.11.
    #[serde(default)]
    pub ack_wait_secs: Option<u64>,

    /// Expected per-destination publish latency in ms. Used to derive
    /// `ack_wait` when `ack_wait_secs` is unset. Default: 200ms.
    #[serde(default = "default_expected_publish_latency_ms")]
    pub expected_publish_latency_ms: u64,
}

/// Broker details input — parsed once, then converted into
/// `content_types::BrokerDetails` for the Profile.
///
/// All six fields are REQUIRED. This is load-bearing for RFC 0002 §11
/// (pilot disclosure) and RL11 (broker tenancy declaration): a deployment
/// cannot publish a Profile claiming bus-backed transport without
/// declaring its operator, jurisdiction, disclosure policy, and tenancy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrokerConfigInput {
    /// Operator name (organization running the NATS broker).
    pub operator_name: String,
    /// Legal jurisdiction (e.g., ISO country code).
    pub jurisdiction: String,
    /// URL or identifier for the broker operator's disclosure policy.
    pub disclosure_policy: String,
    /// Tenancy model: `"shared"`, `"dedicated"`, or a self-describing string.
    pub tenancy: String,
    /// Broker endpoint (operator-facing; may differ from `url`).
    pub broker_endpoint: String,
    /// Backend identifier; MUST be `"nats"` for Phase 2.
    pub backend: String,
}

impl From<BrokerConfigInput> for crate::feed::content_types::BrokerDetails {
    fn from(src: BrokerConfigInput) -> Self {
        Self {
            operator_name: src.operator_name,
            jurisdiction: src.jurisdiction,
            disclosure_policy: src.disclosure_policy,
            tenancy: src.tenancy,
            broker_endpoint: src.broker_endpoint,
            backend: src.backend,
        }
    }
}

impl From<&BrokerConfigInput> for crate::feed::content_types::BrokerDetails {
    fn from(src: &BrokerConfigInput) -> Self {
        src.clone().into()
    }
}

impl BusConfig {
    /// Compute the effective `ack_wait` duration.
    ///
    /// Binding formula from amendment §C.11:
    /// - If `ack_wait_secs` is `Some(n)`, return `Duration::from_secs(n)` as-is.
    /// - Otherwise compute `BRIDGE_QUEUE_CAPACITY × expected_publish_latency_ms`
    ///   in milliseconds, lower-bounded by `ACK_WAIT_DERIVED_FLOOR` (30s).
    ///
    /// Called by: `Config::validate` (to emit the out-of-band WARN) and
    /// the consumer bootstrap (Wave 1 Step 4).
    pub fn effective_ack_wait(&self) -> Duration {
        if let Some(secs) = self.ack_wait_secs {
            return Duration::from_secs(secs);
        }
        let derived_ms =
            u64::from(BRIDGE_QUEUE_CAPACITY).saturating_mul(self.expected_publish_latency_ms);
        let derived = Duration::from_millis(derived_ms);
        if derived < ACK_WAIT_DERIVED_FLOOR {
            ACK_WAIT_DERIVED_FLOOR
        } else {
            derived
        }
    }

    /// Validate the bus config — called from `Config::validate`.
    ///
    /// Errors are fatal (reject startup); WARN-band deviations log a warning
    /// but proceed.
    pub fn validate(&self) -> anyhow::Result<()> {
        // URL MUST use TLS (RFC 0001 §13.5 RL2).
        if !(self.url.starts_with("tls://") || self.url.starts_with("nats+tls://")) {
            anyhow::bail!(
                "bus.url MUST use TLS (tls://... or nats+tls://...): got {}",
                self.url
            );
        }

        // Credentials path MUST exist.
        if !self.credentials_path.exists() {
            anyhow::bail!(
                "bus.credentials_path does not exist: {}",
                self.credentials_path.display()
            );
        }

        // Credentials file MUST have mode ≤ 0o600 on Unix. On non-Unix
        // platforms we log a WARN (same posture as `identity/permissions.rs`).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::metadata(&self.credentials_path).map_err(|e| {
                anyhow::anyhow!(
                    "bus.credentials_path stat failed: {} ({})",
                    self.credentials_path.display(),
                    e
                )
            })?;
            let mode = meta.permissions().mode() & 0o777;
            if mode & 0o077 != 0 {
                anyhow::bail!(
                    "bus.credentials_path has permissive mode {:o} (must be ≤ 0o600): {}",
                    mode,
                    self.credentials_path.display()
                );
            }
        }

        // max_ack_pending bounds.
        if self.max_ack_pending < MIN_MAX_ACK_PENDING || self.max_ack_pending > MAX_MAX_ACK_PENDING
        {
            anyhow::bail!(
                "bus.max_ack_pending must be in [{}, {}], got {}",
                MIN_MAX_ACK_PENDING,
                MAX_MAX_ACK_PENDING,
                self.max_ack_pending
            );
        }

        // Required broker fields — parse errors are caught by serde, but we
        // explicitly reject empty strings to prevent an operator from
        // publishing a misleading Profile.
        for (name, value) in [
            ("operator_name", &self.broker.operator_name),
            ("jurisdiction", &self.broker.jurisdiction),
            ("disclosure_policy", &self.broker.disclosure_policy),
            ("tenancy", &self.broker.tenancy),
            ("broker_endpoint", &self.broker.broker_endpoint),
            ("backend", &self.broker.backend),
        ] {
            if value.trim().is_empty() {
                anyhow::bail!("bus.broker.{name} must be non-empty");
            }
        }

        // WARN-band check on effective ack_wait (amendment §C.11).
        let ack_wait = self.effective_ack_wait();
        if ack_wait < ACK_WAIT_WARN_LOWER || ack_wait > ACK_WAIT_WARN_UPPER {
            tracing::warn!(
                effective_ack_wait_secs = ack_wait.as_secs(),
                lower_secs = ACK_WAIT_WARN_LOWER.as_secs(),
                upper_secs = ACK_WAIT_WARN_UPPER.as_secs(),
                "bus: effective ack_wait outside recommended band — verify against measured destination publish latency"
            );
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a baseline valid BusConfig against a temp credentials file.
    fn valid_config_with_creds(path: PathBuf) -> BusConfig {
        BusConfig {
            url: "tls://nats.example.com:4222".to_string(),
            credentials_path: path,
            stream_name: default_stream_name(),
            consumer_name: None,
            broker: BrokerConfigInput {
                operator_name: "ACME".to_string(),
                jurisdiction: "CH".to_string(),
                disclosure_policy: "https://example.com/policy".to_string(),
                tenancy: "shared".to_string(),
                broker_endpoint: "nats.example.com:4222".to_string(),
                backend: "nats".to_string(),
            },
            max_ack_pending: DEFAULT_MAX_ACK_PENDING,
            ack_wait_secs: None,
            expected_publish_latency_ms: DEFAULT_EXPECTED_PUBLISH_LATENCY_MS,
        }
    }

    /// Write a throwaway credentials file with a given Unix mode.
    fn write_creds_file(mode: u32) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(b"-----BEGIN NATS USER JWT-----\nstub\n")
            .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(mode);
            std::fs::set_permissions(f.path(), perms).unwrap();
        }
        #[cfg(not(unix))]
        let _ = mode;
        f
    }

    #[test]
    fn effective_ack_wait_uses_override_when_set() {
        let creds = write_creds_file(0o600);
        let mut cfg = valid_config_with_creds(creds.path().to_path_buf());
        cfg.ack_wait_secs = Some(120);
        assert_eq!(cfg.effective_ack_wait(), Duration::from_secs(120));
    }

    #[test]
    fn effective_ack_wait_derives_from_latency_when_unset() {
        let creds = write_creds_file(0o600);
        let mut cfg = valid_config_with_creds(creds.path().to_path_buf());
        cfg.ack_wait_secs = None;
        cfg.expected_publish_latency_ms = 200;
        // 256 × 200ms = 51.2s
        assert_eq!(cfg.effective_ack_wait(), Duration::from_millis(51_200));
    }

    #[test]
    fn effective_ack_wait_derivation_is_floored_at_30s() {
        let creds = write_creds_file(0o600);
        let mut cfg = valid_config_with_creds(creds.path().to_path_buf());
        cfg.ack_wait_secs = None;
        // 256 × 50ms = 12.8s → floored to 30s
        cfg.expected_publish_latency_ms = 50;
        assert_eq!(cfg.effective_ack_wait(), ACK_WAIT_DERIVED_FLOOR);
    }

    #[test]
    fn effective_ack_wait_derives_higher_for_slow_destinations() {
        let creds = write_creds_file(0o600);
        let mut cfg = valid_config_with_creds(creds.path().to_path_buf());
        cfg.ack_wait_secs = None;
        // 256 × 500ms = 128s (amendment §C.11 worked example).
        cfg.expected_publish_latency_ms = 500;
        assert_eq!(cfg.effective_ack_wait(), Duration::from_secs(128));
    }

    #[test]
    fn validate_accepts_valid_config() {
        let creds = write_creds_file(0o600);
        let cfg = valid_config_with_creds(creds.path().to_path_buf());
        cfg.validate().expect("valid config must pass validate");
    }

    #[test]
    fn validate_rejects_plaintext_url() {
        let creds = write_creds_file(0o600);
        let mut cfg = valid_config_with_creds(creds.path().to_path_buf());
        cfg.url = "nats://nats.example.com:4222".to_string();
        let err = cfg.validate().expect_err("plaintext URL must be rejected");
        assert!(err.to_string().contains("TLS"), "got: {err}");
    }

    #[test]
    fn validate_rejects_missing_credentials_path() {
        let cfg = valid_config_with_creds(PathBuf::from("/nonexistent/path/creds"));
        let err = cfg
            .validate()
            .expect_err("missing credentials path must be rejected");
        assert!(err.to_string().contains("credentials_path"), "got: {err}");
    }

    #[cfg(unix)]
    #[test]
    fn validate_rejects_permissive_credentials_mode() {
        let creds = write_creds_file(0o644);
        let cfg = valid_config_with_creds(creds.path().to_path_buf());
        let err = cfg
            .validate()
            .expect_err("mode 0644 must be rejected (group/world readable)");
        assert!(err.to_string().contains("permissive"), "got: {err}");
    }

    #[test]
    fn validate_rejects_zero_max_ack_pending() {
        let creds = write_creds_file(0o600);
        let mut cfg = valid_config_with_creds(creds.path().to_path_buf());
        cfg.max_ack_pending = 0;
        let err = cfg.validate().expect_err("max_ack_pending=0 must fail");
        assert!(err.to_string().contains("max_ack_pending"), "got: {err}");
    }

    #[test]
    fn validate_rejects_overlarge_max_ack_pending() {
        let creds = write_creds_file(0o600);
        let mut cfg = valid_config_with_creds(creds.path().to_path_buf());
        cfg.max_ack_pending = 100_000;
        let err = cfg
            .validate()
            .expect_err("max_ack_pending > 65535 must fail");
        assert!(err.to_string().contains("max_ack_pending"), "got: {err}");
    }

    #[test]
    fn validate_rejects_empty_broker_field() {
        let creds = write_creds_file(0o600);
        let mut cfg = valid_config_with_creds(creds.path().to_path_buf());
        cfg.broker.jurisdiction = "".to_string();
        let err = cfg.validate().expect_err("empty broker field must fail");
        assert!(err.to_string().contains("jurisdiction"), "got: {err}");
    }

    #[test]
    fn yaml_round_trip_preserves_fields() {
        let creds = write_creds_file(0o600);
        let cfg = valid_config_with_creds(creds.path().to_path_buf());
        let yaml = serde_yaml::to_string(&cfg).unwrap();
        let parsed: BusConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(parsed.url, cfg.url);
        assert_eq!(parsed.max_ack_pending, cfg.max_ack_pending);
        assert_eq!(
            parsed.expected_publish_latency_ms,
            cfg.expected_publish_latency_ms
        );
        assert_eq!(parsed.broker.backend, "nats");
    }

    #[test]
    fn yaml_omits_optional_when_none() {
        // Missing `consumer_name` and `ack_wait_secs` must parse with their
        // default None values so pre-Phase-2 configs round-trip.
        let mut min_yaml = String::new();
        min_yaml.push_str("url: tls://nats.example.com:4222\n");
        min_yaml.push_str("credentials_path: /tmp/creds.stub\n");
        min_yaml.push_str("broker:\n");
        min_yaml.push_str("  operator_name: ACME\n");
        min_yaml.push_str("  jurisdiction: CH\n");
        min_yaml.push_str("  disclosure_policy: https://example.com/p\n");
        min_yaml.push_str("  tenancy: shared\n");
        min_yaml.push_str("  broker_endpoint: nats.example.com:4222\n");
        min_yaml.push_str("  backend: nats\n");

        let parsed: BusConfig = serde_yaml::from_str(&min_yaml).unwrap();
        assert_eq!(parsed.stream_name, DEFAULT_STREAM_NAME);
        assert_eq!(parsed.max_ack_pending, DEFAULT_MAX_ACK_PENDING);
        assert_eq!(
            parsed.expected_publish_latency_ms,
            DEFAULT_EXPECTED_PUBLISH_LATENCY_MS
        );
        assert!(parsed.ack_wait_secs.is_none());
        assert!(parsed.consumer_name.is_none());
    }
}
