//! Bloom filter for efficient sync — "what I already have" per feed.
//!
//! Exchanged during Have/Want to skip unnecessary queries. The filter contains
//! message hashes that a node already has, so the peer can avoid offering those
//! messages.
//!
//! Configuration: false-positive rate vs size tradeoff via `BloomConfig`.
//! Default: 1% false positive rate, optimized for typical feed sizes.

use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::identity::PublicId;

/// Configuration for bloom filter size vs accuracy tradeoff.
#[derive(Debug, Clone, Copy)]
pub struct BloomConfig {
    /// Target false positive rate (0.0 to 1.0). Default: 0.01 (1%)
    pub false_positive_rate: f64,
    /// Maximum bits to allocate. Default: 65536 (8KB)
    pub max_bits: usize,
    /// Minimum bits to allocate. Default: 64
    pub min_bits: usize,
}

impl Default for BloomConfig {
    fn default() -> Self {
        Self {
            false_positive_rate: 0.01, // 1% false positive rate
            max_bits: 65536,           // 8KB max
            min_bits: 64,              // 8 bytes min
        }
    }
}

impl BloomConfig {
    /// Create a config optimized for lower bandwidth (larger filters, fewer false positives).
    pub fn low_bandwidth() -> Self {
        Self {
            false_positive_rate: 0.001, // 0.1%
            max_bits: 131072,           // 16KB max
            min_bits: 128,
        }
    }

    /// Create a config optimized for smaller payloads (smaller filters, more false positives).
    pub fn compact() -> Self {
        Self {
            false_positive_rate: 0.05, // 5%
            max_bits: 16384,           // 2KB max
            min_bits: 32,
        }
    }

    /// Calculate optimal filter size for expected number of elements.
    fn optimal_size(&self, num_elements: usize) -> (usize, usize) {
        if num_elements == 0 {
            return (self.min_bits, 1);
        }

        // Optimal bits: m = -n * ln(p) / (ln(2)^2)
        // where n = num_elements, p = false_positive_rate
        let n = num_elements as f64;
        let p = self.false_positive_rate;
        let ln2_squared = std::f64::consts::LN_2 * std::f64::consts::LN_2;
        let optimal_bits = (-n * p.ln() / ln2_squared).ceil() as usize;

        // Optimal hash functions: k = (m/n) * ln(2)
        let m = optimal_bits.clamp(self.min_bits, self.max_bits) as f64;
        let optimal_k = (m / n * std::f64::consts::LN_2).ceil() as usize;

        // Clamp to reasonable bounds
        let bits = optimal_bits.clamp(self.min_bits, self.max_bits);
        let k = optimal_k.clamp(1, 16);

        (bits, k)
    }
}

/// A bloom filter for message hashes, serializable for wire transmission.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BloomFilter {
    /// Base64-encoded bit array
    bits: String,
    /// Number of bits in the filter
    num_bits: usize,
    /// Number of hash functions
    num_hashes: usize,
    /// Number of elements inserted
    count: usize,
}

impl BloomFilter {
    /// Create a new bloom filter with automatic sizing based on expected element count.
    pub fn new(expected_elements: usize, config: &BloomConfig) -> Self {
        let (num_bits, num_hashes) = config.optimal_size(expected_elements);
        let num_bytes = num_bits.div_ceil(8);
        let bits_vec = vec![0u8; num_bytes];
        Self {
            bits: B64.encode(&bits_vec),
            num_bits,
            num_hashes,
            count: 0,
        }
    }

    /// Create an empty bloom filter (for peers with no messages).
    pub fn empty() -> Self {
        Self {
            bits: B64.encode([0u8; 8]),
            num_bits: 64,
            num_hashes: 1,
            count: 0,
        }
    }

    /// Create a bloom filter from message hashes.
    pub fn from_hashes<'a, I>(hashes: I, config: &BloomConfig) -> Self
    where
        I: IntoIterator<Item = &'a str>,
        I::IntoIter: ExactSizeIterator,
    {
        let iter = hashes.into_iter();
        let count = iter.len();
        let mut filter = Self::new(count, config);
        for hash in iter {
            filter.insert(hash);
        }
        filter
    }

    /// Insert a message hash into the filter.
    pub fn insert(&mut self, hash: &str) {
        let mut bits_vec = B64.decode(&self.bits).unwrap_or_default();
        if bits_vec.is_empty() {
            bits_vec = vec![0u8; self.num_bits.div_ceil(8)];
        }

        for i in 0..self.num_hashes {
            let bit_index = self.hash_index(hash, i);
            let byte_index = bit_index / 8;
            let bit_offset = bit_index % 8;
            if byte_index < bits_vec.len() {
                bits_vec[byte_index] |= 1 << bit_offset;
            }
        }

        self.bits = B64.encode(&bits_vec);
        self.count += 1;
    }

    /// Check if a hash might be in the filter (may return false positives).
    pub fn might_contain(&self, hash: &str) -> bool {
        let bits_vec = match B64.decode(&self.bits) {
            Ok(v) => v,
            Err(_) => return false,
        };

        for i in 0..self.num_hashes {
            let bit_index = self.hash_index(hash, i);
            let byte_index = bit_index / 8;
            let bit_offset = bit_index % 8;
            if byte_index >= bits_vec.len() || (bits_vec[byte_index] & (1 << bit_offset)) == 0 {
                return false;
            }
        }
        true
    }

    /// Get the number of elements inserted.
    pub fn count(&self) -> usize {
        self.count
    }

    /// Get the size of the filter in bytes.
    pub fn size_bytes(&self) -> usize {
        self.num_bits.div_ceil(8)
    }

    /// Estimate the actual false positive rate based on fill ratio.
    pub fn estimated_false_positive_rate(&self) -> f64 {
        if self.count == 0 || self.num_bits == 0 {
            return 0.0;
        }
        // FPR = (1 - e^(-kn/m))^k
        let k = self.num_hashes as f64;
        let n = self.count as f64;
        let m = self.num_bits as f64;
        (1.0 - (-k * n / m).exp()).powf(k)
    }

    /// Compute hash index for a given hash and function index.
    fn hash_index(&self, hash: &str, func_index: usize) -> usize {
        // Double hashing: h(i) = h1 + i * h2
        // Using SHA-256, split output into two 128-bit halves
        let mut hasher = Sha256::new();
        hasher.update(hash.as_bytes());
        hasher.update([func_index as u8]);
        let result = hasher.finalize();

        // Use first 8 bytes as the hash value
        let hash_value = u64::from_le_bytes(result[..8].try_into().unwrap());
        (hash_value as usize) % self.num_bits
    }
}

/// Per-feed bloom filter summary, used in Have messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedBloomSummary {
    /// The author/feed this bloom filter covers
    pub author: PublicId,
    /// Latest sequence number we have
    pub latest_sequence: u64,
    /// Bloom filter of message hashes we have for this feed
    pub filter: BloomFilter,
}

impl FeedBloomSummary {
    /// Create a summary for a feed with no messages.
    pub fn empty(author: PublicId) -> Self {
        Self {
            author,
            latest_sequence: 0,
            filter: BloomFilter::empty(),
        }
    }
}

/// Build bloom filter summaries for all local feeds.
/// Returns summaries for feeds with messages.
pub fn build_feed_bloom_summaries(
    feeds: &[(PublicId, u64)],
    get_hashes: impl Fn(&PublicId) -> Vec<String>,
    config: &BloomConfig,
) -> Vec<FeedBloomSummary> {
    feeds
        .iter()
        .map(|(author, latest_seq)| {
            let hashes = get_hashes(author);
            let hash_refs: Vec<&str> = hashes.iter().map(|s| s.as_str()).collect();
            FeedBloomSummary {
                author: author.clone(),
                latest_sequence: *latest_seq,
                filter: BloomFilter::from_hashes(hash_refs, config),
            }
        })
        .collect()
}

/// Check if we should request a message hash based on bloom filter.
/// Returns true if the hash is definitely not in the peer's filter,
/// or if no bloom filter is available.
pub fn should_request_hash(filter: Option<&BloomFilter>, hash: &str) -> bool {
    match filter {
        Some(f) => !f.might_contain(hash),
        None => true, // No filter = request everything
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bloom_filter_basic_operations() {
        let config = BloomConfig::default();
        let mut filter = BloomFilter::new(100, &config);

        filter.insert("hash1");
        filter.insert("hash2");
        filter.insert("hash3");

        assert!(filter.might_contain("hash1"));
        assert!(filter.might_contain("hash2"));
        assert!(filter.might_contain("hash3"));
        assert_eq!(filter.count(), 3);
    }

    #[test]
    fn bloom_filter_false_negatives_impossible() {
        let config = BloomConfig::default();
        let mut filter = BloomFilter::new(1000, &config);

        // Insert many hashes
        for i in 0..1000 {
            filter.insert(&format!("hash_{}", i));
        }

        // All inserted hashes must be found (no false negatives)
        for i in 0..1000 {
            assert!(
                filter.might_contain(&format!("hash_{}", i)),
                "False negative for hash_{}",
                i
            );
        }
    }

    #[test]
    fn bloom_filter_false_positives_within_bounds() {
        let config = BloomConfig::default();
        let mut filter = BloomFilter::new(100, &config);

        // Insert 100 hashes
        for i in 0..100 {
            filter.insert(&format!("inserted_hash_{}", i));
        }

        // Check 1000 hashes that were NOT inserted
        let mut false_positives = 0;
        for i in 0..1000 {
            if filter.might_contain(&format!("not_inserted_hash_{}", i)) {
                false_positives += 1;
            }
        }

        // With 1% target FPR and 1000 tests, expect ~10 false positives
        // Allow up to 5% (50) to account for variance
        assert!(
            false_positives < 50,
            "Too many false positives: {} (expected < 50)",
            false_positives
        );
    }

    #[test]
    fn bloom_filter_serialization_roundtrip() {
        let config = BloomConfig::default();
        let mut filter = BloomFilter::new(50, &config);

        filter.insert("hash_a");
        filter.insert("hash_b");

        let json = serde_json::to_string(&filter).unwrap();
        let restored: BloomFilter = serde_json::from_str(&json).unwrap();

        assert!(restored.might_contain("hash_a"));
        assert!(restored.might_contain("hash_b"));
        assert_eq!(restored.count(), 2);
    }

    #[test]
    fn bloom_filter_empty() {
        let filter = BloomFilter::empty();
        assert_eq!(filter.count(), 0);
        assert!(!filter.might_contain("anything"));
    }

    #[test]
    fn bloom_filter_from_hashes() {
        let config = BloomConfig::default();
        let hashes = vec!["hash1", "hash2", "hash3"];
        let filter = BloomFilter::from_hashes(hashes, &config);

        assert!(filter.might_contain("hash1"));
        assert!(filter.might_contain("hash2"));
        assert!(filter.might_contain("hash3"));
        assert_eq!(filter.count(), 3);
    }

    #[test]
    fn bloom_config_compact_smaller_than_default() {
        let compact = BloomConfig::compact();
        let default = BloomConfig::default();

        let (compact_bits, _) = compact.optimal_size(100);
        let (default_bits, _) = default.optimal_size(100);

        assert!(compact_bits <= default_bits);
    }

    #[test]
    fn bloom_config_low_bandwidth_larger_than_default() {
        let low_bw = BloomConfig::low_bandwidth();
        let default = BloomConfig::default();

        let (low_bw_bits, _) = low_bw.optimal_size(100);
        let (default_bits, _) = default.optimal_size(100);

        assert!(low_bw_bits >= default_bits);
    }

    #[test]
    fn feed_bloom_summary_serialization() {
        let config = BloomConfig::default();
        let mut filter = BloomFilter::new(10, &config);
        filter.insert("msg_hash_1");

        let summary = FeedBloomSummary {
            author: PublicId("@test.ed25519".to_string()),
            latest_sequence: 42,
            filter,
        };

        let json = serde_json::to_string(&summary).unwrap();
        let restored: FeedBloomSummary = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.author.0, "@test.ed25519");
        assert_eq!(restored.latest_sequence, 42);
        assert!(restored.filter.might_contain("msg_hash_1"));
    }

    #[test]
    fn should_request_hash_no_filter() {
        assert!(should_request_hash(None, "any_hash"));
    }

    #[test]
    fn should_request_hash_with_filter() {
        let config = BloomConfig::default();
        let mut filter = BloomFilter::new(10, &config);
        filter.insert("existing_hash");

        // Hash in filter = should not request
        assert!(!should_request_hash(Some(&filter), "existing_hash"));

        // Hash not in filter = should request
        assert!(should_request_hash(Some(&filter), "new_hash"));
    }

    #[test]
    fn estimated_false_positive_rate_increases_with_fill() {
        let config = BloomConfig::default();
        let mut filter = BloomFilter::new(100, &config);

        let empty_fpr = filter.estimated_false_positive_rate();
        assert_eq!(empty_fpr, 0.0);

        // Add some elements
        for i in 0..50 {
            filter.insert(&format!("hash_{}", i));
        }
        let half_fpr = filter.estimated_false_positive_rate();

        // Add more elements
        for i in 50..100 {
            filter.insert(&format!("hash_{}", i));
        }
        let full_fpr = filter.estimated_false_positive_rate();

        assert!(half_fpr > 0.0);
        assert!(full_fpr > half_fpr);
    }

    #[test]
    fn optimal_size_respects_bounds() {
        let config = BloomConfig {
            false_positive_rate: 0.01,
            max_bits: 1000,
            min_bits: 100,
        };

        // Very few elements should hit min
        let (bits_small, _) = config.optimal_size(1);
        assert!(bits_small >= 100);

        // Many elements should hit max
        let (bits_large, _) = config.optimal_size(100_000);
        assert!(bits_large <= 1000);
    }

    #[test]
    fn build_feed_bloom_summaries_works() {
        let config = BloomConfig::default();
        let feeds = vec![
            (PublicId("@alice.ed25519".to_string()), 3),
            (PublicId("@bob.ed25519".to_string()), 1),
        ];

        let get_hashes = |author: &PublicId| -> Vec<String> {
            if author.0 == "@alice.ed25519" {
                vec!["hash_a1".to_string(), "hash_a2".to_string()]
            } else {
                vec!["hash_b1".to_string()]
            }
        };

        let summaries = build_feed_bloom_summaries(&feeds, get_hashes, &config);

        assert_eq!(summaries.len(), 2);

        let alice = summaries.iter().find(|s| s.author.0 == "@alice.ed25519").unwrap();
        assert_eq!(alice.latest_sequence, 3);
        assert!(alice.filter.might_contain("hash_a1"));
        assert!(alice.filter.might_contain("hash_a2"));

        let bob = summaries.iter().find(|s| s.author.0 == "@bob.ed25519").unwrap();
        assert_eq!(bob.latest_sequence, 1);
        assert!(bob.filter.might_contain("hash_b1"));
    }
}
