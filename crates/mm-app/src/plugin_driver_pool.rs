//! The connection pool behind the plugin driver: `database/sql`'s `*sql.DB` as far as
//! `DriverImpl` uses it — `Conn(ctx)` to take a connection, `(*sql.Conn).Close` to give it back.
//!
//! One pool per data source, built on first use from `SqlSettings` as `SqlStore` builds Go's:
//!
//! - `MaxOpenConns` bounds the connections handed out at once (0 or less: no bound). A taker
//!   waits for one to come back until `QueryTimeout` runs out, and then fails with
//!   `context deadline exceeded`, as `db.Conn(ctx)` does; a `QueryTimeout` of 0 fails at once.
//! - `MaxIdleConns` bounds what is kept for reuse (0 or less: nothing; never more than
//!   `MaxOpenConns`). A connection lib/pq marked bad is never kept.
//! - `ConnMaxLifetimeMilliseconds` and `ConnMaxIdleTimeMilliseconds` retire a kept connection that
//!   is too old or has sat too long; Go's cleaner does it on a timer, this when the connection is
//!   next wanted — the same connections are never reused either way.
//!
//! A reused connection is Go's too: whatever the previous `Conn` left on the session — a `SET`,
//! a temporary table, a statement name — is still there.
//!
//! **Not shared with the server's own queries.** Go's plugins draw from the store's own pool, so
//! the store's traffic counts against the same `MaxOpenConns`; here the server's queries go
//! through sqlx's pool and this one serves the plugins alone. The settings are read when the
//! pool is built; Go reads them when the store is.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// `context.DeadlineExceeded`'s text: a `Conn` that waited out `QueryTimeout`.
pub const DEADLINE_EXCEEDED: &str = "context deadline exceeded";

/// Why a connection could not be had.
#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    #[error("context deadline exceeded")]
    Deadline,
    #[error(transparent)]
    Connect(#[from] gopq::Error),
}

/// The `SqlSettings` a pool is built from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PoolSettings {
    pub max_open: i64,
    pub max_idle: i64,
    pub max_lifetime_ms: i64,
    pub max_idle_time_ms: i64,
}

impl PoolSettings {
    /// `SetMaxIdleConns` after `SetMaxOpenConns`: none below 1, never above the open bound.
    fn idle_bound(&self) -> usize {
        let idle = usize::try_from(self.max_idle).unwrap_or(0);
        match usize::try_from(self.max_open) {
            Ok(open) if open > 0 => idle.min(open),
            _ => idle,
        }
    }

    fn lifetime(&self) -> Option<Duration> {
        u64::try_from(self.max_lifetime_ms)
            .ok()
            .filter(|ms| *ms > 0)
            .map(Duration::from_millis)
    }

    fn idle_time(&self) -> Option<Duration> {
        u64::try_from(self.max_idle_time_ms)
            .ok()
            .filter(|ms| *ms > 0)
            .map(Duration::from_millis)
    }
}

struct Idle {
    conn: gopq::Conn,
    created: Instant,
    returned: Instant,
}

/// One data source's pool.
pub struct Pool {
    dsn: String,
    settings: PoolSettings,
    permits: Option<Arc<Semaphore>>,
    idle: Mutex<Vec<Idle>>,
}

/// A connection taken from a [`Pool`], holding its place under `MaxOpenConns` until it is put
/// back or dropped.
pub struct Pooled {
    pub conn: gopq::Conn,
    created: Instant,
    pool: Arc<Pool>,
    permit: Option<OwnedSemaphorePermit>,
}

impl Pooled {
    /// `(*sql.Conn).Close`'s `putConn`: kept for reuse when it is sound, young enough and there is
    /// room; otherwise ended.
    pub async fn put_back(self) {
        let Pooled {
            conn,
            created,
            pool,
            permit: _permit,
        } = self;
        let expired = pool
            .settings
            .lifetime()
            .is_some_and(|life| created.elapsed() >= life);
        if !conn.is_bad() && !expired {
            let mut idle = pool.idle.lock().unwrap_or_else(PoisonError::into_inner);
            if idle.len() < pool.settings.idle_bound() {
                idle.push(Idle {
                    conn,
                    created,
                    returned: Instant::now(),
                });
                return;
            }
        }
        let _ = conn.close().await;
    }

    /// Give up its place under `MaxOpenConns` while a statement, transaction or rows handle
    /// still holds it. Go puts such a connection back in the pool, where it no longer counts as
    /// in use; it cannot be shared here, so it only stops counting.
    pub fn release_slot(&mut self) {
        self.permit = None;
    }

    /// End the connection: `database/sql` closing a connection the driver called bad.
    pub async fn discard(self) {
        let _ = self.conn.close().await;
    }
}

impl Pool {
    pub fn new(dsn: String, settings: PoolSettings) -> Arc<Self> {
        let permits = usize::try_from(settings.max_open)
            .ok()
            .filter(|n| *n > 0)
            .map(|n| Arc::new(Semaphore::new(n)));
        Arc::new(Self {
            dsn,
            settings,
            permits,
            idle: Mutex::new(Vec::new()),
        })
    }

    /// A kept connection that is still usable, if any; the retired ones are ended on the way.
    async fn reuse(&self) -> Option<(gopq::Conn, Instant)> {
        loop {
            let candidate = self
                .idle
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop()?;
            let too_old = self
                .settings
                .lifetime()
                .is_some_and(|life| candidate.created.elapsed() >= life);
            let too_idle = self
                .settings
                .idle_time()
                .is_some_and(|limit| candidate.returned.elapsed() >= limit);
            if too_old || too_idle || candidate.conn.is_bad() {
                let _ = candidate.conn.close().await;
                continue;
            }
            return Some((candidate.conn, candidate.created));
        }
    }

    /// `db.Conn(ctx)` with `QueryTimeout` as the deadline.
    pub async fn get(self: &Arc<Self>, timeout: Duration) -> Result<Pooled, PoolError> {
        if timeout.is_zero() {
            return Err(PoolError::Deadline);
        }
        let deadline = tokio::time::Instant::now() + timeout;
        let permit = match &self.permits {
            None => None,
            Some(permits) => {
                match tokio::time::timeout_at(deadline, Arc::clone(permits).acquire_owned()).await {
                    Ok(Ok(permit)) => Some(permit),
                    Ok(Err(_)) | Err(_) => return Err(PoolError::Deadline),
                }
            }
        };
        let (conn, created) = match self.reuse().await {
            Some(found) => found,
            None => match tokio::time::timeout_at(deadline, gopq::Conn::connect(&self.dsn)).await {
                Ok(conn) => (conn?, Instant::now()),
                Err(_) => return Err(PoolError::Deadline),
            },
        };
        Ok(Pooled {
            conn,
            created,
            pool: Arc::clone(self),
            permit,
        })
    }

    /// Connections kept for reuse right now.
    pub fn idle_count(&self) -> usize {
        self.idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

/// The pools by data source.
#[derive(Default)]
pub struct Pools(Mutex<HashMap<String, Arc<Pool>>>);

impl Pools {
    /// The pool for `dsn`, built with `settings` the first time.
    pub fn for_dsn(&self, dsn: &str, settings: PoolSettings) -> Arc<Pool> {
        let mut pools = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(
            pools
                .entry(dsn.to_owned())
                .or_insert_with(|| Pool::new(dsn.to_owned(), settings)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_idle_bound_follows_set_max_idle_conns() {
        let s = |max_open, max_idle| PoolSettings {
            max_open,
            max_idle,
            ..PoolSettings::default()
        };
        assert_eq!(s(300, 20).idle_bound(), 20);
        assert_eq!(s(4, 20).idle_bound(), 4);
        assert_eq!(s(0, 20).idle_bound(), 20);
        assert_eq!(s(10, 0).idle_bound(), 0);
        assert_eq!(s(10, -1).idle_bound(), 0);
    }

    #[test]
    fn a_non_positive_duration_is_no_limit() {
        let s = PoolSettings {
            max_lifetime_ms: 0,
            max_idle_time_ms: 1500,
            ..PoolSettings::default()
        };
        assert_eq!(s.lifetime(), None);
        assert_eq!(s.idle_time(), Some(Duration::from_millis(1500)));
    }

    #[tokio::test]
    async fn a_zero_timeout_fails_at_once_and_a_full_pool_waits_it_out() {
        // No server is contacted: the first fails before connecting, the second waits for a
        // permit that never comes.
        let pool = Pool::new(
            "postgres://nobody@127.0.0.1:1/none?sslmode=disable".into(),
            PoolSettings {
                max_open: 1,
                ..PoolSettings::default()
            },
        );
        assert!(matches!(
            pool.get(Duration::ZERO).await,
            Err(PoolError::Deadline)
        ));
        let held = Arc::clone(pool.permits.as_ref().unwrap())
            .acquire_owned()
            .await
            .unwrap();
        let started = Instant::now();
        assert!(matches!(
            pool.get(Duration::from_millis(50)).await,
            Err(PoolError::Deadline)
        ));
        assert!(started.elapsed() >= Duration::from_millis(50));
        drop(held);
        assert_eq!(pool.idle_count(), 0);
    }
}
