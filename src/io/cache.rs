use std::time::Duration;

/// Tuning for the decoded-block cache. `max_bytes` bounds total resident
/// pixel memory across all cached blocks (moka evicts via TinyLFU). `ttl` is
/// opt-in; COG data is immutable so the default relies on capacity eviction.
#[derive(Clone, Copy, Debug)]
pub struct CacheConfig {
    pub max_bytes: u64,
    pub ttl: Option<Duration>,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_bytes: 256 << 20,
            ttl: None,
        }
    }
}
