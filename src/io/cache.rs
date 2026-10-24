use std::time::Duration;

/// Tuning for the decoded-block cache. `max_bytes` bounds total resident
/// pixel memory across all cached blocks (moka evicts via TinyLFU). `ttl` is
/// opt-in; COG data is immutable so the default relies on capacity eviction.
#[derive(Clone, Copy, Debug)]
pub struct CacheConfig {
    /// Resident decoded-pixel byte capacity; defaults to 512 MiB.
    /// Eviction is eventually consistent and does not account for caller-held
    /// clones or decode working buffers. A zero capacity disables retention.
    pub max_bytes: u64,
    /// Optional lifetime of an inserted decoded block. Defaults to `None`,
    /// relying on capacity eviction and immutable source identities.
    pub ttl: Option<Duration>,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_bytes: 512 << 20,
            ttl: None,
        }
    }
}
