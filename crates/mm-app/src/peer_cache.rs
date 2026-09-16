//! The Go server's in-memory caches, which this process writes past.
//!
//! Go memoises sessions by token and users by id, and purges an entry only from its own write
//! paths or a cluster message. This server shares the tables but not those maps, so a session
//! revoked here — a logout, a revoke-all, a deactivation — kept authenticating against Go until
//! the entry aged out, and a password changed here left the old one working there (formerly D-350
//! and D-237).
//!
//! The cluster bus that would carry the purge is enterprise code, and Go's local socket registers
//! no cache route. What is left is Go's own authenticated REST API, and [`PeerCache`] is the seam:
//! `mm-api` installs an implementation (`mm_api::go_cache`), and the app functions that port Go's
//! cache clears await it, so Go has forgotten before the response that changed things is written.
//!
//! **Each method names the Go function whose effect it must reproduce, and no more.** An earlier
//! version purged *every* Go cache on each call, and Go's status cache is not a cache in that
//! sense: `get_statuses` answers from it alone, so every logout served here blanked the presence
//! list of every Go-connected client. Measured by `parity::websocket_actions`.
//!
//! Absent — the default, and what every test that builds an `App` gets — nothing is sent.

use std::future::Future;
use std::pin::Pin;

/// A boxed future, because the trait is held as `dyn`.
pub type PeerFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

/// Something that makes the peer Go server forget what it cached.
///
/// Infallible by signature: a failure is the implementation's to log, and never fails the write
/// that triggered it — the row has already changed, and Go's entry ages out regardless.
pub trait PeerCache: Send + Sync + std::fmt::Debug {
    /// Go's `PlatformService.ClearUserSessionCache(userID)`: that user's cached sessions and the
    /// session copies on their web connections.
    fn clear_user_sessions<'a>(&'a self, user_id: &'a str) -> PeerFuture<'a>;

    /// Go's `ClearAllUsersSessionCache`, for the revoke-every-session route.
    fn clear_all_sessions(&self) -> PeerFuture<'_>;

    /// The profile half of Go's `InvalidateCacheForUser(userID)` — the cached user a login's
    /// password check reads.
    fn invalidate_user<'a>(&'a self, user_id: &'a str) -> PeerFuture<'a>;
}
