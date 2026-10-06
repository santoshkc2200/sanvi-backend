//! Per-key single-flight: one loader runs per expired key, every concurrent
//! miss waits for it and re-reads the cache (TASK-021 11.2g — stampede
//! protection on the expensive shared keys, verified at 500 concurrent
//! misses on one key).
//!
//! The dedup is process-local by design: it collapses the herd that a cache
//! expiry produces *within* one replica. Cross-instance herds are bounded by
//! the shared Redis layer they all write back to — one database read per
//! replica per expiry, which is the floor any correct cache-aside has.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::watch;

struct Flight {
    /// Set before the watch send, so a waiter that observes either is safe
    /// to stop waiting (the ordering closes the subscribe/send race: a
    /// waiter only awaits the watch after re-checking `done` false, and the
    /// leader sends strictly after storing `done`).
    done: AtomicBool,
    completed: watch::Sender<bool>,
}

/// The per-key in-flight map. Clone is cheap: the map is shared.
#[derive(Clone, Default)]
pub struct SingleFlight {
    flights: Arc<Mutex<HashMap<String, Arc<Flight>>>>,
}

impl SingleFlight {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take the leader slot for `key`. `None` means a load for the key is
    /// already in flight — the caller should [`SingleFlight::wait`] and then
    /// re-read the cache.
    pub fn try_lead(&self, key: &str) -> Option<FlightGuard> {
        let mut flights = self.flights.lock().expect("single-flight lock poisoned");
        if flights
            .get(key)
            .is_some_and(|existing| !existing.done.load(Ordering::Acquire))
        {
            return None;
        }
        let (completed, _) = watch::channel(false);
        let flight = Arc::new(Flight {
            done: AtomicBool::new(false),
            completed,
        });
        flights.insert(key.to_string(), flight.clone());
        Some(FlightGuard {
            key: key.to_string(),
            flight,
            flights: Arc::clone(&self.flights),
        })
    }

    /// Wait until the in-flight load for `key` completes. Returns at once
    /// when nothing is in flight (including after the flight already
    /// finished — the caller's cache re-read decides what is next).
    pub async fn wait(&self, key: &str) {
        loop {
            let (done, mut rx) = {
                let flights = self.flights.lock().expect("single-flight lock poisoned");
                match flights.get(key) {
                    None => return,
                    Some(flight) => {
                        let done = flight.done.load(Ordering::Acquire);
                        // Subscribe before the done check so a flight that
                        // completes between the two reads still wakes us.
                        let rx = flight.completed.subscribe();
                        (done, rx)
                    }
                }
            };
            if done {
                return;
            }
            if rx.changed().await.is_err() {
                // Sender gone: the flight left the map — re-loop to observe
                // that (or any replacement's state).
                continue;
            }
        }
    }

    /// The canonical cache-aside loop the stampede gate tests: check the
    /// cache, lead-or-wait, load once, fill, return. Waiters exit through
    /// the cache re-read; a leader that fails to fill (load error) releases
    /// the flight and the next waiter leads its own attempt — fail-closed,
    /// never a fabricated value.
    ///
    /// The callbacks are boxed-future functions rather than async closures:
    /// an `AsyncFn` bound's `Send`-for-all-lifetimes proof is beyond rustc's
    /// inference at the async-trait boundary (the `FeatureGate` port), while
    /// `dyn Future + Send` states the requirement instead of inferring it.
    /// All three borrow their environment for `'a` — the caller's own scope.
    pub async fn get_or_load<'a, T, E>(
        &'a self,
        key: &str,
        cache_get: impl Fn() -> std::pin::Pin<Box<dyn Future<Output = Option<T>> + Send + 'a>>
        + Send
        + Sync
        + 'a,
        cache_set: impl Fn(T) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send + 'a>>
        + Send
        + Sync
        + 'a,
        load: impl Fn() -> std::pin::Pin<Box<dyn Future<Output = Result<T, E>> + Send + 'a>>
        + Send
        + Sync
        + 'a,
    ) -> Result<T, E>
    where
        T: Clone + Send + Sync + 'a,
        E: Send + 'a,
    {
        if let Some(hit) = cache_get().await {
            return Ok(hit);
        }
        loop {
            if let Some(_leader) = self.try_lead(key) {
                // The guard lives to the end of this block: leadership is
                // held across the load and the cache fill, so waiters stay
                // parked until the value is actually available.
                // Another caller may have filled the cache between the
                // first read and taking leadership — re-read before paying
                // for the load.
                if let Some(hit) = cache_get().await {
                    return Ok(hit);
                }
                let value = load().await?;
                cache_set(value.clone()).await;
                return Ok(value); // guard drop completes the flight
            }
            self.wait(key).await;
            if let Some(hit) = cache_get().await {
                return Ok(hit);
            }
        }
    }
}

/// Leadership of one key's load. Dropping it completes the flight and wakes
/// every waiter; holding it past the load's completion is the only way to
/// keep waiters parked.
pub struct FlightGuard {
    key: String,
    flight: Arc<Flight>,
    flights: Arc<Mutex<HashMap<String, Arc<Flight>>>>,
}

impl Drop for FlightGuard {
    fn drop(&mut self) {
        self.flight.done.store(true, Ordering::Release);
        let _ = self.flight.completed.send(true);
        let mut flights = self.flights.lock().expect("single-flight lock poisoned");
        // Only remove our own flight; a newer one may have replaced a
        // finished entry (cannot happen while we hold leadership, but the
        // pointer check costs nothing and keeps drop order-free).
        if flights
            .get(&self.key)
            .is_some_and(|current| Arc::ptr_eq(current, &self.flight))
        {
            flights.remove(&self.key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[tokio::test]
    async fn stampede_protection_holds_at_500_concurrent_misses() {
        let flights = Arc::new(SingleFlight::new());
        // The shared "cache": None until a loader fills it — every task
        // starts as a genuine miss on the same key.
        let cache: Arc<Mutex<Option<u64>>> = Arc::new(Mutex::new(None));
        let loads = Arc::new(AtomicUsize::new(0));

        let mut futures = Vec::with_capacity(500);
        for i in 0..500 {
            let flights = Arc::clone(&flights);
            let cache = Arc::clone(&cache);
            let loads = Arc::clone(&loads);
            futures.push(async move {
                flights
                    .get_or_load(
                        "one:key",
                        || Box::pin(async { *cache.lock().unwrap() }),
                        |value| {
                            let cache = Arc::clone(&cache);
                            Box::pin(async move { *cache.lock().unwrap() = Some(value) })
                                as std::pin::Pin<Box<dyn Future<Output = ()> + Send>>
                        },
                        || {
                            Box::pin(async {
                                loads.fetch_add(1, Ordering::SeqCst);
                                // Hold the flight long enough for the whole
                                // herd to pile up behind it.
                                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                                Ok::<u64, String>(i)
                            })
                        },
                    )
                    .await
                    .expect("load succeeds")
            });
        }
        // One thread, 500 tasks polled concurrently: every miss piles onto
        // the single flight before the leader's 50 ms window closes.
        let results = futures_util::future::join_all(futures).await;
        for value in results {
            assert_eq!(value, 0, "every waiter receives the leader's value");
        }
        assert_eq!(
            loads.load(Ordering::SeqCst),
            1,
            "500 concurrent misses on one key must produce exactly one load"
        );
        assert_eq!(*cache.lock().unwrap(), Some(0), "the cache was filled");
    }

    #[tokio::test]
    async fn finished_flights_lead_again() {
        let flights = SingleFlight::new();
        {
            let _guard = flights.try_lead("k").expect("first leader");
        }
        let _second = flights.try_lead("k").expect("finished flight replaced");
        assert!(flights.try_lead("k").is_none(), "in flight blocks");
    }

    #[tokio::test]
    async fn wait_returns_when_nothing_is_in_flight() {
        let flights = SingleFlight::new();
        flights.wait("never-led").await;
        let guard = flights.try_lead("k").expect("leader");
        guard.drop_after_completion();
        flights.wait("k").await;
    }

    #[tokio::test]
    async fn waiter_observes_completion_while_parked() {
        let flights = Arc::new(SingleFlight::new());
        let leader = flights.try_lead("k").expect("leader");
        let waiter = {
            let flights = Arc::clone(&flights);
            tokio::spawn(async move { flights.wait("k").await })
        };
        // Give the waiter time to park on the watch, then complete.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        leader.drop_after_completion();
        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .expect("waiter wakes on completion")
            .expect("waiter joins");
    }

    #[tokio::test]
    async fn get_or_load_falls_back_to_the_next_leader_when_a_load_fails() {
        let flights = SingleFlight::new();
        let attempts = Arc::new(AtomicUsize::new(0));
        let attempts_for_load = Arc::clone(&attempts);
        let result: Result<u8, String> = flights
            .get_or_load(
                "k",
                || Box::pin(async { None }),
                |_| Box::pin(async {}),
                move || {
                    let attempts = Arc::clone(&attempts_for_load);
                    Box::pin(async move {
                        if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                            Err("first load fails".to_string())
                        } else {
                            Ok(7)
                        }
                    })
                },
            )
            .await;
        // The error surfaces to the leader; a retry through the same helper
        // finds nothing cached, leads again and succeeds.
        assert_eq!(result, Err("first load fails".to_string()));
        let retry: Result<u8, String> = flights
            .get_or_load(
                "k",
                || Box::pin(async { None }),
                |_| Box::pin(async {}),
                || Box::pin(async { Ok(7) }),
            )
            .await;
        assert_eq!(retry, Ok(7));
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
    }

    impl FlightGuard {
        /// Test helper: complete the flight without a scope boundary.
        fn drop_after_completion(self) {}
    }
}
