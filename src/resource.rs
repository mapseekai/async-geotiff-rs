//! Shared accounting for active library working buffers, separate from caches
//! and caller-owned completed outputs. Reservations reject overload immediately:
//! waiting while holding other allocations can deadlock a multi-stage pipeline.

use crate::error::{AgError, AgResult};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, OnceLock,
};

#[derive(Debug)]
struct State {
    limit: u64,
    used: AtomicU64,
    peak: AtomicU64,
}

#[derive(Clone, Debug)]
pub struct WorkBudget(Arc<State>);

/// Clones retain one reservation, for example across a cancelled blocking job.
#[derive(Clone, Debug)]
pub struct WorkReservation(Arc<Lease>);

#[derive(Debug)]
struct Lease {
    state: Arc<State>,
    bytes: u64,
}

impl WorkBudget {
    pub fn new(limit_bytes: u64) -> Self {
        Self(Arc::new(State {
            limit: limit_bytes,
            used: AtomicU64::new(0),
            peak: AtomicU64::new(0),
        }))
    }

    /// All default readers/tilers/pools share this 512 MiB working allowance.
    pub fn global() -> Self {
        static GLOBAL: OnceLock<WorkBudget> = OnceLock::new();
        GLOBAL.get_or_init(|| Self::new(512 * 1024 * 1024)).clone()
    }

    pub fn limit_bytes(&self) -> u64 {
        self.0.limit
    }
    pub fn used_bytes(&self) -> u64 {
        self.0.used.load(Ordering::Relaxed)
    }
    pub fn peak_bytes(&self) -> u64 {
        self.0.peak.load(Ordering::Relaxed)
    }

    pub fn try_reserve(&self, bytes: u64) -> AgResult<WorkReservation> {
        let previous = self
            .0
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Relaxed, |used| {
                used.checked_add(bytes).filter(|&n| n <= self.0.limit)
            })
            .map_err(|_| {
                AgError::ResourceLimit(format!(
                    "working memory request {bytes} exceeds available budget (limit {})",
                    self.0.limit
                ))
            })?;
        self.0.peak.fetch_max(previous + bytes, Ordering::Relaxed);
        Ok(WorkReservation(Arc::new(Lease {
            state: self.0.clone(),
            bytes,
        })))
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.state.used.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

impl WorkReservation {
    pub fn bytes(&self) -> u64 {
        self.0.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reservations_are_shared_and_released_only_after_last_owner() {
        let budget = WorkBudget::new(100);
        let lease = budget.try_reserve(70).unwrap();
        assert!(budget.clone().try_reserve(31).is_err());
        let retained = lease.clone();
        drop(lease);
        assert_eq!(budget.used_bytes(), 70);
        drop(retained);
        assert_eq!(budget.used_bytes(), 0);
        assert!(budget.try_reserve(100).is_ok());
        assert_eq!(budget.peak_bytes(), 100);
    }

    #[test]
    fn oversized_and_overflowing_requests_fail_without_charging() {
        let budget = WorkBudget::new(100);
        assert!(budget.try_reserve(u64::MAX).is_err());
        assert_eq!(budget.used_bytes(), 0);
    }
}
