//! Shared defaults for CPU-heavy work dispatched with `spawn_blocking`.

pub(crate) fn default_blocking_permits() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .max(1)
}
