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
                                let (entered, released) = signals.lock().unwrap().take().unwrap();
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
