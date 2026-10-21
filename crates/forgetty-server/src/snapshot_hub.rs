//! Shared snapshot builds for catch-up (AD-023).
//!
//! A snapshot costs about 100 to 200 ms of CPU for an 8 MiB ring. Many slow
//! streams on one pane must not each pay it. The hub gives them one build:
//!
//! - a build in progress for a pane is joined, not repeated
//! - a result younger than [`REUSE_FOR`] is handed out again
//! - at most [`MAX_BUILDS`] builds run at once, server wide, and at most one
//!   per node (a device, by its iroh node id): one node asking for many panes
//!   builds them one after the other
//! - the server wide permits go to the waiting nodes in turn (round robin), so
//!   one node cannot keep another from a build
//! - a build nobody waits for any more (every stream that asked was replaced or
//!   closed) is dropped before it starts
//!
//! Builds run on their own task, so a waiter that goes away never leaves the
//! others without an answer.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use forgetty_core::PaneId;
use forgetty_session::PaneSnapshot;
use tokio::sync::{oneshot, watch};

/// A snapshot younger than this is reused.
pub const REUSE_FOR: Duration = Duration::from_millis(250);
/// Concurrent builds, server wide.
pub const MAX_BUILDS: usize = 2;

/// Who asks: the 32 bytes of a node id. Builds are limited and shared out per
/// node. The local socket and the tests use [`LOCAL`].
pub type NodeKey = [u8; 32];
/// The key for a request that has no remote node.
pub const LOCAL: NodeKey = [0; 32];

/// Hands out build permits: `max` at once, one per node, and to the waiting
/// nodes in turn.
struct Scheduler {
    max: usize,
    state: Mutex<SchedState>,
}

#[derive(Default)]
struct SchedState {
    running: usize,
    /// Nodes with a build running.
    busy: HashSet<NodeKey>,
    /// Waiting requests of each node, oldest first.
    queues: HashMap<NodeKey, VecDeque<oneshot::Sender<Permit>>>,
    /// Nodes with a waiting request, in the order they get their turn.
    turn: VecDeque<NodeKey>,
}

/// A build permit. Gives itself back on drop.
struct Permit {
    sched: Option<Arc<Scheduler>>,
    node: NodeKey,
}

impl Drop for Permit {
    fn drop(&mut self) {
        if let Some(sched) = self.sched.take() {
            let mut st = sched.lock();
            st.running -= 1;
            st.busy.remove(&self.node);
            sched.dispatch(&mut st);
        }
    }
}

impl Scheduler {
    fn lock(&self) -> MutexGuard<'_, SchedState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Wait for a permit for `node`.
    async fn acquire(self: &Arc<Self>, node: NodeKey) -> Option<Permit> {
        let (tx, rx) = oneshot::channel();
        {
            let mut st = self.lock();
            let q = st.queues.entry(node).or_default();
            let was_empty = q.is_empty();
            q.push_back(tx);
            if was_empty {
                st.turn.push_back(node);
            }
            self.dispatch(&mut st);
        }
        rx.await.ok()
    }

    /// Give free permits to the nodes in turn. A node that already has a build
    /// running is passed over.
    fn dispatch(self: &Arc<Self>, st: &mut SchedState) {
        let mut passes = st.turn.len();
        while st.running < self.max && passes > 0 {
            passes -= 1;
            let Some(node) = st.turn.pop_front() else { break };
            if st.busy.contains(&node) {
                st.turn.push_back(node);
                continue;
            }
            let Some(q) = st.queues.get_mut(&node) else { continue };
            // A request that was dropped while it waited has no receiver.
            let mut granted = false;
            while let Some(tx) = q.pop_front() {
                if tx.is_closed() {
                    continue;
                }
                let permit = Permit { sched: Some(self.clone()), node };
                match tx.send(permit) {
                    Ok(()) => {
                        granted = true;
                        break;
                    }
                    Err(mut p) => {
                        // Gone between the check and the send: not ours.
                        p.sched = None;
                    }
                }
            }
            if q.is_empty() {
                st.queues.remove(&node);
            } else {
                st.turn.push_back(node);
            }
            if granted {
                st.running += 1;
                st.busy.insert(node);
                passes = st.turn.len();
            }
        }
    }
}

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

type BuildFn = Box<dyn FnOnce() -> Option<PaneSnapshot> + Send>;

struct Build {
    tx: watch::Sender<Outcome>,
    /// The work. Whoever gets a permit first takes it.
    work: Mutex<Option<BuildFn>>,
    /// Nodes that have a request in the scheduler queue for this build.
    claimed: Mutex<HashSet<NodeKey>>,
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
    sched: Arc<Scheduler>,
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
            sched: Arc::new(Scheduler {
                max: max_builds,
                state: Mutex::new(SchedState::default()),
            }),
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

    /// [`Self::get_if`] for a request that has no remote node.
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
        self.get_for(LOCAL, pane, usable, build).await
    }

    /// Like [`Self::get`], but a finished result is handed out again only
    /// while `usable` accepts it (it may be young and still too far behind
    /// the log to be of use).
    pub async fn get_for<F, U>(
        self: &Arc<Self>,
        node: NodeKey,
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
                        // A build that waits for a permit also asks for one
                        // in the name of every node that joins it, so a node
                        // that has to wait behind another's queue can start
                        // it on its own turn.
                        if lock_set(&b.claimed).insert(node) {
                            self.spawn_claim(node, pane, b.clone());
                        }
                        (b.clone(), b.tx.subscribe())
                    }
                    _ => {
                        let f = build.take()?;
                        let (tx, rx) = watch::channel(Outcome::Pending);
                        let b = Arc::new(Build {
                            tx,
                            work: Mutex::new(Some(Box::new(f))),
                            claimed: Mutex::new(HashSet::from([node])),
                            waiters: AtomicUsize::new(1),
                        });
                        map.insert(pane, Slot::Building(b.clone()));
                        self.spawn_claim(node, pane, b.clone());
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

    /// Ask the scheduler for a permit in the name of `node`, and build with it
    /// if nobody has started this build yet.
    fn spawn_claim(self: &Arc<Self>, node: NodeKey, pane: PaneId, b: Arc<Build>) {
        let hub = self.clone();
        tokio::spawn(async move {
            let Some(permit) = hub.sched.acquire(node).await else { return };
            // Under the map lock, where joins are counted: the build is gone
            // (another node started it), or nobody waits for it any more.
            let f = {
                let mut map = hub.lock();
                let mut work = b.work.lock().unwrap_or_else(|e| e.into_inner());
                if work.is_none() {
                    return; // started by a claim of another node
                }
                if b.waiters.load(Ordering::SeqCst) == 0 {
                    work.take();
                    if matches!(map.get(&pane), Some(Slot::Building(x)) if Arc::ptr_eq(x, &b)) {
                        map.remove(&pane);
                    }
                    hub.stats.cancelled.fetch_add(1, Ordering::Relaxed);
                    let _ = b.tx.send(Outcome::Cancelled);
                    return;
                }
                work.take()
            };
            let Some(f) = f else { return };
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

fn lock_set(m: &Mutex<HashSet<NodeKey>>) -> MutexGuard<'_, HashSet<NodeKey>> {
    m.lock().unwrap_or_else(|e| e.into_inner())
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

    fn node(n: u8) -> NodeKey {
        [n; 32]
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn one_node_builds_one_snapshot_at_a_time_and_cannot_hold_another_back() {
        let hub = Arc::new(SnapshotHub::new(2, Duration::from_millis(10)));
        let (now, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        // The hostile node asks for 10 panes at once.
        let mut hostile = Vec::new();
        for _ in 0..10 {
            let (hub, now, peak) = (hub.clone(), now.clone(), peak.clone());
            hostile.push(tokio::spawn(async move {
                hub.get_for(
                    node(1),
                    pane(),
                    |_| true,
                    move || {
                        let n = now.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(n, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(100));
                        now.fetch_sub(1, Ordering::SeqCst);
                        Some(snap(0))
                    },
                )
                .await
            }));
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
        // An honest node asks while the hostile node's queue is long: it is
        // served in its turn, not after the 10 builds.
        let t0 = Instant::now();
        let started = Arc::new(Mutex::new(None::<Duration>));
        let honest = {
            let (hub, started) = (hub.clone(), started.clone());
            tokio::spawn(async move {
                hub.get_for(
                    node(2),
                    pane(),
                    |_| true,
                    move || {
                        *started.lock().unwrap() = Some(t0.elapsed());
                        std::thread::sleep(Duration::from_millis(20));
                        Some(snap(1))
                    },
                )
                .await
            })
        };
        honest.await.unwrap().unwrap();
        let waited = started.lock().unwrap().unwrap();
        for j in hostile {
            j.await.unwrap().unwrap();
        }
        assert_eq!(peak.load(Ordering::SeqCst), 1, "a node ran more than one build at once");
        assert!(waited < Duration::from_millis(300), "the honest node waited {waited:?}");
        assert!(hub.stats.max_running.load(Ordering::SeqCst) <= 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn permits_go_round_robin_to_the_waiting_nodes() {
        let hub = Arc::new(SnapshotHub::new(1, Duration::from_millis(10)));
        let order = Arc::new(Mutex::new(Vec::<u8>::new()));
        // One permit, busy for a while. Node 1 queues three, node 2 two.
        let busy = {
            let hub = hub.clone();
            tokio::spawn(async move {
                hub.get_for(
                    node(9),
                    pane(),
                    |_| true,
                    || {
                        std::thread::sleep(Duration::from_millis(100));
                        Some(snap(0))
                    },
                )
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        let mut joins = Vec::new();
        for n in [1u8, 1, 1, 2, 2] {
            let (hub, order) = (hub.clone(), order.clone());
            joins.push(tokio::spawn(async move {
                hub.get_for(
                    node(n),
                    pane(),
                    |_| true,
                    move || {
                        order.lock().unwrap().push(n);
                        std::thread::sleep(Duration::from_millis(10));
                        Some(snap(0))
                    },
                )
                .await
            }));
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        busy.await.unwrap().unwrap();
        for j in joins {
            j.await.unwrap().unwrap();
        }
        // Node 1 asked first and more often, but node 2 is not behind all of it.
        assert_eq!(*order.lock().unwrap(), vec![1, 2, 1, 2, 1]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_node_that_joins_a_queued_build_can_start_it_on_its_own_turn() {
        let hub = Arc::new(SnapshotHub::new(1, Duration::from_millis(10)));
        let p = pane();
        let busy = {
            let hub = hub.clone();
            tokio::spawn(async move {
                hub.get_for(
                    node(9),
                    pane(),
                    |_| true,
                    || {
                        std::thread::sleep(Duration::from_millis(100));
                        Some(snap(0))
                    },
                )
                .await
            })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        // Node 1 queues three builds, the last one for `p`. Node 2 then asks
        // for `p`: it joins that build and does not wait behind the other two.
        let mut joins = Vec::new();
        for _ in 0..2 {
            let hub = hub.clone();
            joins.push(tokio::spawn(async move {
                hub.get_for(
                    node(1),
                    pane(),
                    |_| true,
                    || {
                        std::thread::sleep(Duration::from_millis(100));
                        Some(snap(0))
                    },
                )
                .await
            }));
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
        let first = {
            let hub = hub.clone();
            tokio::spawn(async move { hub.get_for(node(1), p, |_| true, || Some(snap(5))).await })
        };
        tokio::time::sleep(Duration::from_millis(5)).await;
        let t0 = Instant::now();
        let joined = hub.get_for(node(2), p, |_| true, || Some(snap(6))).await.unwrap();
        let took = t0.elapsed();
        assert_eq!(joined.offset, 5, "node 2 joined node 1's build");
        assert!(took < Duration::from_millis(300), "node 2 waited {took:?}");
        first.await.unwrap().unwrap();
        busy.await.unwrap().unwrap();
        for j in joins {
            j.await.unwrap().unwrap();
        }
    }
}
