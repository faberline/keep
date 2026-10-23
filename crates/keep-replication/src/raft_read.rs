//! Read consistency for replicated shard groups (`x-read-consistency`).
//!
//! A replica answers reads from its local engine; what differs per request is
//! the proof it must hold first that its state is recent enough:
//!
//! - **leader** (default): a [`ReadFence`] commits a [`RaftCmd::Barrier`]
//!   that starts *after* the read arrived and waits until this node has
//!   applied it. Everything committed before the read began is then visible,
//!   on a leader or a follower (a follower's barrier is forwarded to the
//!   leader), and a deposed leader cannot answer because its barrier cannot
//!   commit. Concurrent readers share one barrier.
//! - **bounded(ms)**: the node answers when its staleness — time since the
//!   last moment it provably held every committed entry — is within the bound.
//!   Followers learn that moment by polling the leader's `/raftz` commit index
//!   ([`track_freshness`]); any node also learns it from a completed fence.
//! - **any**: answer locally, no proof.
//!
//! [`RaftCmd::Barrier`]: crate::raft::RaftCmd::Barrier

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use raft_runtime::{Index, NodeId, RaftHost, RaftStatus};

/// How often a follower samples the leader's commit index.
pub const FRESHNESS_POLL: Duration = Duration::from_millis(100);

/// Per-request budget for a follower's `/raftz` sample of the leader.
const STATUS_TIMEOUT: Duration = Duration::from_millis(300);

/// Samples awaiting local apply; older ones are dropped first.
const MAX_PENDING_SAMPLES: usize = 64;

/// Coalesced read barrier plus the freshness it proves, for one shard group.
#[derive(Default)]
pub struct ReadFence {
    /// Barriers started so far (1-based sequence numbers).
    started: AtomicU64,
    /// Sequence number of the last barrier that completed.
    done: tokio::sync::Mutex<u64>,
    /// Latest instant at which this node provably held every committed entry.
    fresh_as_of: Mutex<Option<Instant>>,
}

impl ReadFence {
    /// Wait until a barrier that started after this call has been applied
    /// locally. `barrier` proposes one and resolves with its index once
    /// applied; it runs at most once per call, and not at all when another
    /// reader's barrier already covers this one.
    pub async fn run<F, Fut>(&self, barrier: F) -> Result<()>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Index>>,
    {
        let arrived = self.started.load(Ordering::SeqCst);
        let mut done = self.done.lock().await;
        if *done > arrived {
            // A barrier numbered above `arrived` began after this read did.
            return Ok(());
        }
        let started_at = Instant::now();
        let seq = self.started.fetch_add(1, Ordering::SeqCst) + 1;
        barrier().await?;
        *done = seq;
        self.note_fresh(started_at);
        Ok(())
    }

    /// Record that this node held every entry committed as of `at`.
    pub fn note_fresh(&self, at: Instant) {
        let mut fresh = self.fresh_as_of.lock().unwrap();
        if fresh.is_none_or(|prev| prev < at) {
            *fresh = Some(at);
        }
    }

    /// How far behind the committed state this node may be, or `None` before
    /// any proof of freshness.
    pub fn staleness(&self) -> Option<Duration> {
        self.fresh_as_of
            .lock()
            .unwrap()
            .map(|at| Instant::now().saturating_duration_since(at))
    }
}

/// Keep `fence`'s freshness current on a follower: sample the leader's commit
/// index every [`FRESHNESS_POLL`]; once this node has applied up to a sample,
/// it was fresh as of the moment that sample was requested.
pub async fn track_freshness(
    host: Arc<RaftHost>,
    peers: HashMap<NodeId, String>,
    fence: Arc<ReadFence>,
) {
    let http = reqwest::Client::new();
    let mut applied = host.applied_watch();
    let mut pending: VecDeque<(Instant, Index)> = VecDeque::new();
    let mut tick = tokio::time::interval(FRESHNESS_POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if host.is_leader().await {
                    // A leader proves freshness only through its own fences.
                    pending.clear();
                    continue;
                }
                let Some(url) = host.leader().await.and_then(|id| peers.get(&id)) else {
                    continue;
                };
                let asked_at = Instant::now();
                if let Some(status) = leader_status(&http, url).await {
                    if status.is_leader {
                        if pending.len() == MAX_PENDING_SAMPLES {
                            pending.pop_front();
                        }
                        pending.push_back((asked_at, status.commit_index));
                    }
                }
            }
            changed = applied.changed() => {
                if changed.is_err() {
                    return;
                }
            }
        }
        let head = *applied.borrow();
        while let Some(&(at, commit)) = pending.front() {
            if commit > head {
                break;
            }
            fence.note_fresh(at);
            pending.pop_front();
        }
    }
}

async fn leader_status(http: &reqwest::Client, base: &str) -> Option<RaftStatus> {
    let resp = http
        .get(format!("{base}/raftz"))
        .timeout(STATUS_TIMEOUT)
        .send()
        .await
        .ok()?;
    resp.json().await.ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[tokio::test]
    async fn concurrent_readers_share_one_barrier() {
        let fence = Arc::new(ReadFence::default());
        let proposed = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(tokio::sync::Notify::new());

        // First reader holds the fence while its barrier is in flight.
        let first = {
            let (fence, proposed, gate) = (fence.clone(), proposed.clone(), gate.clone());
            tokio::spawn(async move {
                fence
                    .run(|| async move {
                        proposed.fetch_add(1, Ordering::SeqCst);
                        gate.notified().await;
                        Ok(1)
                    })
                    .await
            })
        };
        while proposed.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        // Readers arriving now cannot reuse the in-flight barrier (it began
        // before them); they queue and share the next one.
        let waiters: Vec<_> = (0..8)
            .map(|_| {
                let (fence, proposed) = (fence.clone(), proposed.clone());
                tokio::spawn(async move {
                    fence
                        .run(|| async move {
                            proposed.fetch_add(1, Ordering::SeqCst);
                            Ok(2)
                        })
                        .await
                })
            })
            .collect();
        tokio::task::yield_now().await;
        gate.notify_one();
        first.await.unwrap().unwrap();
        for w in waiters {
            w.await.unwrap().unwrap();
        }
        assert_eq!(proposed.load(Ordering::SeqCst), 2);
        assert!(fence.staleness().is_some());
    }

    #[tokio::test]
    async fn failed_barrier_proves_nothing() {
        let fence = ReadFence::default();
        let err = fence
            .run(|| async { Err(anyhow::anyhow!("no leader")) })
            .await;
        assert!(err.is_err());
        assert!(fence.staleness().is_none());
        fence.run(|| async { Ok(7) }).await.unwrap();
        assert!(fence.staleness().unwrap() < Duration::from_secs(1));
    }

    #[test]
    fn freshness_only_moves_forward() {
        let fence = ReadFence::default();
        let now = Instant::now();
        fence.note_fresh(now);
        fence.note_fresh(now - Duration::from_secs(5));
        assert!(fence.staleness().unwrap() < Duration::from_secs(1));
    }
}
