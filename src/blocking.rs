//! Shared defaults and a shared permit pool for CPU-heavy work dispatched
//! with `spawn_blocking` (decode, stitch, warp, render).

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::error::{AgError, AgResult};

pub(crate) fn default_blocking_permits() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .max(1)
}

/// A shared bound on concurrent CPU-heavy blocking jobs.
///
/// One `CpuLimiter` can be cloned and handed to many [`CogReader`]s and
/// [`Tiler`]s so the *whole process* (or a whole mosaic) shares a single CPU
/// budget instead of each reader/tiler getting its own semaphore. Without this,
/// a mosaic that opens N assets would allow N × `available_parallelism` decode
/// jobs plus N independent warp pools, oversubscribing the machine and thrashing
/// caches under load.
///
/// Cloning is cheap (an `Arc` bump); all clones draw permits from the same pool.
/// Each default construction creates an independent pool sized to available
/// parallelism, or four permits if that information is unavailable. Running
/// blocking jobs retain their permits after request cancellation until they exit.
///
/// **Deadlock-freedom invariant:** a job must never hold one permit while
/// awaiting a second from the same limiter. Acquire → `spawn_blocking` → drop
/// must be flat, not nested. The tile pipeline upholds this by releasing a
/// reader's decode permit (inside its own `spawn_blocking`) before the warp/
/// render permit is acquired — see `Tiler::run_tile_cpu`.
///
/// [`CogReader`]: crate::io::CogReader
/// [`Tiler`]: crate::tiler::Tiler
#[derive(Clone, Debug)]
pub struct CpuLimiter(Arc<Semaphore>);

impl CpuLimiter {
    /// Build an independent limiter allowing `permits` concurrent blocking jobs,
    /// clamped to `1..=tokio::sync::Semaphore::MAX_PERMITS`.
    pub fn new(permits: usize) -> Self {
        Self(Arc::new(Semaphore::new(
            permits.clamp(1, Semaphore::MAX_PERMITS),
        )))
    }

    /// Acquire one permit, awaited asynchronously so excess work yields the
    /// Tokio worker instead of flooding the blocking pool. Hold the returned
    /// permit for the duration of the `spawn_blocking` job.
    pub(crate) async fn acquire(&self) -> AgResult<OwnedSemaphorePermit> {
        Arc::clone(&self.0)
            .acquire_owned()
            .await
            .map_err(|_| AgError::Warp("cpu limiter closed".into()))
    }

    /// Current number of free permits (diagnostics/tests).
    pub fn available_permits(&self) -> usize {
        self.0.available_permits()
    }
}

impl Default for CpuLimiter {
    fn default() -> Self {
        Self::new(default_blocking_permits())
    }
}

#[cfg(test)]
mod tests {
    use super::CpuLimiter;

    #[test]
    fn new_clamps_zero_permits_to_one() {
        assert_eq!(CpuLimiter::new(0).available_permits(), 1);
        assert_eq!(CpuLimiter::new(4).available_permits(), 4);
    }

    #[test]
    fn oversized_cpu_limit_does_not_panic() {
        assert_eq!(
            CpuLimiter::new(usize::MAX).available_permits(),
            tokio::sync::Semaphore::MAX_PERMITS
        );
    }

    #[tokio::test]
    async fn clones_share_one_permit_pool() {
        let a = CpuLimiter::new(1);
        let b = a.clone();
        let permit = a.acquire().await.unwrap();
        // The clone draws from the same pool: no permits left.
        assert_eq!(b.available_permits(), 0);
        drop(permit);
        assert_eq!(b.available_permits(), 1);
    }
}
