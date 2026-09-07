//! Request admission, deadline, and cooperative cancellation.
//!
//! Admission rejects excess requests immediately instead of building an unbounded
//! queue. Clones share one limit. Dropping a request cancels its later stages;
//! an already-running blocking job retains its admission until it actually ends.
//!
//! Public tile and warp calls default to a 30-second deadline. Configure defaults
//! through `TilerConfig` / `MosaicConfig`, or pass `RequestOptions` to a
//! `*_with_options` call. Cancelling its handle, timing out, or dropping that
//! future cancels later stages. Synchronous kernels are cooperative at stage
//! boundaries, so cancellation does not preempt a running codec or warp kernel.
//! CPU, admission and working-byte permits remain held until that kernel ends.
//! Returned images/bytes belong to the caller and are outside active-work limits.

use crate::error::{AgError, AgResult};
use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_MAX_IN_FLIGHT_REQUESTS: usize = 64;

/// Shared admission limit. Defaults share a process-wide pool of 64 requests.
#[derive(Clone, Debug)]
pub struct RequestLimiter(Arc<Semaphore>);

impl RequestLimiter {
    pub fn new(max_in_flight: usize) -> Self {
        Self(Arc::new(Semaphore::new(
            max_in_flight.clamp(1, Semaphore::MAX_PERMITS),
        )))
    }

    pub fn available_permits(&self) -> usize {
        self.0.available_permits()
    }

    pub(crate) async fn run<T, F, Fut>(
        &self,
        timeout: Duration,
        options: &RequestOptions,
        work: F,
    ) -> AgResult<T>
    where
        F: FnOnce(RequestContext) -> Fut,
        Fut: Future<Output = AgResult<T>>,
    {
        if options.cancellation.is_cancelled() {
            return Err(AgError::Cancelled);
        }
        let permit = self
            .0
            .clone()
            .try_acquire_owned()
            .map_err(|_| AgError::Overloaded)?;
        let deadline = Instant::now()
            .checked_add(options.timeout.unwrap_or(timeout))
            .ok_or_else(|| AgError::query("timeout", "duration exceeds clock range"))?;
        let context = RequestContext {
            _admission: Arc::new(permit),
            cancellation: RequestCancellation::default(),
            deadline,
            work: Vec::new(),
        };
        let _guard = CancelOnDrop(context.cancellation.clone());
        context.check()?;
        tokio::select! {
            biased;
            _ = options.cancellation.cancelled() => Err(AgError::Cancelled),
            result = tokio::time::timeout_at(deadline, work(context.clone())) => {
                result.map_err(|_| AgError::DeadlineExceeded)?
            }
        }
    }
}

impl Default for RequestLimiter {
    fn default() -> Self {
        static SHARED: OnceLock<RequestLimiter> = OnceLock::new();
        SHARED
            .get_or_init(|| Self::new(DEFAULT_MAX_IN_FLIGHT_REQUESTS))
            .clone()
    }
}

/// Caller-controlled cancellation handle. Cancelling any clone cancels them all.
#[derive(Clone, Debug)]
pub struct RequestCancellation(watch::Sender<bool>);

impl Default for RequestCancellation {
    fn default() -> Self {
        Self(watch::channel(false).0)
    }
}

impl RequestCancellation {
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }
    pub fn is_cancelled(&self) -> bool {
        *self.0.borrow()
    }
    async fn cancelled(&self) {
        let mut receiver = self.0.subscribe();
        let _ = receiver.wait_for(|cancelled| *cancelled).await;
    }
}

/// Per-call overrides. `timeout: None` uses the tiler configuration.
#[derive(Clone, Debug, Default)]
pub struct RequestOptions {
    pub timeout: Option<Duration>,
    pub cancellation: RequestCancellation,
}

#[derive(Clone)]
pub(crate) struct RequestContext {
    _admission: Arc<OwnedSemaphorePermit>,
    cancellation: RequestCancellation,
    deadline: Instant,
    work: Vec<crate::resource::WorkReservation>,
}

impl RequestContext {
    pub(crate) fn with_work(&self, reservation: crate::resource::WorkReservation) -> Self {
        let mut next = self.clone();
        next.work.push(reservation);
        next
    }

    pub(crate) fn check(&self) -> AgResult<()> {
        if self.cancellation.is_cancelled() {
            return Err(AgError::Cancelled);
        }
        if Instant::now() >= self.deadline {
            return Err(AgError::DeadlineExceeded);
        }
        Ok(())
    }
}

struct CancelOnDrop(RequestCancellation);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn stalled_read_is_bounded_by_request_deadline() {
        let limiter = RequestLimiter::new(1);
        let result: AgResult<()> = limiter
            .run(Duration::from_secs(30), &RequestOptions::default(), |_| {
                std::future::pending()
            })
            .await;
        assert!(matches!(result, Err(AgError::DeadlineExceeded)));
        assert_eq!(limiter.available_permits(), 1);
    }

    #[tokio::test]
    async fn admission_rejects_excess_without_starting_work() {
        let limiter = RequestLimiter::new(1);
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let first_limiter = limiter.clone();
        let first = tokio::spawn(async move {
            first_limiter
                .run(
                    Duration::from_secs(30),
                    &RequestOptions::default(),
                    |_| async {
                        entered_tx.send(()).unwrap();
                        std::future::pending::<AgResult<()>>().await
                    },
                )
                .await
        });
        entered_rx.await.unwrap();
        let result: AgResult<()> = limiter
            .run(
                Duration::from_secs(30),
                &RequestOptions::default(),
                |_| async { panic!("overloaded request must not start") },
            )
            .await;
        assert!(matches!(result, Err(AgError::Overloaded)));
        first.abort();
        let _ = first.await;
        assert_eq!(limiter.available_permits(), 1);
    }

    #[tokio::test]
    async fn cancellation_keeps_admission_until_blocking_job_stops() {
        let limiter = RequestLimiter::new(1);
        let first_limiter = limiter.clone();
        let budget = crate::resource::WorkBudget::new(100);
        let task_budget = budget.clone();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let first = tokio::spawn(async move {
            first_limiter
                .run(
                    Duration::from_secs(30),
                    &RequestOptions::default(),
                    |context| async move {
                        let context = context.with_work(task_budget.try_reserve(70).unwrap());
                        tokio::task::spawn_blocking(move || {
                            entered_tx.send(()).unwrap();
                            release_rx.recv().unwrap();
                            assert!(matches!(context.check(), Err(AgError::Cancelled)));
                        })
                        .await
                        .unwrap();
                        Ok(())
                    },
                )
                .await
        });
        entered_rx.await.unwrap();
        first.abort();
        let _ = first.await;
        assert_eq!(limiter.available_permits(), 0);
        assert_eq!(budget.used_bytes(), 70);
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while limiter.available_permits() != 1 || budget.used_bytes() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("blocking admission and bytes must be released");
    }

    #[tokio::test]
    async fn explicit_cancellation_stops_stalled_request() {
        let limiter = RequestLimiter::new(1);
        let options = RequestOptions::default();
        options.cancellation.cancel();
        let result: AgResult<()> = limiter
            .run(Duration::from_secs(30), &options, |_| {
                std::future::pending()
            })
            .await;
        assert!(matches!(result, Err(AgError::Cancelled)));
    }
}
