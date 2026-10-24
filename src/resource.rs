//! Shared accounting for active library working buffers, separate from caches
//! and caller-owned completed outputs. Individual reservations stay nonblocking:
//! waiting while holding other allocations can deadlock a multi-stage pipeline.
//! Public tile requests use a shared admission gate. On memory pressure the
//! entire failed attempt unwinds before one exclusive retry, inside the original
//! request deadline. This drains competing requests without raising the byte cap.

use crate::error::{AgError, AgResult};
use crate::request::RequestContext;
use std::future::Future;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc, OnceLock,
};
use tokio::sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};

#[derive(Debug)]
struct State {
    limit: u64,
    used: AtomicU64,
    peak: AtomicU64,
    requests: Arc<RwLock<()>>,
}

/// Byte allowance shared by clones for active buffers and retained reader metadata.
///
/// Reservations remain charged until their last handle is dropped, including
/// after request cancellation while blocking work is still running. Cache
/// residency and completed images owned by callers have separate lifetimes and
/// are not covered by this allowance.
#[derive(Clone, Debug)]
pub struct WorkBudget(Arc<State>);

/// Retained by request contexts, including jobs that outlive cancellation.
#[derive(Debug)]
pub(crate) enum WorkAdmission {
    Shared { _permit: OwnedRwLockReadGuard<()> },
    Exclusive { _permit: OwnedRwLockWriteGuard<()> },
}

/// Clones retain one reservation, for example across a cancelled blocking job.
#[derive(Clone, Debug)]
pub struct WorkReservation(Arc<Lease>);

#[derive(Debug)]
struct Lease {
    state: Arc<State>,
    bytes: u64,
}

impl WorkBudget {
    /// Create an independent allowance with an exact byte limit.
    /// A zero limit permits only zero-byte reservations; it does not disable checks.
    pub fn new(limit_bytes: u64) -> Self {
        Self(Arc::new(State {
            limit: limit_bytes,
            used: AtomicU64::new(0),
            peak: AtomicU64::new(0),
            requests: Arc::new(RwLock::new(())),
        }))
    }

    /// Run only at a public request boundary, never inside a read/warp stage.
    /// A failed attempt must release all its buffers before waiting for the
    /// exclusive gate. Blocking jobs retain the shared guard via RequestContext,
    /// so the retry cannot overlap work left behind by a cancelled child future.
    /// Tokio's writer-preferring queue prevents new readers starving a retry.
    ///
    /// ResourceLimit may also denote an intrinsically oversized plan. One retry
    /// in isolation distinguishes that from contention without an unbounded loop;
    /// a second failure is returned unchanged. Other errors are never retried.
    pub(crate) async fn run_request<T, F, Fut>(
        &self,
        context: &RequestContext,
        work: F,
    ) -> AgResult<T>
    where
        F: Fn(RequestContext) -> Fut,
        Fut: Future<Output = AgResult<T>>,
    {
        context.check()?;
        let result = {
            let permit = self.0.requests.clone().read_owned().await;
            let attempt = context.with_work_admission(WorkAdmission::Shared { _permit: permit });
            attempt.check()?;
            attempt.scope(work(attempt.clone())).await
        };
        if !matches!(result, Err(AgError::ResourceLimit(_))) {
            return result;
        }
        // Do not upgrade a held read lock or keep a failed attempt's context.
        // Its last blocking job, if any, is allowed to finish before this wait.
        context.check()?;
        tracing::debug!(
            limit_bytes = self.0.limit,
            "retrying request after working-memory pressure"
        );
        let permit = self.0.requests.clone().write_owned().await;
        let attempt = context.with_work_admission(WorkAdmission::Exclusive { _permit: permit });
        attempt.check()?;
        attempt.scope(work(attempt.clone())).await
    }

    /// All default readers/tilers/pools share this 512 MiB working allowance.
    pub fn global() -> Self {
        static GLOBAL: OnceLock<WorkBudget> = OnceLock::new();
        GLOBAL.get_or_init(|| Self::new(512 * 1024 * 1024)).clone()
    }

    /// Return the fixed byte limit shared by all clones of this budget.
    pub fn limit_bytes(&self) -> u64 {
        self.0.limit
    }
    /// Snapshot bytes held by live reservations, including retained reader metadata.
    pub fn used_bytes(&self) -> u64 {
        self.0.used.load(Ordering::Relaxed)
    }
    /// Snapshot the highest reserved byte total since this budget was created.
    pub fn peak_bytes(&self) -> u64 {
        self.0.peak.load(Ordering::Relaxed)
    }

    /// Reserve bytes immediately without waiting. Cloning the returned handle
    /// retains the same charge; the final handle's drop releases it.
    ///
    /// # Errors
    /// Returns [`AgError::ResourceLimit`] when the request exceeds the currently
    /// available allowance or the total cannot be represented. No bytes are
    /// reserved on failure.
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
    /// Return this reservation's byte charge, shared rather than multiplied by clones.
    pub fn bytes(&self) -> u64 {
        self.0.bytes
    }
}

#[cfg(test)]
mod tests {
    //! Deterministic tests for request-level memory backpressure.
    use super::*;
    use crate::request::{RequestCancellation, RequestLimiter, RequestOptions};
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    #[tokio::test]
    async fn concurrent_multistage_requests_unwind_before_exclusive_retry() {
        let budget = WorkBudget::new(100);
        let limiter = RequestLimiter::new(2);
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let total_attempts = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..2 {
            let budget = budget.clone();
            let limiter = limiter.clone();
            let barrier = barrier.clone();
            let total_attempts = total_attempts.clone();
            tasks.push(tokio::spawn(async move {
                let attempts = AtomicUsize::new(0);
                limiter
                    .run(
                        Duration::from_secs(2),
                        &RequestOptions::default(),
                        |context| {
                            let budget = &budget;
                            let barrier = &barrier;
                            let total_attempts = &total_attempts;
                            let attempts = &attempts;
                            async move {
                                budget
                                    .run_request(&context, |_| async {
                                        total_attempts.fetch_add(1, Ordering::Relaxed);
                                        let attempt = attempts.fetch_add(1, Ordering::Relaxed);
                                        let _source = budget.try_reserve(40)?;
                                        if attempt == 0 {
                                            barrier.wait().await;
                                        }
                                        // Both initial attempts hold 40 bytes. Neither may wait
                                        // for 40 more while retaining its original allocation.
                                        let _render = budget.try_reserve(40)?;
                                        tokio::task::yield_now().await;
                                        Ok(())
                                    })
                                    .await
                            }
                        },
                    )
                    .await
            }));
        }
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        assert!((3..=4).contains(&total_attempts.load(Ordering::Relaxed)));
        assert_eq!(budget.used_bytes(), 0);
        assert!(budget.peak_bytes() <= 100);
        assert_eq!(limiter.available_permits(), 2);
        assert!(budget.0.requests.try_write().is_ok());
    }

    #[tokio::test]
    async fn uncontended_requests_remain_parallel() {
        let budget = WorkBudget::new(100);
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let limiter = RequestLimiter::new(2);
        let mut tasks = Vec::new();
        for _ in 0..2 {
            let (budget, barrier, limiter) = (budget.clone(), barrier.clone(), limiter.clone());
            tasks.push(tokio::spawn(async move {
                limiter
                    .run(
                        Duration::from_secs(2),
                        &RequestOptions::default(),
                        |context| {
                            let budget = &budget;
                            let barrier = &barrier;
                            async move {
                                budget
                                    .run_request(&context, |_| async {
                                        let _work = budget.try_reserve(40)?;
                                        barrier.wait().await;
                                        Ok(())
                                    })
                                    .await
                            }
                        },
                    )
                    .await
            }));
        }
        for task in tasks {
            task.await.unwrap().unwrap();
        }
        assert_eq!(budget.peak_bytes(), 80);
        assert_eq!(budget.used_bytes(), 0);
    }

    #[tokio::test]
    async fn permanent_limits_are_retried_at_most_once_and_other_errors_never_retry() {
        let budget = WorkBudget::new(100);
        let limiter = RequestLimiter::new(1);
        let calls = AtomicUsize::new(0);
        let result = limiter
            .run(
                Duration::from_secs(2),
                &RequestOptions::default(),
                |context| {
                    let budget = &budget;
                    let calls = &calls;
                    async move {
                        budget
                            .run_request(&context, |_| async {
                                calls.fetch_add(1, Ordering::Relaxed);
                                budget.try_reserve(101).map(|_| ())
                            })
                            .await
                    }
                },
            )
            .await;
        assert!(matches!(result, Err(AgError::ResourceLimit(_))));
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        assert_eq!(budget.used_bytes(), 0);
        calls.store(0, Ordering::Relaxed);
        let result: AgResult<()> = limiter
            .run(
                Duration::from_secs(2),
                &RequestOptions::default(),
                |context| {
                    let budget = &budget;
                    let calls = &calls;
                    async move {
                        budget
                            .run_request(&context, |_| async {
                                calls.fetch_add(1, Ordering::Relaxed);
                                Err(AgError::Tiff("fixture error".into()))
                            })
                            .await
                    }
                },
            )
            .await;
        assert!(matches!(result, Err(AgError::Tiff(_))));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(limiter.available_permits(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn original_deadline_includes_exclusive_queue_wait() {
        let budget = WorkBudget::new(100);
        let limiter = RequestLimiter::new(1);
        let held = budget.0.requests.clone().read_owned().await;
        let calls = AtomicUsize::new(0);
        let result: AgResult<()> = limiter
            .run(
                Duration::from_secs(1),
                &RequestOptions::default(),
                |context| {
                    let budget = &budget;
                    let calls = &calls;
                    async move {
                        budget
                            .run_request(&context, |_| async {
                                calls.fetch_add(1, Ordering::Relaxed);
                                Err(AgError::ResourceLimit("forced contention".into()))
                            })
                            .await
                    }
                },
            )
            .await;
        assert!(matches!(result, Err(AgError::DeadlineExceeded)));
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        assert_eq!(limiter.available_permits(), 1);
        // A cancelled writer must not prevent future readers proceeding.
        assert!(budget.0.requests.try_read().is_ok());
        drop(held);
        assert!(budget.0.requests.try_write().is_ok());
        assert_eq!(budget.used_bytes(), 0);
    }

    #[tokio::test]
    async fn explicit_cancellation_removes_queued_retry_and_returns_admission() {
        let budget = WorkBudget::new(100);
        let limiter = RequestLimiter::new(1);
        let held = budget.0.requests.clone().read_owned().await;
        let cancellation = RequestCancellation::default();
        let (entered, waiting) = tokio::sync::oneshot::channel();
        let entered = std::sync::Mutex::new(Some(entered));
        let (worker_budget, worker_limiter, worker_cancel) =
            (budget.clone(), limiter.clone(), cancellation.clone());
        let task = tokio::spawn(async move {
            worker_limiter
                .run(
                    Duration::from_secs(2),
                    &RequestOptions {
                        cancellation: worker_cancel,
                        timeout: None,
                    },
                    |context| {
                        let worker_budget = &worker_budget;
                        let entered = &entered;
                        async move {
                            worker_budget
                                .run_request(&context, |_| async {
                                    entered.lock().unwrap().take().unwrap().send(()).unwrap();
                                    Err::<(), _>(AgError::ResourceLimit("forced contention".into()))
                                })
                                .await
                        }
                    },
                )
                .await
        });
        waiting.await.unwrap();
        cancellation.cancel();
        assert!(matches!(task.await.unwrap(), Err(AgError::Cancelled)));
        assert_eq!(limiter.available_permits(), 1);
        assert!(budget.0.requests.try_read().is_ok());
        drop(held);
        assert!(budget.0.requests.try_write().is_ok());
    }

    #[tokio::test]
    async fn cancelled_blocking_job_retains_memory_gate_until_actual_exit() {
        let budget = WorkBudget::new(100);
        let limiter = RequestLimiter::new(1);
        let (entered, waiting) = tokio::sync::oneshot::channel();
        let (release, released) = std::sync::mpsc::channel();
        let signals = std::sync::Mutex::new(Some((entered, released)));
        let (worker_budget, worker_limiter) = (budget.clone(), limiter.clone());
        let task = tokio::spawn(async move {
            worker_limiter
                .run(
                    Duration::from_secs(5),
                    &RequestOptions::default(),
                    |context| {
                        let worker_budget = &worker_budget;
                        let signals = &signals;
                        async move {
                            worker_budget
                                .run_request(&context, |_| async {
                                    let request = RequestContext::current()
                                        .unwrap()
                                        .with_work(worker_budget.try_reserve(70)?);
                                    let (entered, released) =
                                        signals.lock().unwrap().take().unwrap();
                                    tokio::task::spawn_blocking(move || {
                                        RequestContext::run_blocking(Some(request), || {
                                            entered.send(()).unwrap();
                                            released.recv_timeout(Duration::from_secs(3)).unwrap();
                                            Ok(())
                                        })
                                    })
                                    .await
                                    .unwrap()
                                })
                                .await
                        }
                    },
                )
                .await
        });
        waiting.await.unwrap();
        task.abort();
        let _ = task.await;
        assert_eq!(budget.used_bytes(), 70);
        assert_eq!(limiter.available_permits(), 0);
        assert!(budget.0.requests.try_write().is_err());
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while budget.used_bytes() != 0
                || limiter.available_permits() != 1
                || budget.0.requests.try_write().is_err()
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

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
