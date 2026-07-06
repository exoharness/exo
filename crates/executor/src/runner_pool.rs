use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use exoharness::Result;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::task::AbortHandle;

/// Bounded pool of runner processes for one harness module.
///
/// Checkout is exclusive by ownership: `acquire` hands the runner itself to
/// the caller, so no per-runner lock exists. `checkin` returns it warm;
/// dropping a [`PooledRunner`] without checking in discards the runner
/// instead — the safe default for a runner whose turn failed. Idle runners
/// expire after `idle_ttl`, so a quiescent pool drains to zero; dropping the
/// pool reaps them immediately (the expiry timers hold only a `Weak`).
pub(crate) struct RunnerPool<R> {
    permits: Arc<Semaphore>,
    idle: Mutex<Vec<Idle<R>>>,
    idle_ttl: Duration,
    next_token: AtomicU64,
}

struct Idle<R> {
    runner: R,
    token: u64,
    expiry: AbortHandle,
}

impl<R: Send + 'static> RunnerPool<R> {
    pub(crate) fn new(max_size: usize, idle_ttl: Duration) -> Arc<Self> {
        Arc::new(Self {
            permits: Arc::new(Semaphore::new(max_size.max(1))),
            idle: Mutex::new(Vec::new()),
            idle_ttl,
            next_token: AtomicU64::new(0),
        })
    }

    /// Waits for capacity (FIFO), then reuses the most recently used idle
    /// runner or spawns a fresh one. Spawn failures release the capacity and
    /// are never cached.
    pub(crate) async fn acquire(
        self: &Arc<Self>,
        spawn: impl FnOnce() -> Result<R>,
    ) -> Result<PooledRunner<R>> {
        let permit = Arc::clone(&self.permits)
            .acquire_owned()
            .await
            .expect("runner pool semaphore is never closed");
        // Bind the pop to a statement so the idle guard drops before the
        // (blocking) spawn in the miss path.
        let reused = self.idle.lock().expect("runner pool idle lock").pop();
        let runner = match reused {
            Some(idle) => {
                idle.expiry.abort();
                idle.runner
            }
            None => spawn()?,
        };
        Ok(PooledRunner {
            runner: Some(runner),
            pool: Arc::clone(self),
            _permit: permit,
        })
    }

    fn checkin(self: &Arc<Self>, runner: R) {
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        let idle_ttl = self.idle_ttl;
        let weak = Arc::downgrade(self);

        // Hold the idle lock across spawn + push so the timer (Weak-held, so
        // it never keeps the pool alive) can't run its removal before the
        // entry exists. Checkout aborts the timer; a firing timer that lost
        // the race to a checkout finds its token gone and no-ops.
        let mut idle = self.idle.lock().expect("runner pool idle lock");
        let expiry = tokio::spawn(async move {
            tokio::time::sleep(idle_ttl).await;
            let Some(pool) = weak.upgrade() else { return };
            let mut idle = pool.idle.lock().expect("runner pool idle lock");
            if let Some(index) = idle.iter().position(|entry| entry.token == token) {
                idle.remove(index);
            }
        })
        .abort_handle();
        idle.push(Idle {
            runner,
            token,
            expiry,
        });
    }
}

pub(crate) struct PooledRunner<R: Send + 'static> {
    runner: Option<R>,
    pool: Arc<RunnerPool<R>>,
    _permit: OwnedSemaphorePermit,
}

impl<R: Send + 'static> PooledRunner<R> {
    /// Return the runner to the pool warm. Dropping without calling this
    /// discards the runner.
    pub(crate) fn checkin(mut self) {
        let runner = self.runner.take().expect("runner present until checkin");
        self.pool.checkin(runner);
    }
}

impl<R: Send + 'static> Deref for PooledRunner<R> {
    type Target = R;

    fn deref(&self) -> &R {
        self.runner.as_ref().expect("runner present until checkin")
    }
}

impl<R: Send + 'static> DerefMut for PooledRunner<R> {
    fn deref_mut(&mut self) -> &mut R {
        self.runner.as_mut().expect("runner present until checkin")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    const TTL: Duration = Duration::from_secs(300);

    fn counting_spawn(spawns: &Arc<AtomicUsize>) -> impl FnOnce() -> Result<usize> + use<> {
        let spawns = Arc::clone(spawns);
        move || Ok(spawns.fetch_add(1, Ordering::SeqCst) + 1)
    }

    #[tokio::test]
    async fn reuses_idle_runners_lifo() {
        let pool = RunnerPool::new(4, TTL);
        let spawns = Arc::new(AtomicUsize::new(0));

        let first = pool.acquire(counting_spawn(&spawns)).await.unwrap();
        let second = pool.acquire(counting_spawn(&spawns)).await.unwrap();
        first.checkin();
        second.checkin();

        let reused = pool.acquire(counting_spawn(&spawns)).await.unwrap();
        assert_eq!(*reused, 2, "most recently checked in comes back first");
        let older = pool.acquire(counting_spawn(&spawns)).await.unwrap();
        assert_eq!(*older, 1);
        assert_eq!(
            spawns.load(Ordering::SeqCst),
            2,
            "no spawn while idle runners exist"
        );
    }

    #[tokio::test]
    async fn drop_discards_and_frees_capacity() {
        let pool = RunnerPool::new(1, TTL);
        let spawns = Arc::new(AtomicUsize::new(0));

        drop(pool.acquire(counting_spawn(&spawns)).await.unwrap());

        let next = pool.acquire(counting_spawn(&spawns)).await.unwrap();
        assert_eq!(*next, 2, "discarded runner is not reused");
        assert_eq!(spawns.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn saturated_acquire_waits_for_checkin() {
        let pool = RunnerPool::new(1, TTL);
        let spawns = Arc::new(AtomicUsize::new(0));

        let held = pool.acquire(counting_spawn(&spawns)).await.unwrap();
        let waiter = tokio::spawn({
            let pool = Arc::clone(&pool);
            let spawn = counting_spawn(&spawns);
            async move { *pool.acquire(spawn).await.unwrap() }
        });

        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(!waiter.is_finished(), "acquire must block at capacity");

        held.checkin();
        assert_eq!(
            waiter.await.unwrap(),
            1,
            "waiter reuses the checked-in runner"
        );
        assert_eq!(spawns.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_pool_is_not_pinned_by_idle_timers() {
        let pool = RunnerPool::new(1, TTL);
        let spawns = Arc::new(AtomicUsize::new(0));
        pool.acquire(counting_spawn(&spawns))
            .await
            .unwrap()
            .checkin();

        let weak = Arc::downgrade(&pool);
        drop(pool);
        assert!(
            weak.upgrade().is_none(),
            "idle expiry timer must hold a Weak, not keep the pool (and its runners) alive",
        );
    }

    #[tokio::test(start_paused = true)]
    async fn idle_runner_expires_after_ttl() {
        let pool = RunnerPool::new(1, TTL);
        let spawns = Arc::new(AtomicUsize::new(0));

        pool.acquire(counting_spawn(&spawns))
            .await
            .unwrap()
            .checkin();
        tokio::time::sleep(TTL + Duration::from_secs(1)).await;

        let next = pool.acquire(counting_spawn(&spawns)).await.unwrap();
        assert_eq!(*next, 2, "expired runner is gone; a fresh one is spawned");
    }

    #[tokio::test(start_paused = true)]
    async fn checkout_invalidates_pending_expiry() {
        let pool = RunnerPool::new(1, TTL);
        let spawns = Arc::new(AtomicUsize::new(0));

        pool.acquire(counting_spawn(&spawns))
            .await
            .unwrap()
            .checkin();
        tokio::time::sleep(TTL / 2).await;

        // Cycle the runner: the first check-in's timer must now be a no-op.
        pool.acquire(counting_spawn(&spawns))
            .await
            .unwrap()
            .checkin();
        tokio::time::sleep(TTL / 2 + Duration::from_secs(1)).await;

        let survivor = pool.acquire(counting_spawn(&spawns)).await.unwrap();
        assert_eq!(
            *survivor, 1,
            "re-checked-in runner survives the stale timer"
        );
        survivor.checkin();

        tokio::time::sleep(TTL + Duration::from_secs(1)).await;
        let fresh = pool.acquire(counting_spawn(&spawns)).await.unwrap();
        assert_eq!(
            *fresh, 2,
            "runner expires a full TTL after its last check-in"
        );
    }
}
