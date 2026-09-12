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

tokio::task_local! {
    // Synchronous jobs explicitly clone this before leaving the request task.
    // Direct reader calls outside a tile request have no request admission.
    static CURRENT_REQUEST: RequestContext;
}

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
            result = tokio::time::timeout_at(
                deadline,
                CURRENT_REQUEST.scope(context.clone(), work(context.clone())),
            ) => {
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
    pub(crate) fn current() -> Option<Self> {
        CURRENT_REQUEST.try_with(Clone::clone).ok()
    }

    /// Execute synchronous work while retaining its originating request.
    /// Capture `current()` before spawning: task locals do not follow spawned
    /// tasks or blocking threads. A cancelled queued job must not start work.
    pub(crate) fn run_blocking<T>(
        request: Option<Self>,
        work: impl FnOnce() -> AgResult<T>,
    ) -> AgResult<T> {
        if let Some(request) = &request {
            request.check()?;
        }
        let result = work();
        if let Some(request) = &request {
            request.check()?;
        }
        result
    }

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

    #[tokio::test]
    async fn cancelled_queued_job_does_not_run_its_work() {
        let limiter = RequestLimiter::new(1);
        let worker_limiter = limiter.clone();
        let (captured_tx, captured_rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            worker_limiter
                .run(
                    Duration::from_secs(30),
                    &RequestOptions::default(),
                    |_| async {
                        captured_tx.send(RequestContext::current()).ok().unwrap();
                        std::future::pending::<AgResult<()>>().await
                    },
                )
                .await
        });
        let request = captured_rx.await.unwrap();
        assert!(request.is_some());
        task.abort();
        let _ = task.await;
        assert_eq!(limiter.available_permits(), 0);
        let result: AgResult<()> = RequestContext::run_blocking(request, || {
            panic!("work queued by a cancelled request must not start")
        });
        assert!(matches!(result, Err(AgError::Cancelled)));
        assert_eq!(limiter.available_permits(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn expired_decode_owner_does_not_fail_a_healthy_waiter() {
        use crate::{
            io::{CogReader, ReaderConfig},
            CpuLimiter,
        };
        use object_store::{memory::InMemory, path::Path, ObjectStoreExt};

        let store = Arc::new(InMemory::new());
        store
            .put(
                &Path::from("fixture"),
                bytes::Bytes::from_static(include_bytes!("../tests/fixtures/rgb_nodata.tif"))
                    .into(),
            )
            .await
            .unwrap();
        let cpu = CpuLimiter::new(1);
        let config = ReaderConfig {
            cpu_limiter: Some(cpu.clone()),
            ..ReaderConfig::default()
        };
        let cache = config.block_cache.clone();
        let reader = Arc::new(
            CogReader::builder()
                .reader_config(config)
                .open_object_store(store, "fixture")
                .await
                .unwrap(),
        );
        let limiter = RequestLimiter::new(1);
        let request = RequestContext {
            _admission: Arc::new(limiter.0.clone().try_acquire_owned().unwrap()),
            cancellation: RequestCancellation::default(),
            deadline: Instant::now() + Duration::from_secs(1),
            work: Vec::new(),
        };
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        // Occupy the CPU permit, so the owner claims the block before its
        // deadline but cannot decode until after it. Keep the owner future
        // alive to exercise the race before its outer timeout gets polled.
        let permit = cpu.acquire().await.unwrap();
        let blocker = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            started_tx.send(()).unwrap();
            let _ = release_rx.recv_timeout(Duration::from_secs(5));
        });
        started_rx.await.unwrap();
        let owner_reader = reader.clone();
        let owner = tokio::spawn(CURRENT_REQUEST.scope(request, async move {
            owner_reader.read_window(0, 0, 0, 1, 1).await
        }));
        while cache.stats().fetched == 0 {
            tokio::task::yield_now().await;
        }
        let waiter = tokio::spawn(async move { reader.read_window(0, 0, 0, 1, 1).await });
        while cache.stats().misses < 2 {
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs(2)).await;
        release_tx.send(()).unwrap();
        blocker.await.unwrap();
        assert!(matches!(
            owner.await.unwrap(),
            Err(AgError::DeadlineExceeded)
        ));
        let image = waiter
            .await
            .unwrap()
            .expect("healthy waiter must retry the expired owner's claim");
        assert_eq!(image.pixels.to_f64()[[0, 0, 0]], 1.0);
        assert_eq!(limiter.available_permits(), 1);
    }

    #[tokio::test]
    async fn cancelled_http_initialization_retains_request_admission() {
        let limiter = RequestLimiter::new(1);
        let worker_limiter = limiter.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let task = tokio::spawn(async move {
            worker_limiter
                .run(Duration::from_secs(30), &RequestOptions::default(), |_| {
                    crate::io::cog::build_http_transport(move || {
                        started_tx.send(()).unwrap();
                        let _ = release_rx.recv_timeout(Duration::from_secs(5));
                        Ok(())
                    })
                })
                .await
        });
        started_rx.await.unwrap();
        task.abort();
        let _ = task.await;
        let available_while_running = limiter.available_permits();
        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while limiter.available_permits() != 1 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(available_while_running, 0);
    }

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
