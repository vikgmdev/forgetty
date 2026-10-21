//! Shared snapshot builds for catch-up (AD-023).
//!
//! A snapshot costs about 100 to 200 ms of CPU for an 8 MiB ring. Many slow
//! streams on one pane must not each pay it. The hub gives them one build:
//!
//! - a build in progress for a pane is joined, not repeated
//! - a result younger than [`REUSE_FOR`] is handed out again
//! - at most [`MAX_BUILDS`] builds run at once, server wide
//! - a build nobody waits for any more (every stream that asked was replaced or
//!   closed) is dropped before it starts
//!
//! Builds run on their own task, so a waiter that goes away never leaves the
//! others without an answer.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use forgetty_core::PaneId;
use forgetty_session::PaneSnapshot;
use tokio::sync::{watch, Semaphore};

/// A snapshot younger than this is reused.
pub const REUSE_FOR: Duration = Duration::from_millis(250);
/// Concurrent builds, server wide.
pub const MAX_BUILDS: usize = 2;

/// Counters, for the harness and the logs.
#[derive(Default)]
pub struct HubStats {
    /// Builds that ran.
    pub builds: AtomicU64,
    /// Requests that joined a build in progress.
    pub joins: AtomicU64,
    /// Requests answered from a result younger than [`REUSE_FOR`].
    pub reuses: AtomicU64,
    /// Builds dropped because nobody waited for them.
    pub cancelled: AtomicU64,
    /// Builds running now.
    pub running: AtomicUsize,
    /// Most builds ever running at once.
    pub max_running: AtomicUsize,
    /// Microseconds the last build took.
    pub last_us: AtomicU64,
    /// Bytes of the last snapshot.
    pub last_bytes: AtomicU64,
}

#[derive(Clone)]
enum Outcome {
    Pending,
    Built(Option<Arc<PaneSnapshot>>),
    Cancelled,
}

struct Build {
    tx: watch::Sender<Outcome>,
    /// Requests waiting for this build. Changed under the map lock when it
    /// goes up, so a cancel cannot race a join.
    waiters: AtomicUsize,
}

enum Slot {
    Building(Arc<Build>),
    Ready(Instant, Arc<PaneSnapshot>),
}

/// Drops its waiter count with the request.
struct Waiter(Arc<Build>);

impl Drop for Waiter {
    fn drop(&mut self) {
        self.0.waiters.fetch_sub(1, Ordering::SeqCst);
    }
}

/// See the module docs.
pub struct SnapshotHub {
    map: Mutex<HashMap<PaneId, Slot>>,
    permits: Arc<Semaphore>,
    reuse_for: Duration,
    /// Counters.
    pub stats: HubStats,
}

impl Default for SnapshotHub {
    fn default() -> Self {
        Self::new(MAX_BUILDS, REUSE_FOR)
    }
}

impl SnapshotHub {
    /// A hub with `max_builds` concurrent builds and the given reuse window.
    pub fn new(max_builds: usize, reuse_for: Duration) -> Self {
        SnapshotHub {
            map: Mutex::new(HashMap::new()),
            permits: Arc::new(Semaphore::new(max_builds)),
            reuse_for,
            stats: HubStats::default(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<PaneId, Slot>> {
        self.map.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Forget a pane (it closed).
    pub fn forget(&self, pane: PaneId) {
        self.lock().remove(&pane);
    }

    /// A snapshot of `pane`: a fresh result, a build in progress, or a new
    /// build with `build` (a blocking function). `None` when the pane is gone.
    ///
    /// If the future is dropped, the request leaves. A build with no request
    /// left is not started.
    pub async fn get<F>(self: &Arc<Self>, pane: PaneId, build: F) -> Option<Arc<PaneSnapshot>>
    where
        F: FnOnce() -> Option<PaneSnapshot> + Send + 'static,
    {
        self.get_if(pane, |_| true, build).await
    }

    /// Like [`Self::get`], but a finished result is handed out again only
    /// while `usable` accepts it (it may be young and still too far behind
    /// the log to be of use).
    pub async fn get_if<F, U>(
        self: &Arc<Self>,
        pane: PaneId,
        usable: U,
        build: F,
    ) -> Option<Arc<PaneSnapshot>>
    where
        F: FnOnce() -> Option<PaneSnapshot> + Send + 'static,
        U: Fn(&PaneSnapshot) -> bool,
    {
        let mut build = Some(build);
        loop {
            let (b, mut rx) = {
                let mut map = self.lock();
                match map.get(&pane) {
                    Some(Slot::Ready(at, snap))
                        if at.elapsed() < self.reuse_for && usable(snap) =>
                    {
                        self.stats.reuses.fetch_add(1, Ordering::Relaxed);
                        return Some(snap.clone());
                    }
                    Some(Slot::Building(b)) => {
                        self.stats.joins.fetch_add(1, Ordering::Relaxed);
                        b.waiters.fetch_add(1, Ordering::SeqCst);
                        (b.clone(), b.tx.subscribe())
                    }
                    _ => {
                        let f = build.take()?;
                        let (tx, rx) = watch::channel(Outcome::Pending);
                        let b = Arc::new(Build { tx, waiters: AtomicUsize::new(1) });
                        map.insert(pane, Slot::Building(b.clone()));
                        self.spawn_build(pane, b.clone(), f);
                        (b, rx)
                    }
                }
            };
            let _waiter = Waiter(b);
            let outcome = match rx.wait_for(|o| !matches!(o, Outcome::Pending)).await {
                Ok(o) => o.clone(),
                Err(_) => return None,
            };
            match outcome {
                Outcome::Built(snap) => return snap,
                // The build was dropped before it started: ask again. The
                // closure moved into that build, so a retry joins or fails.
                Outcome::Cancelled | Outcome::Pending => {
                    build.as_ref()?;
                }
            }
        }
    }

    fn spawn_build<F>(self: &Arc<Self>, pane: PaneId, b: Arc<Build>, f: F)
    where
        F: FnOnce() -> Option<PaneSnapshot> + Send + 'static,
    {
        let hub = self.clone();
        tokio::spawn(async move {
            let permit = hub.permits.clone().acquire_owned().await;
            // Nobody waits any more: do not start. Checked under the map lock,
            // where joins are counted.
            {
                let mut map = hub.lock();
                if b.waiters.load(Ordering::SeqCst) == 0 {
                    if matches!(map.get(&pane), Some(Slot::Building(x)) if Arc::ptr_eq(x, &b)) {
                        map.remove(&pane);
                    }
                    hub.stats.cancelled.fetch_add(1, Ordering::Relaxed);
                    let _ = b.tx.send(Outcome::Cancelled);
                    return;
                }
            }
            let now_running = hub.stats.running.fetch_add(1, Ordering::SeqCst) + 1;
            hub.stats.max_running.fetch_max(now_running, Ordering::SeqCst);
            let started = Instant::now();
            let built = tokio::task::spawn_blocking(f).await.ok().flatten().map(Arc::new);
            hub.stats.running.fetch_sub(1, Ordering::SeqCst);
            hub.stats.builds.fetch_add(1, Ordering::Relaxed);
            hub.stats.last_us.store(started.elapsed().as_micros() as u64, Ordering::Relaxed);
            if let Some(s) = &built {
                hub.stats.last_bytes.store(s.bytes.len() as u64, Ordering::Relaxed);
            }
            drop(permit);
            {
                let mut map = hub.lock();
                // Only if the slot is still this build. A `forget` (the pane
                // closed) or a newer build owns it otherwise, and this late
                // result must not bring the slot back.
                if matches!(map.get(&pane), Some(Slot::Building(x)) if Arc::ptr_eq(x, &b)) {
                    match &built {
                        Some(s) => {
                            map.insert(pane, Slot::Ready(Instant::now(), s.clone()));
                        }
                        None => {
                            map.remove(&pane);
                        }
                    }
                }
            }
            let _ = b.tx.send(Outcome::Built(built));
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forgetty_core::PaneId;
    use uuid::Uuid;

    fn snap(offset: u64) -> PaneSnapshot {
        PaneSnapshot { offset, rows: 24, cols: 80, bytes: vec![1, 2, 3] }
    }

    fn pane() -> PaneId {
        PaneId(Uuid::new_v4())
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn many_requests_for_one_pane_share_one_build() {
        let hub = Arc::new(SnapshotHub::default());
        let p = pane();
        let runs = Arc::new(AtomicUsize::new(0));
        let mut joins = Vec::new();
        for _ in 0..50 {
            let (hub, runs) = (hub.clone(), runs.clone());
            joins.push(tokio::spawn(async move {
                hub.get(p, move || {
                    runs.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_millis(100));
                    Some(snap(7))
                })
                .await
            }));
        }
        for j in joins {
            assert_eq!(j.await.unwrap().unwrap().offset, 7);
        }
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        assert_eq!(hub.stats.builds.load(Ordering::SeqCst), 1);
        assert!(
            hub.stats.joins.load(Ordering::SeqCst) + hub.stats.reuses.load(Ordering::SeqCst) >= 49
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_result_is_reused_while_young_and_rebuilt_after() {
        let hub = Arc::new(SnapshotHub::new(2, Duration::from_millis(100)));
        let p = pane();
        let runs = Arc::new(AtomicUsize::new(0));
        let get = |hub: Arc<SnapshotHub>, runs: Arc<AtomicUsize>| async move {
            hub.get(p, move || {
                runs.fetch_add(1, Ordering::SeqCst);
                Some(snap(1))
            })
            .await
        };
        get(hub.clone(), runs.clone()).await.unwrap();
        get(hub.clone(), runs.clone()).await.unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 1, "young result reused");
        tokio::time::sleep(Duration::from_millis(150)).await;
        get(hub.clone(), runs.clone()).await.unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 2, "old result rebuilt");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_build_that_ends_after_forget_leaves_no_slot() {
        let hub = Arc::new(SnapshotHub::default());
        let p = pane();
        let waiter = {
            let hub = hub.clone();
            tokio::spawn(async move {
                hub.get(p, || {
                    std::thread::sleep(Duration::from_millis(150));
                    Some(snap(3))
                })
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        hub.forget(p);
        // The waiter still gets its answer.
        assert_eq!(waiter.await.unwrap().unwrap().offset, 3);
        assert!(hub.lock().get(&p).is_none(), "the finished build re-inserted a slot");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn no_more_than_two_builds_run_at_once() {
        let hub = Arc::new(SnapshotHub::default());
        let mut joins = Vec::new();
        for _ in 0..12 {
            let hub = hub.clone();
            joins.push(tokio::spawn(async move {
                hub.get(pane(), || {
                    std::thread::sleep(Duration::from_millis(50));
                    Some(snap(0))
                })
                .await
            }));
        }
        for j in joins {
            j.await.unwrap().unwrap();
        }
        assert_eq!(hub.stats.builds.load(Ordering::SeqCst), 12);
        assert!(hub.stats.max_running.load(Ordering::SeqCst) <= MAX_BUILDS);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_build_nobody_waits_for_is_not_started() {
        let hub = Arc::new(SnapshotHub::new(1, Duration::from_millis(10)));
        // Occupy the only permit.
        let busy = {
            let hub = hub.clone();
            tokio::spawn(async move {
                hub.get(pane(), || {
                    std::thread::sleep(Duration::from_millis(200));
                    Some(snap(0))
                })
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(30)).await;
        let runs = Arc::new(AtomicUsize::new(0));
        let waiter = {
            let (hub, runs) = (hub.clone(), runs.clone());
            tokio::spawn(async move {
                hub.get(pane(), move || {
                    runs.fetch_add(1, Ordering::SeqCst);
                    Some(snap(0))
                })
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(30)).await;
        waiter.abort(); // the stream was replaced
        busy.await.unwrap().unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(runs.load(Ordering::SeqCst), 0, "the abandoned build ran");
        assert_eq!(hub.stats.cancelled.load(Ordering::SeqCst), 1);
    }
}
