//! Port of `channels/app/platform/web_hub.go` and the *send* half of `web_conn.go`.
//!
//! The hub is the registry of live websocket connections and the fan-out rule that decides which
//! of them a given event reaches. It is deliberately transport-agnostic: it holds an
//! [`mpsc::Sender`] per connection and never touches a socket. `mm-api` owns the upgrade, the read
//! pump and the write pump, exactly as Go splits `api4/websocket.go` from `app/platform`.
//!
//! # What is ported and what is not
//!
//! Ported, because it decides *who sees what*:
//!
//! - [`Hub::register`] / [`Hub::unregister`] and the two indexes Go's `hubConnectionIndex` keeps
//!   (by connection id, by user id);
//! - the `hello` frame on a fresh registration (`web_hub.go:604`), including that Go sends it only
//!   when `reuseCount == 0`;
//! - [`App::should_send_event`], a line-for-line port of `WebConn.ShouldSendEvent`
//!   (`web_conn.go:884`) — the addressing filters, the omit sets, the sanitized/sensitive split,
//!   and the channel/team membership tests;
//! - the slow-queue drop for `typing` / `status_change` / `multiple_channels_viewed`, which is a
//!   *visible* behaviour: those events are silently discarded once a connection's queue is half
//!   full;
//! - the broadcast-hook runner (`web_broadcast_hook.go`): [`Hub::run_broadcast_hooks`] and
//!   [`HookedWebSocketEvent`], run **per connection** inside the fan-out where Go runs them
//!   (`web_hub.go:731`). A hook that modifies the event makes a copy, and the copy leaves through
//!   the non-precomputed encoding — see [`OutgoingFrame`]. The hooks themselves live in
//!   [`crate::broadcast_hooks`]: three of Go's nine are registered, and an id the registry does
//!   not know is logged and skipped, which is the seam for the other six ([D-183]).
//! - **reconnect replay**: a disconnected connection is parked inactive with its queues
//!   ([`Hub::park`]), a client that comes back with its `connection_id` inherits them
//!   ([`Hub::check_conn`]), and the write pump replays from the [`DeadQueue`] or, when the
//!   client missed more than it holds, sends a new `hello` under a new id.
//!
//! Not ported, each for a stated reason:
//!
//! - **Cluster send.** `Publish` mirrors an event to other nodes through `clusterIFace`
//!   (`cluster.go:189`); there is one node, and the strangler's *other* process is the Go server,
//!   which has its own hub. See [D-182] — a client connected to this server does not see events
//!   raised by a route still served by Go.
//! - **`Reject`** (`web_conn.go:577`). A rejected event is skipped by Go's write pump. None of
//!   the three registered hooks rejects, so [`HookedWebSocketEvent`] has no `reject` and the pump
//!   does not check the flag; both arrive with the first hook that needs them ([D-183]).
//! - **The MFA arm of `IsAuthenticated`.** `MFARequired` is not ported, so a connection whose user
//!   owes MFA is treated as authenticated. See [D-184].
//!
//! # Where the sequence number is assigned
//!
//! Nowhere in this file. Go assigns `seq` inside `writePump` as it pops the queue
//! (`web_conn.go:583`), so the numbering is per connection and follows *delivery* order, not
//! broadcast order. `mm-api`'s write pump does the same. Assigning it here would be wrong the
//! moment two hubs' broadcasts interleave on one connection.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, RwLock, RwLockWriteGuard};

use mm_model::session::Session;
use mm_model::utils::{StringInterface, get_millis, new_id};
use mm_model::websocket_message::{
    WEBSOCKET_EVENT_HELLO, WEBSOCKET_EVENT_MULTIPLE_CHANNELS_VIEWED, WEBSOCKET_EVENT_NEW_USER,
    WEBSOCKET_EVENT_REACTION_ADDED, WEBSOCKET_EVENT_REACTION_REMOVED,
    WEBSOCKET_EVENT_STATUS_CHANGE, WEBSOCKET_EVENT_TYPING, WEBSOCKET_EVENT_USER_UPDATED,
    WebSocketEvent, WebSocketResponse,
};
use mm_store::ChannelStore;
use tokio::sync::mpsc;

use crate::App;
use crate::broadcast_hooks::BroadcastHookError;

/// Port of `platform.sendQueueSize` (web_conn.go:34).
pub const SEND_QUEUE_SIZE: usize = 256;

/// Port of `platform.sendSlowWarn` (web_conn.go:35) — 50% of the queue. Past this point the three
/// event types below are dropped rather than queued.
const SEND_SLOW_WARN: usize = (SEND_QUEUE_SIZE * 50) / 100;

/// Port of `platform.webConnMemberCacheTime` (web_conn.go:41) — 30 minutes, in milliseconds.
const WEB_CONN_MEMBER_CACHE_TIME: i64 = 1000 * 60 * 30;

/// Port of `platform.UnsetPresenceIndicator` (web_conn.go:55).
///
/// `<>` rather than `""` because a mobile client may legitimately set the active channel to the
/// empty string, and Go has to tell that apart from never having been told.
pub const UNSET_PRESENCE_INDICATOR: &str = "<>";

/// Port of `platform.deadQueueSize` (web_conn.go:42).
pub const DEAD_QUEUE_SIZE: usize = 128;

/// Port of `platform.inactiveConnReaperInterval` (web_hub.go:25) — five minutes, in
/// milliseconds. Both how often the reaper runs and how long an inactive connection's last
/// activity may be past before it is dropped.
const INACTIVE_CONN_REAPER_INTERVAL: i64 = 5 * 60 * 1000;

/// One event the write pump has written: the frame with its sequence set, and which of the two
/// encodings it left in — a replay must send the same bytes.
#[derive(Debug, Clone)]
pub struct DeadQueueEntry {
    pub event: WebSocketEvent,
    pub precomputed: bool,
}

/// Port of `WebConn.deadQueue` and `deadQueuePointer` (web_conn.go:109-117) and the five functions
/// over them (web_conn.go:665-772): a ring of the last [`DEAD_QUEUE_SIZE`] events written to a
/// connection, kept so a client that drops can be sent what it missed.
///
/// The functions are Go's line for line, **including their assumptions**: that sequence numbers
/// rise by one between neighbours, that the ring is filled from slot 0 without gaps, and that a
/// sequence decreasing between two slots marks the wrap. The write pump is the only writer and
/// keeps all three true.
///
/// **Provisional oracle.** The Go functions are unexported, so `reference/dump` cannot call them
/// and there is no generated corpus; the unit tests below are transcribed from the Go source, and
/// `parity::websocket_reconnect` is the evidence that the whole path agrees.
#[derive(Debug)]
pub struct DeadQueue {
    slots: Vec<Option<DeadQueueEntry>>,
    /// The next slot to write.
    pointer: usize,
}

impl Default for DeadQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl DeadQueue {
    pub fn new() -> Self {
        Self {
            slots: vec![None; DEAD_QUEUE_SIZE],
            pointer: 0,
        }
    }

    /// Port of `addToDeadQueue` (web_conn.go:665).
    pub fn add(&mut self, entry: DeadQueueEntry) {
        self.slots[self.pointer] = Some(entry);
        self.pointer = (self.pointer + 1) % DEAD_QUEUE_SIZE;
    }

    /// Port of `_hasMsgLoss` (web_conn.go:685): whether the newest entry is **not** the one just
    /// before `seq`. An empty queue has lost nothing.
    pub fn has_msg_loss(&self, seq: i64) -> bool {
        let index = if self.pointer == 0 {
            if self.slots[0].is_none() {
                return false;
            }
            DEAD_QUEUE_SIZE - 1
        } else {
            self.pointer - 1
        };
        // Go dereferences the slot unconditionally; it is never empty on a path the pump can
        // reach. An empty one is reported as loss, which sends the client a fresh `hello` rather
        // than silence.
        self.slots[index]
            .as_ref()
            .is_none_or(|entry| entry.event.get_sequence() != seq - 1)
    }

    /// Port of `_isInDeadQueue` (web_conn.go:710): the slot holding `seq`, scanning from slot 0
    /// and **stopping at the first empty slot**.
    pub fn position_of(&self, seq: i64) -> Option<usize> {
        for slot in &self.slots {
            match slot {
                None => return None,
                Some(entry) if entry.event.get_sequence() == seq => {
                    return self.slots.iter().position(|s| std::ptr::eq(s, slot));
                }
                Some(_) => {}
            }
        }
        None
    }

    /// Port of `clearDeadQueue` (web_conn.go:726): empty up to the first gap, pointer to 0.
    pub fn clear(&mut self) {
        for slot in &mut self.slots {
            if slot.is_none() {
                break;
            }
            *slot = None;
        }
        self.pointer = 0;
    }

    /// The frames `drainDeadQueue` (web_conn.go:739) writes, from `index` to the newest.
    ///
    /// Before the ring has wrapped — the pointer's slot is still empty — that is `index` up to the
    /// pointer. After, it walks forward from `index` through the end of the ring and stops where
    /// the sequence goes *down*, which is the oldest entry. Go's loop has no other exit; the cap
    /// of one lap here only turns an impossible infinite loop into a stop.
    pub fn drain_from(&self, index: usize) -> Vec<&DeadQueueEntry> {
        if self.slots[0].is_none() {
            return Vec::new();
        }
        if self.slots[self.pointer].is_none() {
            return (index..self.pointer)
                .filter_map(|i| self.slots[i].as_ref())
                .collect();
        }
        let mut out = Vec::new();
        let mut current = index;
        for _ in 0..DEAD_QUEUE_SIZE {
            let Some(entry) = self.slots[current].as_ref() else {
                break;
            };
            out.push(entry);
            current = (current + 1) % DEAD_QUEUE_SIZE;
            let Some(next) = self.slots[current].as_ref() else {
                break;
            };
            if entry.event.get_sequence() > next.event.get_sequence() {
                break;
            }
        }
        out
    }
}

/// What a disconnected connection leaves for a client that comes back: the active queue, still
/// receiving broadcasts, and the dead queue of what was already written.
#[derive(Debug)]
pub struct ParkedQueues {
    pub active: mpsc::Receiver<OutgoingFrame>,
    pub dead: DeadQueue,
}

/// Port of `platform.CheckConnResult` (web_conn.go:151) — what a resumed connection inherits.
#[derive(Debug)]
pub struct CheckConnResult {
    /// The same channel's sending half: the broadcasts already queued and those still to come go
    /// to one receiver, which the new pump takes over.
    send: mpsc::Sender<OutgoingFrame>,
    pub queues: ParkedQueues,
    /// The old connection's `reuse_count` plus one.
    pub reuse_count: usize,
}

/// One frame bound for a connection.
///
/// Two variants because Go's `send` channel is a `chan model.WebSocketMessage` carrying either a
/// `*WebSocketEvent` or a `*WebSocketResponse`, and the write pump treats them differently: only
/// the event gets a sequence number.
///
/// Not `Clone`: `WebSocketResponse` deliberately is not, because its `AppError` wraps a cause that
/// a derived `Clone` would drop, changing how the response serialises.
///
/// # `precomputed` is wire format, not an optimisation
///
/// Go encodes a queued event one of two ways, and a client can tell them apart:
///
/// - `Hub.Broadcast` calls `msg.PrecomputeJSON()` (web_hub.go:718), so every *broadcast* event
///   leaves through `precomputedJSONBuf` — hand-concatenated, **a space after each colon**, and
///   no trailing newline;
/// - anything else — `hello`, and every `WebSocketResponse` — goes through `json.Encoder.Encode`,
///   which is compact **and appends a newline**.
///
/// So `{"event": "posted", …}` and `{"event":"hello",…}\n` are both correct, for different
/// frames. Measured against the running Go server, not inferred.
#[derive(Debug)]
pub enum OutgoingFrame {
    Event {
        /// Boxed: `WebSocketEvent` is by far the larger variant, and every queue slot would
        /// otherwise pay for it.
        event: Box<WebSocketEvent>,
        /// True when this frame took `Hub.Broadcast`'s precompute path — see above.
        precomputed: bool,
    },
    Response(Box<WebSocketResponse>),
}

/// Port of `platform.WebConn` (web_conn.go:84), reduced to the state the fan-out consults.
///
/// Everything a *reader* of this struct needs is behind a lock because the hub hands out
/// `Arc<WebConn>` and the connection's own read pump mutates presence and session concurrently
/// with a broadcast.
#[derive(Debug)]
pub struct WebConn {
    /// Go's `WebConn.connectionID`: set at construction and **replaced** at most once, by the
    /// write pump, when a client comes back having missed more than the dead queue holds.
    connection_id: RwLock<String>,

    /// Go's `WebConn.UserId`. A lock rather than a plain field because Go **assigns** it after
    /// construction: `authentication_challenge` sets `conn.UserId = session.UserId`
    /// (websocket_router.go:57) on a connection that was upgraded with no session. Until
    /// 2026-09-15 this was an immutable field filled from the upgrade-time session, so a
    /// connection that authenticated over the socket was registered under the user `""` — and
    /// every user-addressed event, `hello` included, was addressed to nobody.
    ///
    /// Deliberately **not** derived from [`WebConn::session`]: Go's `InvalidateCache` clears the
    /// session and leaves `UserId`, and the hub index is keyed by this value.
    user_id: RwLock<String>,

    /// Go's `WebConn.PostedAck`: the client connected with `?posted_ack=true`
    /// (api4/websocket.go:81). Read by one thing only — the `posted_ack` broadcast hook — and it
    /// is the client's promise to acknowledge `posted` frames that carry `should_ack`.
    pub posted_ack: bool,

    /// The session this connection authenticated with. Replaced wholesale when the connection
    /// re-authenticates, which is why it is a lock over the whole struct rather than per field.
    session: RwLock<Session>,

    /// Go's `Active atomic.Bool`. False between `unregister` and the connection actually going
    /// away; an inactive connection is skipped by the fan-out's logging but still receives.
    active: AtomicBool,

    send: mpsc::Sender<OutgoingFrame>,

    /// Presence, set by the `presence` websocket action. `<>` until the client says otherwise —
    /// see [`UNSET_PRESENCE_INDICATOR`].
    active_channel_id: RwLock<String>,
    active_rhs_thread_channel_id: RwLock<String>,
    active_thread_view_thread_channel_id: RwLock<String>,

    /// Go's `allChannelMembers` + `lastAllChannelMembersTime`: channel id → roles, refreshed
    /// every [`WEB_CONN_MEMBER_CACHE_TIME`] ms. Only the *keys* are ever read; the roles come
    /// along because the store call returns them.
    all_channel_members: RwLock<Option<(HashMap<String, String>, i64)>>,

    /// Go's `reuseCount`: how many times this connection's queues have been handed to a client
    /// that reconnected. `hello` is queued on registration only when it is zero.
    pub reuse_count: usize,

    /// Go's `lastUserActivityAt`: construction time, then whatever `UpdateActivity` records for
    /// this connection's session. Read by the reaper and by the unregister arm's away test.
    last_user_activity_at: AtomicI64,

    /// The queues this connection left in the hub when its socket went away — see [`Hub::park`].
    parked: Mutex<Option<ParkedQueues>>,

    /// Go's `close(wc.send)` as the write pump sees it: the hub has removed this connection and
    /// the socket should close. See [`Hub::close_and_remove`].
    close: tokio::sync::Notify,
}

impl WebConn {
    /// Port of `PlatformService.NewWebConn` (web_conn.go:200), minus the TCP_NODELAY tweak and the
    /// plugin connect hook.
    ///
    /// Returns the connection and the receiving half of its queue; `mm-api`'s write pump owns the
    /// receiver until the socket goes away, then parks it in the hub ([`Hub::park`]).
    pub fn new(
        connection_id: String,
        session: Session,
        posted_ack: bool,
    ) -> (Arc<WebConn>, mpsc::Receiver<OutgoingFrame>) {
        let (tx, rx) = mpsc::channel(SEND_QUEUE_SIZE);
        (
            Self::build(connection_id, session, posted_ack, tx, 0, true),
            rx,
        )
    }

    /// `NewWebConn` given a config `PopulateWebConnConfig` filled from a found connection
    /// (web_conn.go:183-190): the old id and queues, `reuse_count` one higher, and **not active**
    /// until [`Hub::register`] makes it so. Returns the queues for the write pump.
    pub fn resume(
        connection_id: String,
        session: Session,
        posted_ack: bool,
        found: CheckConnResult,
    ) -> (Arc<WebConn>, ParkedQueues) {
        let conn = Self::build(
            connection_id,
            session,
            posted_ack,
            found.send,
            found.reuse_count,
            false,
        );
        (conn, found.queues)
    }

    fn build(
        connection_id: String,
        session: Session,
        posted_ack: bool,
        send: mpsc::Sender<OutgoingFrame>,
        reuse_count: usize,
        active: bool,
    ) -> Arc<WebConn> {
        Arc::new(WebConn {
            connection_id: RwLock::new(connection_id),
            user_id: RwLock::new(session.user_id.clone()),
            posted_ack,
            session: RwLock::new(session),
            active: AtomicBool::new(active),
            send,
            active_channel_id: RwLock::new(UNSET_PRESENCE_INDICATOR.to_owned()),
            active_rhs_thread_channel_id: RwLock::new(UNSET_PRESENCE_INDICATOR.to_owned()),
            active_thread_view_thread_channel_id: RwLock::new(UNSET_PRESENCE_INDICATOR.to_owned()),
            all_channel_members: RwLock::new(None),
            reuse_count,
            last_user_activity_at: AtomicI64::new(get_millis()),
            parked: Mutex::new(None),
            close: tokio::sync::Notify::new(),
        })
    }

    /// Go's `GetConnectionID`.
    pub fn connection_id(&self) -> String {
        self.connection_id
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Go's `SetConnectionID` (web_conn.go:323) — the write pump's, on message loss.
    pub fn set_connection_id(&self, connection_id: impl Into<String>) {
        *self
            .connection_id
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = connection_id.into();
    }

    pub fn last_user_activity_at(&self) -> i64 {
        self.last_user_activity_at.load(Ordering::Acquire)
    }

    /// Resolves once the hub has closed and removed this connection ([`Hub::close_and_remove`]).
    /// The notification is stored if nobody is waiting, so a pump that starts waiting late still
    /// sees it.
    pub async fn closed(&self) {
        self.close.notified().await;
    }

    fn take_parked(&self) -> Option<ParkedQueues> {
        self.parked
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
    }

    /// A snapshot of the session. Cloned rather than borrowed so no caller holds the lock across
    /// an `.await`.
    pub fn session(&self) -> Session {
        self.session
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Go's `WebConn.UserId`, read. Cloned for the same reason [`WebConn::session`] is.
    pub fn user_id(&self) -> String {
        self.user_id
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// `conn.UserId = session.UserId` (websocket_router.go:57). Call before [`Hub::register`]:
    /// the index is keyed by this value at registration.
    pub fn set_user_id(&self, user_id: impl Into<String>) {
        *self
            .user_id
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = user_id.into();
    }

    /// Port of `(*WebConn).SetSession` (web_conn.go:398).
    pub fn set_session(&self, session: Session) {
        *self
            .session
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = session;
    }

    /// Port of `(*WebConn).SetActiveChannelID` (web_conn.go:336) and its three siblings.
    pub fn set_presence(&self, which: Presence, value: &str) {
        let cell = match which {
            Presence::Channel => &self.active_channel_id,
            Presence::RhsThreadChannel => &self.active_rhs_thread_channel_id,
            Presence::ThreadViewThreadChannel => &self.active_thread_view_thread_channel_id,
        };
        *cell
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = value.to_owned();
    }

    fn presence(&self, which: Presence) -> String {
        let cell = match which {
            Presence::Channel => &self.active_channel_id,
            Presence::RhsThreadChannel => &self.active_rhs_thread_channel_id,
            Presence::ThreadViewThreadChannel => &self.active_thread_view_thread_channel_id,
        };
        cell.read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Port of `(*WebConn).isSet` (web_conn.go:388) — "the client has told us", as distinct from
    /// "the client told us it has nothing open".
    fn is_set(val: &str) -> bool {
        val != UNSET_PRESENCE_INDICATOR
    }

    /// Port of `(*WebConn).notInChannel` (web_conn.go:1018).
    fn not_in_channel(&self, val: &str) -> bool {
        let active = self.presence(Presence::Channel);
        Self::is_set(&active) && val != active
    }

    /// Port of `(*WebConn).notInThread` (web_conn.go:1022).
    ///
    /// Note the `&&` between the two arms: an *unset* thread indicator makes its arm false, so a
    /// connection that has told us about neither thread view is never "not in thread".
    fn not_in_thread(&self, val: &str) -> bool {
        let rhs = self.presence(Presence::RhsThreadChannel);
        let view = self.presence(Presence::ThreadViewThreadChannel);
        (Self::is_set(&rhs) && val != rhs) && (Self::is_set(&view) && val != view)
    }

    /// Port of `(*WebConn).isMemberOfTeam` (web_conn.go:1029), reading the session's hydrated
    /// `TeamMembers`.
    fn is_member_of_team(&self, team_id: &str) -> bool {
        self.session().get_team_by_team_id(team_id).is_some()
    }

    /// Port of `(*WebConn).InvalidateCache` (web_conn.go:774): the membership cache, the session
    /// and its expiry all go — **the token stays**.
    ///
    /// Go keeps the token in its own field, so after this `IsBasicAuthenticated` sees an expiry of
    /// zero and re-reads the session by that token: a session that is still valid comes back with
    /// whatever changed (roles, team memberships), and a revoked one clears the token and leaves
    /// the connection unauthenticated. Here the token lives inside [`Session`], so it is carried
    /// over into an otherwise empty one, which [`App::conn_is_authenticated`] treats identically.
    pub fn invalidate_cache(&self) {
        self.invalidate_channel_members();
        let mut session = self
            .session
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *session = Session {
            token: std::mem::take(&mut session.token),
            ..Session::default()
        };
    }

    /// The membership half of [`WebConn::invalidate_cache`] alone.
    pub fn invalidate_channel_members(&self) {
        *self
            .all_channel_members
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }

    fn cached_channel_members(&self) -> Option<HashMap<String, String>> {
        let guard = self
            .all_channel_members
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match guard.as_ref() {
            Some((members, at)) if get_millis() - *at <= WEB_CONN_MEMBER_CACHE_TIME => {
                Some(members.clone())
            }
            _ => None,
        }
    }

    fn store_channel_members(&self, members: HashMap<String, String>) {
        *self
            .all_channel_members
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((members, get_millis()));
    }

    /// Whether the connection's queue has passed [`SEND_SLOW_WARN`].
    ///
    /// `capacity()` is the *remaining* room, so this is Go's `len(wc.send) >= sendSlowWarn`
    /// rewritten against what tokio exposes.
    fn queue_is_slow(&self) -> bool {
        SEND_QUEUE_SIZE - self.send.capacity() >= SEND_SLOW_WARN
    }

    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }

    pub(crate) fn set_active(&self, value: bool) {
        self.active.store(value, Ordering::Release);
    }

    /// Try to enqueue a frame. Go's `select` with a `default` arm: a full queue is not backpressure
    /// but a *disconnect* — the connection is closed and removed.
    fn try_send(
        &self,
        frame: OutgoingFrame,
    ) -> Result<(), mpsc::error::TrySendError<OutgoingFrame>> {
        self.send.try_send(frame)
    }
}

/// Which presence slot [`WebConn::set_presence`] addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    Channel,
    RhsThreadChannel,
    ThreadViewThreadChannel,
}

/// A boxed future, which is how a `dyn` trait carries an async method without `async_trait`.
pub type HookFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// Port of `platform.BroadcastHook` (web_broadcast_hook.go:11).
///
/// Implementations are in [`crate::broadcast_hooks`]. `args` is the map the raiser passed to
/// `WebsocketBroadcast::add_hook` for this hook, positionally paired with its id. `suite` is
/// Go's `webConn.Suite` and `webConn.Platform.Store` folded into one: the lookups a hook makes
/// on the recipient's behalf. The method is async because `channel_mentions` reads the database
/// per connection; the three pure hooks return a ready future.
pub trait BroadcastHook: Send + Sync {
    /// Modify `msg` for this connection, or leave it alone. An error is logged by the runner and
    /// does not stop the broadcast.
    fn process<'a, 'e>(
        &'a self,
        msg: &'a mut HookedWebSocketEvent<'e>,
        conn: &'a WebConn,
        args: &'a StringInterface,
        suite: &'a dyn BroadcastHookSuite,
    ) -> HookFuture<'a, Result<(), BroadcastHookError>>
    where
        'e: 'a;
}

/// What a hook may ask of the server for the connection it is processing — the slice of
/// `webConn.Suite` (`SuiteIFace`) and `webConn.Platform.Store` the ported hooks use.
///
/// One method so far. `channelMentionsBroadcastHook` does `Store.Channel().Get(channelID, true)`
/// and then `Suite.HasPermissionToResolveChannelMention(rctx, webConn.UserId, channel)`; a
/// channel the store cannot find is "not resolvable", so the two collapse into one question.
pub trait BroadcastHookSuite: Send + Sync {
    fn has_permission_to_resolve_channel_mention<'a>(
        &'a self,
        user_id: &'a str,
        channel_id: &'a str,
    ) -> HookFuture<'a, bool>;
}

/// Port of `platform.HookedWebSocketEvent` (web_broadcast_hook.go:42).
///
/// A hook sees the event through this and never directly, so the shared event every other
/// connection is about to receive cannot be modified by accident: the first mutating call makes
/// a copy (Go's `RemovePrecomputedJSON`) and later calls work on that.
///
/// **The copy is the wire difference.** The shared event carries precomputed JSON and leaves in
/// the spaced, unterminated form; the copy has none and leaves through `json.Encoder` — compact,
/// newline-terminated. So a connection whose hooks changed nothing gets a different encoding of
/// the same bytes than a connection whose hooks added a key. [`Hub::run_broadcast_hooks`]
/// reports which happened by returning the copy, and [`broadcast_frame`] turns that into the
/// [`OutgoingFrame::Event`] `precomputed` flag.
#[derive(Debug)]
pub struct HookedWebSocketEvent<'a> {
    original: &'a WebSocketEvent,
    copy: Option<WebSocketEvent>,
}

impl<'a> HookedWebSocketEvent<'a> {
    /// Port of `MakeHookedWebSocketEvent` (web_broadcast_hook.go:47).
    pub fn new(original: &'a WebSocketEvent) -> Self {
        Self {
            original,
            copy: None,
        }
    }

    /// Port of `(*HookedWebSocketEvent).Add` (web_broadcast_hook.go:53).
    pub fn add(&mut self, key: impl Into<String>, value: serde_json::Value) {
        self.copy_if_necessary();
        if let Some(copy) = self.copy.as_mut() {
            copy.add(key, value);
        }
    }

    /// Port of `(*HookedWebSocketEvent).EventType` (web_broadcast_hook.go:59).
    pub fn event_type(&self) -> &str {
        self.copy
            .as_ref()
            .map_or(self.original, |copy| copy)
            .event_type()
    }

    /// Port of `(*HookedWebSocketEvent).Get` (web_broadcast_hook.go:68) — a value from the event
    /// data as the hooks so far have left it. Never mutate through this.
    ///
    /// `None` for an absent key **and** for a JSON `null`: Go's callers compare the result to
    /// `nil`, and a decoded `null` is `nil` there.
    pub fn get(&self, key: &str) -> Option<&serde_json::Value> {
        self.copy
            .as_ref()
            .map_or(self.original, |copy| copy)
            .get_data()?
            .get(key)
            .filter(|value| !value.is_null())
    }

    /// Port of `copyIfNecessary` (web_broadcast_hook.go:77).
    fn copy_if_necessary(&mut self) {
        if self.copy.is_none() {
            self.copy = Some(self.original.deep_copy_like_go());
        }
    }

    /// Port of `(*HookedWebSocketEvent).Event` (web_broadcast_hook.go:83), which returns the
    /// copy if one was made and the original otherwise. Here the caller holds the original
    /// already, so only the copy comes back: `Some` means a hook modified the event.
    pub fn into_copy(self) -> Option<WebSocketEvent> {
        self.copy
    }
}

/// Port of `platform.hubConnectionIndex` (web_hub.go:854), with the channel index omitted —
/// `EnableWebHubChannelIteration` defaults to **false** (config.go:1061), so the live path is the
/// membership query, not the index.
#[derive(Debug, Default)]
struct HubIndex {
    by_conn: HashMap<String, Arc<WebConn>>,
    by_user: HashMap<String, Vec<Arc<WebConn>>>,
}

/// Port of `platform.Hub` (web_hub.go).
///
/// Go runs one goroutine per hub and serialises every operation through channels; here a
/// `RwLock` over the index does the same job. The difference is not observable: Go's hub loop
/// holds no state across iterations that a lock would not protect.
pub struct Hub {
    index: RwLock<HubIndex>,
    /// Go's `Hub.broadcastHooks`, handed to every hub by `hubStart` (web_hub.go:136) from
    /// `Server.makeBroadcastHooks`.
    broadcast_hooks: HashMap<&'static str, Box<dyn BroadcastHook>>,
    /// When the inactive-connection reaper last ran — see [`Hub::reap_if_due`].
    last_reap: AtomicI64,
}

impl fmt::Debug for Hub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut hook_ids: Vec<&str> = self.broadcast_hooks.keys().copied().collect();
        hook_ids.sort_unstable();
        f.debug_struct("Hub")
            .field("index", &self.index)
            .field("broadcast_hooks", &hook_ids)
            .finish()
    }
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}

impl Hub {
    /// A hub with the stock hooks — `hubStart(s.makeBroadcastHooks())`.
    pub fn new() -> Self {
        Self::with_hooks(crate::broadcast_hooks::make_broadcast_hooks())
    }

    /// A hub with exactly these hooks. Tests use it to see the runner without the registry.
    pub fn with_hooks(broadcast_hooks: HashMap<&'static str, Box<dyn BroadcastHook>>) -> Self {
        Self {
            index: RwLock::new(HubIndex::default()),
            broadcast_hooks,
            last_reap: AtomicI64::new(get_millis()),
        }
    }

    /// Port of `Hub.runBroadcastHooks` (web_broadcast_hook.go:17).
    ///
    /// Runs each hook in `hook_ids`, in order, against `msg` for `conn`. Returns the modified
    /// copy if any hook changed the event, and `None` when none did — in which case the caller
    /// sends the shared `msg` itself, precomputed. An id the registry does not know is logged and
    /// skipped, as is a hook that fails; neither stops the broadcast or the later hooks.
    ///
    /// Go indexes `hookArgs[i]` unconditionally and would panic past the end. `add_hook` appends
    /// to both slices, so only a hand-built broadcast can get here with fewer args than ids; such
    /// a hook sees an empty map and reports its missing key through the ordinary error path.
    pub async fn run_broadcast_hooks(
        &self,
        msg: &WebSocketEvent,
        conn: &WebConn,
        hook_ids: &[String],
        hook_args: &[StringInterface],
        suite: &dyn BroadcastHookSuite,
    ) -> Option<WebSocketEvent> {
        if hook_ids.is_empty() {
            return None;
        }

        let mut hooked = HookedWebSocketEvent::new(msg);
        let no_args = StringInterface::new();

        for (i, hook_id) in hook_ids.iter().enumerate() {
            let args = hook_args.get(i).unwrap_or(&no_args);
            let Some(hook) = self.broadcast_hooks.get(hook_id.as_str()) else {
                tracing::warn!(
                    hook_id = %hook_id,
                    "runBroadcastHooks: Unable to find broadcast hook"
                );
                continue;
            };

            if let Err(err) = hook.process(&mut hooked, conn, args, suite).await {
                tracing::warn!(
                    hook_id = %hook_id,
                    error = %err,
                    "runBroadcastHooks: Error processing hook"
                );
            }
        }

        hooked.into_copy()
    }

    fn write_index(&self) -> RwLockWriteGuard<'_, HubIndex> {
        self.index
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Port of `Hub.Register` (web_hub.go:378) plus the register arm of the hub loop
    /// (web_hub.go:586).
    ///
    /// Marks the connection active and indexes it, then queues `hello` — **only when
    /// `reuse_count` is zero**. A resumed client keeps the id it already has; if it missed too
    /// much to resume, the write pump sends it a new `hello` itself. Go also requires
    /// `IsBasicAuthenticated`; every caller registers a connection whose session it has just
    /// resolved, so that half cannot be false here.
    pub fn register(&self, conn: Arc<WebConn>, hello: WebSocketEvent) {
        conn.set_active(true);
        {
            let mut index = self.write_index();
            self.reap_if_due(&mut index);
            index.by_conn.insert(conn.connection_id(), conn.clone());
            index
                .by_user
                .entry(conn.user_id())
                .or_default()
                .push(conn.clone());
        }
        if conn.reuse_count == 0 {
            // A fresh queue cannot be full, so the error arm is unreachable rather than ignored.
            // `hello` does *not* go through `PrecomputeJSON` — it is queued directly by the hub
            // loop (web_hub.go:604), so it leaves compact and newline-terminated.
            let _ = conn.try_send(OutgoingFrame::Event {
                event: Box::new(hello),
                precomputed: false,
            });
        }
    }

    /// The first half of the unregister arm (web_hub.go:607): the connection goes **inactive and
    /// stays indexed**, holding its queues for a client that comes back ([`Hub::check_conn`]).
    /// Broadcasts keep reaching it and wait in the queue until it is resumed, reaped, or the
    /// queue fills. A connection the hub has already removed drops its queues instead.
    ///
    /// The status half needs the app — see [`App::hub_unregister`].
    pub fn park(&self, conn: &Arc<WebConn>, queues: ParkedQueues) {
        let mut index = self.write_index();
        conn.set_active(false);
        if index_has(&index, conn) {
            *conn
                .parked
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(queues);
        }
        self.reap_if_due(&mut index);
    }

    /// Port of `Hub.CheckConn` (web_hub.go:414) and its arm (web_hub.go:568), which is
    /// `hubConnectionIndex.RemoveInactiveByConnectionID` (web_hub.go:1017).
    ///
    /// An **inactive** connection of this user whose id is `connection_id` is taken out of the
    /// index and its queues handed over, with `reuse_count` one higher. An active connection with
    /// that id is not a match — its client never left — and neither is another user's.
    pub fn check_conn(&self, user_id: &str, connection_id: &str) -> Option<CheckConnResult> {
        if user_id.is_empty() {
            return None;
        }
        let mut index = self.write_index();
        self.reap_if_due(&mut index);
        let conn = index
            .by_user
            .get(user_id)?
            .iter()
            .find(|c| c.connection_id() == connection_id && !c.is_active())?
            .clone();
        remove_from_index(&mut index, &conn);
        let queues = conn.take_parked()?;
        Some(CheckConnResult {
            // Cloned: the resumed connection must feed the very channel whose receiver it inherits.
            send: conn.send.clone(),
            queues,
            reuse_count: conn.reuse_count + 1,
        })
    }

    /// Port of `closeAndRemoveConn` (web_hub.go:825): out of the index and its queues dropped,
    /// and a live socket told to close — Go closes the send channel, and the write pump answers
    /// that with a close frame.
    pub fn close_and_remove(&self, conn: &Arc<WebConn>) {
        {
            let mut index = self.write_index();
            remove_from_index(&mut index, conn);
        }
        drop(conn.take_parked());
        conn.close.notify_one();
    }

    /// Port of `hubConnectionIndex.RemoveInactiveConnections` (web_hub.go:1033) — drop every
    /// inactive connection whose last activity is more than [`INACTIVE_CONN_REAPER_INTERVAL`] ago.
    ///
    /// Go runs it on a five-minute ticker; this hub has no loop of its own, so it runs on the
    /// first registration, park or resume lookup after five minutes have passed. Either way a
    /// connection can outlive the threshold by up to one interval, and the check a resuming
    /// client depends on — "is it still there" — runs the reaper first, so it never finds one Go's
    /// ticker would already have dropped.
    fn reap_if_due(&self, index: &mut HubIndex) {
        let now = get_millis();
        if now - self.last_reap.load(Ordering::Acquire) < INACTIVE_CONN_REAPER_INTERVAL {
            return;
        }
        self.last_reap.store(now, Ordering::Release);
        let stale: Vec<Arc<WebConn>> = index
            .by_conn
            .values()
            .filter(|c| {
                !c.is_active() && now - c.last_user_activity_at() > INACTIVE_CONN_REAPER_INTERVAL
            })
            .cloned()
            .collect();
        for conn in stale {
            remove_from_index(index, &conn);
            drop(conn.take_parked());
        }
    }

    /// Port of `Hub.UpdateActivity` (web_hub.go:480) and its arm (web_hub.go:691): the active
    /// connections of this user that authenticated with this token record `activity_at`.
    pub fn update_activity(&self, user_id: &str, session_token: &str, activity_at: i64) {
        for conn in self.for_user(user_id) {
            if conn.is_active() && conn.session().token == session_token {
                conn.last_user_activity_at
                    .store(activity_at, Ordering::Release);
            }
        }
    }

    /// Port of `hubConnectionIndex.ForUser` (web_hub.go:975).
    fn for_user(&self, user_id: &str) -> Vec<Arc<WebConn>> {
        self.index
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .by_user
            .get(user_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Port of `hubConnectionIndex.ForConnection` (web_hub.go:1006).
    fn for_connection(&self, connection_id: &str) -> Option<Arc<WebConn>> {
        self.index
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .by_conn
            .get(connection_id)
            .cloned()
    }

    /// Port of `Hub.InvalidateAll` (web_hub.go:471) and its arm (web_hub.go:679), reached from
    /// `ClearSessionCacheForAllUsersSkipClusterSend` (cluster_handlers.go:85) — every connection
    /// is [`WebConn::invalidate_cache`]d **and loses its token**, so the next authentication check
    /// short-circuits to unauthenticated instead of re-reading a session by it.
    ///
    /// Not reached from `InvalidateAllCaches`, which purges the session cache only; see
    /// [`App::invalidate_all_caches`].
    pub fn invalidate_all(&self) {
        for conn in self.all() {
            conn.invalidate_cache();
            conn.set_session(Session::default());
        }
    }

    fn all(&self) -> Vec<Arc<WebConn>> {
        self.index
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .by_conn
            .values()
            .cloned()
            .collect()
    }

    /// Port of `hubConnectionIndex.ForUserActiveCount` (web_hub.go:995).
    pub fn conn_count_for_user(&self, user_id: &str) -> usize {
        self.for_user(user_id)
            .iter()
            .filter(|c| c.is_active())
            .count()
    }

    /// Port of `hubConnectionIndex.AllActive` (web_hub.go:1045) — Go's `Hub.connectionCount`,
    /// which is what `total_websocket_connections` reports. **Active only**: a parked connection
    /// is indexed and not counted.
    pub fn conn_count(&self) -> usize {
        self.index
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .by_conn
            .values()
            .filter(|c| c.is_active())
            .count()
    }

    /// The hub leg of `PlatformService.InvalidateCacheForUser` (web_hub.go:233).
    ///
    /// Until 2026-09-15 this dropped only the membership cache. Go's chain is
    /// `InvalidateChannelCacheForUser` → `invalidateWebConnSessionCacheForUser` → `Hub.InvalidateUser`,
    /// whose arm (web_hub.go:663) is the full [`WebConn::invalidate_cache`] — so the session goes
    /// too, and a connection re-reads it on its next event.
    pub fn invalidate_channel_members_for_user(&self, user_id: &str) {
        self.invalidate_user(user_id);
    }

    /// Port of `Hub.InvalidateUser` (web_hub.go:462) and its arm (web_hub.go:663), with
    /// `EnableWebHubChannelIteration` off — the default, and the only index this hub keeps.
    pub fn invalidate_user(&self, user_id: &str) {
        for conn in self.for_user(user_id) {
            conn.invalidate_cache();
        }
    }

    /// Port of `Hub.SendMessage` (web_hub.go:492) and the direct-message arm (web_hub.go:700): a
    /// frame for a connection the hub does not hold is dropped, and a full queue closes and
    /// removes the connection.
    pub fn send_message(&self, conn: &Arc<WebConn>, frame: OutgoingFrame) {
        let held = index_has(
            &self
                .index
                .read()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            conn,
        );
        if !held {
            return;
        }
        if conn.try_send(frame).is_err() {
            if conn.is_active() {
                tracing::error!(
                    user_id = %conn.user_id(),
                    conn_id = %conn.connection_id(),
                    "webhub.broadcast: cannot send, closing websocket for user"
                );
            }
            self.close_and_remove(conn);
        }
    }
}

/// Port of `hubConnectionIndex.Has` (web_hub.go:969), by identity.
fn index_has(index: &HubIndex, conn: &Arc<WebConn>) -> bool {
    index.by_conn.values().any(|c| Arc::ptr_eq(c, conn))
}

/// Port of `hubConnectionIndex.Remove` (web_hub.go:902), by identity.
///
/// Go deletes the by-id entry under the connection's **current** id, and after a lost-message
/// reconnect that is not the id it was indexed under, so the old key leaks and keeps pointing at
/// the connection. Nothing can reach it through that key — a broadcast addressed to it finds a
/// connection `Has` no longer holds — so removing every entry for the connection is the same
/// behaviour without the leak.
fn remove_from_index(index: &mut HubIndex, conn: &Arc<WebConn>) {
    index.by_conn.retain(|_, c| !Arc::ptr_eq(c, conn));
    let user_id = conn.user_id();
    if let Some(conns) = index.by_user.get_mut(&user_id) {
        conns.retain(|c| !Arc::ptr_eq(c, conn));
        if conns.is_empty() {
            index.by_user.remove(&user_id);
        }
    }
}

impl BroadcastHookSuite for App {
    /// `webConn.Platform.Store.Channel().Get(channelID, true)` — `allowFromCache`, not
    /// `includeDeleted`; the query is `SqlChannelStore.Get`'s, which has no `DeleteAt` predicate
    /// — then `Suite.HasPermissionToResolveChannelMention`. A channel the store does not answer
    /// with is skipped by the hook, which is `false` here.
    fn has_permission_to_resolve_channel_mention<'a>(
        &'a self,
        user_id: &'a str,
        channel_id: &'a str,
    ) -> HookFuture<'a, bool> {
        Box::pin(async move {
            let channel = match self.store().channel().get(channel_id).await {
                Ok(channel) => channel,
                Err(err) => {
                    tracing::debug!(error = %err, channel_id, "channel mention hook: channel lookup failed");
                    return false;
                }
            };
            self.has_permission_to_resolve_channel_mention(user_id, &channel)
                .await
        })
    }
}

impl App {
    /// Port of `PlatformService.Publish` (cluster.go:189) and `PublishSkipClusterSend`
    /// (cluster.go:220).
    ///
    /// The cluster leg is not ported ([D-182]); what remains is the hub leg, and Go's choice of
    /// which hubs to visit — the one hub owning `broadcast.user_id`, or all of them. There is one
    /// hub here, so that choice collapses, but the *targeting* it implies is still applied by
    /// [`App::should_send_event`].
    #[tracing::instrument(skip(self, event), fields(event = %event.event_type()))]
    pub async fn publish(&self, event: WebSocketEvent) {
        // Go strips the hook fields before precomputing the JSON so they never reach a client
        // (web_hub.go:721); the hooks come back out and are run per connection below.
        let (event, hooks, hook_args) = event.without_broadcast_hooks();

        let broadcast = event.get_broadcast().cloned().unwrap_or_default();

        // Go's quick return for a connection-addressed event (web_hub.go:747). It skips
        // ShouldSendEvent's other filters only in the sense that ShouldSendEvent returns on the
        // same field first; the call is still made.
        let targets = if !broadcast.connection_id.is_empty() {
            self.hub()
                .for_connection(&broadcast.connection_id)
                .into_iter()
                .collect()
        } else if !broadcast.user_id.is_empty() {
            self.hub().for_user(&broadcast.user_id)
        } else {
            self.hub().all()
        };

        for conn in targets {
            if !self.should_send_event(&conn, &event).await {
                continue;
            }
            // `webConn.send <- h.runBroadcastHooks(msg, webConn, ...)` (web_hub.go:731).
            let hooked = self
                .hub()
                .run_broadcast_hooks(&event, &conn, &hooks, &hook_args, self)
                .await;
            if conn.try_send(broadcast_frame(&event, hooked)).is_err() {
                // "Don't log the warning if it's an inactive connection."
                if conn.is_active() {
                    tracing::error!(
                        user_id = %conn.user_id(),
                        conn_id = %conn.connection_id(),
                        "webhub.broadcast: cannot send, closing websocket for user"
                    );
                }
                self.hub().close_and_remove(&conn);
            }
        }
    }

    /// Port of `(*WebConn).ShouldSendEvent` (web_conn.go:884).
    ///
    /// The decision splits in two. [`addressing_verdict`] is everything that depends only on the
    /// connection and the event — which is all of it except two lookups: the `manage_system`
    /// permission and, for a channel-scoped event, the user's memberships. Those are the shell.
    ///
    /// The split exists so the ordering can be tested. The order of the filters *is* the
    /// behaviour: `connection_id` wins over `user_id`, which wins over `omit_users`, which wins
    /// over `channel_id`, which wins over `team_id`. A broadcast carrying two of them is not an
    /// intersection — the earlier field decides and the later is never consulted — so reordering
    /// them silently widens or narrows the audience of every event that sets more than one.
    pub async fn should_send_event(&self, conn: &WebConn, event: &WebSocketEvent) -> bool {
        if !self.conn_is_authenticated(conn).await {
            return false;
        }

        // Go computes this lazily and memoizes it across the two arms that read it. Hoisted here
        // because the pure half cannot await; the condition is the union of the two arms' guards,
        // so the lookup happens on exactly the events Go would have made it for.
        let broadcast_flags = event
            .get_broadcast()
            .map(|b| (b.contains_sanitized_data, b.contains_sensitive_data));
        let has_manage_system = match broadcast_flags {
            Some((true, _)) | Some((_, true)) => {
                Some(self.session_grants_manage_system(conn).await)
            }
            _ => None,
        };

        match addressing_verdict(conn, event, has_manage_system) {
            Verdict::Send => true,
            Verdict::Skip => false,
            Verdict::GuestVisibility => self.should_send_event_to_guest(conn, event).await,
            Verdict::RequiresChannelMembership(channel_id) => {
                let members = match conn.cached_channel_members() {
                    Some(members) => members,
                    None => match self
                        .store()
                        .channel()
                        .get_all_channel_members_for_user(&conn.user_id(), false)
                        .await
                    {
                        Ok(members) => {
                            conn.store_channel_members(members.clone());
                            members
                        }
                        Err(err) => {
                            tracing::error!(error = %err, "webhub.shouldSendEvent");
                            return false;
                        }
                    },
                };
                members.contains_key(&channel_id)
            }
        }
    }

    /// Port of `(*WebConn).ShouldSendEventToGuest` (web_conn.go:852): for the two user events,
    /// whether this guest may see the user — [`App::user_can_see_other_user`], asked as the
    /// connection's user. A lookup that fails withholds the event, as Go's does.
    async fn should_send_event_to_guest(&self, conn: &WebConn, event: &WebSocketEvent) -> bool {
        match guest_subject(event) {
            GuestSubject::Anyone => true,
            GuestSubject::Nobody => false,
            GuestSubject::User(other_user_id) => {
                match self
                    .user_can_see_other_user(&conn.user_id(), &other_user_id)
                    .await
                {
                    Ok(can_see) => can_see,
                    Err(err) => {
                        tracing::error!(error = %err, "webhub.shouldSendEvent.");
                        false
                    }
                }
            }
        }
    }

    /// Port of `(*WebConn).IsAuthenticated` (web_conn.go:824), basic half — which is also
    /// `IsBasicAuthenticated` (web_conn.go:782), the check the pong handler makes.
    ///
    /// Go re-fetches the session by token once it has expired, and gives up — clearing the
    /// connection's session — if the fetch fails. The MFA half is not ported ([D-184]).
    ///
    /// Public because the socket router asks it before any `wsapi` action (websocket_router.go:109)
    /// — the question is the expiry-aware one, not "was a user ever attached".
    pub async fn conn_is_authenticated(&self, conn: &WebConn) -> bool {
        let session = conn.session();
        if session.expires_at >= get_millis() {
            return true;
        }
        if session.token.is_empty() {
            return false;
        }
        match self.get_session(&session.token).await {
            Ok(fresh) => {
                conn.set_session(fresh);
                true
            }
            Err(err) => {
                tracing::debug!(error = %err, "websocket: invalid session");
                conn.set_session(Session::default());
                false
            }
        }
    }

    async fn session_grants_manage_system(&self, conn: &WebConn) -> bool {
        let session = conn.session();
        let roles: Vec<String> = session
            .get_user_roles()
            .into_iter()
            .map(|r| r.to_owned())
            .collect();
        self.roles_grant_permission(&roles, &mm_model::permission::PERMISSION_MANAGE_SYSTEM.id)
            .await
    }

    /// Port of `(*WebConn).createHelloMessage` (web_conn.go:828).
    ///
    /// **`server_version` cannot match Go's.** Go builds it from `model.CurrentVersion`,
    /// `model.BuildNumber`, the hash of the client configuration, and whether a licence manager
    /// exists — three of which are properties of the Go binary. The key is present and shaped the
    /// same; its value is this server's own, and always will be: there is nothing to defer, so
    /// this is stated here rather than in the backlog. `server_hostname` differs the same way.
    pub fn hello_message(&self, conn: &WebConn) -> WebSocketEvent {
        let mut hello =
            WebSocketEvent::new(WEBSOCKET_EVENT_HELLO, "", "", &conn.user_id(), None, "");
        hello.add(
            "server_version",
            serde_json::Value::String(self.server_version_string()),
        );
        hello.add(
            "connection_id",
            serde_json::Value::String(conn.connection_id().clone()),
        );
        if let Ok(hostname) = hostname() {
            hello.add("server_hostname", serde_json::Value::String(hostname));
        }
        hello
    }

    fn server_version_string(&self) -> String {
        // Go's four dot-separated parts, in order: version, build number, client config hash,
        // enterprise-ready flag. Only the shape is matched — see `hello_message`.
        format!(
            "{}.{}.{}.{}",
            mm_model::version::CURRENT_VERSION,
            "0",
            "",
            !self.config().license.is_empty()
        )
    }

    /// The hub this app publishes through.
    pub fn hub(&self) -> &Hub {
        &self.hub
    }

    /// Port of `App.ClearSessionCacheForUser` (app/session.go:309) and
    /// `PlatformService.ClearUserSessionCache` (platform/session.go:105).
    ///
    /// Go clears two things: its session cache, which this server does not keep ([D-087]), and —
    /// through `ClearSessionCacheForUserSkipClusterSend` (cluster_handlers.go:76) — every live
    /// websocket connection's copy of the session. **The second is not a cache in the sense
    /// [D-087] means.** A connection authenticated with a session keeps acting as it until told
    /// otherwise: a revoked session keeps receiving events, and a changed role or team membership
    /// is not seen by the addressing filters. So this is not a no-op here, whatever the session
    /// cache is.
    pub fn clear_session_cache_for_user(&self, user_id: &str) {
        self.hub().invalidate_user(user_id);
    }

    /// Port of `PlatformService.HubUnregister` (web_hub.go:190) and the whole unregister arm
    /// (web_hub.go:607): park the connection ([`Hub::park`]), then settle the user's status.
    ///
    /// - **No active connection left** — including none at all — is `QueueSetStatusOffline`. Go
    ///   batches that and flushes every 500ms; the guard, the status written, the cache, the row
    ///   and the broadcast are `SetStatusOffline`'s, which runs here at once. The broadcast reaches
    ///   the parked connection too, so a client that resumes is told it went offline.
    /// - **Otherwise**, the newest activity among the active connections decides: if even that is
    ///   past the away timeout, `SetStatusLastActivityAt` records it and lets the user go away.
    ///
    /// Go's cluster count is skipped: there is one node.
    pub async fn hub_unregister(&self, conn: &Arc<WebConn>, queues: ParkedQueues) {
        self.hub().park(conn, queues);

        let user_id = conn.user_id();
        if user_id.is_empty() {
            return;
        }
        let conns = self.hub().for_user(&user_id);
        if conns.iter().all(|c| !c.is_active()) {
            self.set_status_offline(&user_id, false, false).await;
            return;
        }
        let latest_activity = conns
            .iter()
            .filter(|c| c.is_active())
            .map(|c| c.last_user_activity_at())
            .max()
            .unwrap_or(0);
        if crate::status::is_user_away(
            get_millis(),
            latest_activity,
            self.config().user_status_away_timeout,
        ) {
            self.set_status_last_activity_at(&user_id, latest_activity)
                .await;
        }
    }
}

/// What [`addressing_verdict`] concluded without touching the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Send,
    Skip,
    /// The event is channel-scoped and survived every earlier filter: it goes only to a member of
    /// this channel.
    RequiresChannelMembership(String),
    /// The connection is a guest and the event carries no addressing at all, so Go falls through
    /// to `ShouldSendEventToGuest`.
    GuestVisibility,
}

/// The database-free half of `ShouldSendEvent` (web_conn.go:884), in Go's order.
///
/// `has_manage_system` is `Some` exactly when the broadcast set `contains_sanitized_data` or
/// `contains_sensitive_data` — the two arms that consult the permission. Passing `None` when one
/// of them is set is a caller bug and is treated as "not granted", which is the safe direction for
/// sanitized data and the safe direction for sensitive data too.
pub fn addressing_verdict(
    conn: &WebConn,
    event: &WebSocketEvent,
    has_manage_system: Option<bool>,
) -> Verdict {
    // The slow-queue drop. These three are the high-frequency, low-value events; past half a
    // queue Go discards them rather than let them push a `posted` out.
    if conn.queue_is_slow()
        && matches!(
            event.event_type(),
            WEBSOCKET_EVENT_TYPING
                | WEBSOCKET_EVENT_STATUS_CHANGE
                | WEBSOCKET_EVENT_MULTIPLE_CHANNELS_VIEWED
        )
    {
        return Verdict::Skip;
    }

    let broadcast = match event.get_broadcast() {
        Some(broadcast) => broadcast,
        // Go dereferences `msg.GetBroadcast()` unconditionally and would panic on nil; every
        // constructor sets it. Treated as "no filters" rather than reproducing a panic in library
        // code, which CLAUDE.md forbids.
        None => return Verdict::Send,
    };

    // The sanitized/sensitive split. Setting **both** is a bug in the caller and sends to nobody,
    // which Go states explicitly and unit-tests; that falls out of these two arms without a third
    // branch, for either value of the permission.
    if broadcast.contains_sanitized_data && has_manage_system.unwrap_or(false) {
        return Verdict::Skip;
    }
    if broadcast.contains_sensitive_data && !has_manage_system.unwrap_or(false) {
        return Verdict::Skip;
    }

    if !broadcast.connection_id.is_empty() {
        return verdict(conn.connection_id() == broadcast.connection_id);
    }

    if conn.connection_id() == broadcast.omit_connection_id {
        return Verdict::Skip;
    }

    if !broadcast.user_id.is_empty() {
        return verdict(conn.user_id() == broadcast.user_id);
    }

    if let Some(omit_users) = &broadcast.omit_users
        && !omit_users.is_empty()
        // Go tests key *presence*, not the bool's value: `omit_users: {u: false}` still omits.
        && omit_users.contains_key(&conn.user_id())
    {
        return Verdict::Skip;
    }

    if !broadcast.channel_id.is_empty() {
        let channel_id = &broadcast.channel_id;

        // Typing and reactions are scoped further than membership: a member who does not have the
        // channel or its thread open does not get them.
        if matches!(
            event.event_type(),
            WEBSOCKET_EVENT_TYPING
                | WEBSOCKET_EVENT_REACTION_ADDED
                | WEBSOCKET_EVENT_REACTION_REMOVED
        ) && conn.not_in_channel(channel_id)
            && conn.not_in_thread(channel_id)
        {
            return Verdict::Skip;
        }

        return Verdict::RequiresChannelMembership(channel_id.clone());
    }

    if !broadcast.team_id.is_empty() {
        return verdict(conn.is_member_of_team(&broadcast.team_id));
    }

    if conn.session().is_guest() {
        return Verdict::GuestVisibility;
    }

    Verdict::Send
}

/// Whom a guest's visibility decision is about — the pure half of `ShouldSendEventToGuest`
/// (web_conn.go:852).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuestSubject {
    /// Any other event: sent.
    Anyone,
    /// A guest-scoped event Go cannot read the user from: withheld.
    Nobody,
    /// Sent only if the guest may see this user.
    User(String),
}

/// Exactly two event types are scoped for a guest, and each names its user differently:
///
/// - `user_updated` carries the **user object** — Go's `data["user"].(*model.User)`, which holds
///   for every event a server raises itself. Anything else there fails the assertion and the event
///   is withheld; an object with no `id` is a user with the empty id, as a zero `*model.User` is.
/// - `new_user` carries **`user_id`**, a string. Go's assertion is unchecked and would panic on
///   anything else; that is withheld here rather than reproduced.
pub fn guest_subject(event: &WebSocketEvent) -> GuestSubject {
    let data = event.get_data();
    match event.event_type() {
        WEBSOCKET_EVENT_USER_UPDATED => match data.and_then(|d| d.get("user")) {
            Some(serde_json::Value::Object(user)) => GuestSubject::User(
                user.get("id")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
            ),
            _ => GuestSubject::Nobody,
        },
        WEBSOCKET_EVENT_NEW_USER => match data
            .and_then(|d| d.get("user_id"))
            .and_then(serde_json::Value::as_str)
        {
            Some(user_id) => GuestSubject::User(user_id.to_owned()),
            None => GuestSubject::Nobody,
        },
        _ => GuestSubject::Anyone,
    }
}

/// The frame a broadcast leaves in, given what the hooks did to it.
///
/// `Hub.Broadcast` precomputes the event before fanning out (web_hub.go:723), so the shared event
/// goes out precomputed; a copy a hook made has had that precomputation removed
/// (`RemovePrecomputedJSON`) and goes out through `json.Encoder`. Two encodings, one bit — and a
/// client can see which it got.
pub fn broadcast_frame(event: &WebSocketEvent, hooked: Option<WebSocketEvent>) -> OutgoingFrame {
    match hooked {
        Some(modified) => OutgoingFrame::Event {
            event: Box::new(modified),
            precomputed: false,
        },
        None => OutgoingFrame::Event {
            // One clone per connection, as Go shares one pointer: the queue owns its frame.
            event: Box::new(event.clone()),
            precomputed: true,
        },
    }
}

fn verdict(send: bool) -> Verdict {
    if send { Verdict::Send } else { Verdict::Skip }
}

/// `os.Hostname()`. Go logs a warning and omits `server_hostname` when it fails, which is
/// reproduced by the `Result` here.
fn hostname() -> Result<String, std::io::Error> {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim_end().to_owned())
        .or_else(|_| std::env::var("HOSTNAME").map_err(std::io::Error::other))
}

/// Build a fresh connection id. Port of the `cfg.ConnectionID = model.NewId()` arm of
/// `connectWebSocket` (api4/websocket.go:99).
pub fn new_connection_id() -> String {
    new_id()
}

/// Convenience for handlers: an event addressed to one user with no other filter.
pub fn user_event(event_type: &str, user_id: &str, data: StringInterface) -> WebSocketEvent {
    WebSocketEvent::new(event_type, "", "", user_id, None, "").set_data(data)
}

/// Convenience for handlers: an event addressed to a channel, optionally omitting one connection.
pub fn channel_event(
    event_type: &str,
    team_id: &str,
    channel_id: &str,
    omit_users: Option<BTreeMap<String, bool>>,
    data: StringInterface,
) -> WebSocketEvent {
    WebSocketEvent::new(event_type, team_id, channel_id, "", omit_users, "").set_data(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mm_model::team_member::TeamMember;
    use mm_model::websocket_message::{
        WEBSOCKET_EVENT_POSTED, WEBSOCKET_EVENT_PREFERENCES_CHANGED, WEBSOCKET_EVENT_USER_UPDATED,
    };

    const USER: &str = "6rtg4qbe5bn55mw5t6gphxyaxa";
    const OTHER_USER: &str = "1111111111111111111111111";
    const CHANNEL: &str = "abcdefghijklmnopqrstuvwxyz";
    const TEAM: &str = "zyxwvutsrqponmlkjihgfedcba";
    const CONN: &str = "cccccccccccccccccccccccccc";

    fn session(user_id: &str) -> Session {
        Session {
            id: "sessionid".to_owned(),
            token: "token".to_owned(),
            user_id: user_id.to_owned(),
            // Far future: `conn_is_authenticated` is not on the pure path, but a session that
            // reads as expired anywhere else would make these tests lie by accident.
            expires_at: get_millis() + 60_000,
            roles: "system_user".to_owned(),
            ..Default::default()
        }
    }

    /// A connection with no queue pressure, no presence set, and no team memberships.
    ///
    /// **The receiver comes back with it and callers must keep it.** Dropping it closes the
    /// queue, and `capacity()` — which `queue_is_slow` reads — then reports a closed channel
    /// rather than a full one, so a test that dropped it would be asserting against a connection
    /// that is permanently "slow". Hence `conn(USER).0` at the call sites: the temporary tuple
    /// lives to the end of the statement, which is exactly as long as the assertion needs.
    fn conn(user_id: &str) -> (Arc<WebConn>, mpsc::Receiver<OutgoingFrame>) {
        WebConn::new(CONN.to_owned(), session(user_id), false)
    }

    fn event(event_type: &str) -> WebSocketEvent {
        WebSocketEvent::new(event_type, "", "", "", None, "")
    }

    fn with_broadcast(
        event_type: &str,
        mutate: impl FnOnce(&mut mm_model::websocket_message::WebsocketBroadcast),
    ) -> WebSocketEvent {
        let event = event(event_type);
        let mut broadcast = event.get_broadcast().cloned().unwrap_or_default();
        mutate(&mut broadcast);
        event.set_broadcast(broadcast)
    }

    #[test]
    fn an_unaddressed_event_reaches_everyone() {
        assert_eq!(
            addressing_verdict(&conn(USER).0, &event(WEBSOCKET_EVENT_POSTED), None),
            Verdict::Send
        );
    }

    #[test]
    fn connection_id_is_tested_before_user_id() {
        // Addressed to *this* connection but a different user. Go returns on `connection_id`
        // before it ever reads `user_id`, so this is a send — and a port that checked `user_id`
        // first would drop it.
        let event = with_broadcast(WEBSOCKET_EVENT_POSTED, |b| {
            b.connection_id = CONN.to_owned();
            b.user_id = OTHER_USER.to_owned();
        });
        assert_eq!(
            addressing_verdict(&conn(USER).0, &event, None),
            Verdict::Send
        );
    }

    #[test]
    fn a_different_connection_id_excludes_even_the_addressed_user() {
        let event = with_broadcast(WEBSOCKET_EVENT_POSTED, |b| {
            b.connection_id = "dddddddddddddddddddddddddd".to_owned();
            b.user_id = USER.to_owned();
        });
        assert_eq!(
            addressing_verdict(&conn(USER).0, &event, None),
            Verdict::Skip
        );
    }

    #[test]
    fn omit_connection_id_is_tested_before_user_id() {
        let event = with_broadcast(WEBSOCKET_EVENT_POSTED, |b| {
            b.omit_connection_id = CONN.to_owned();
            b.user_id = USER.to_owned();
        });
        assert_eq!(
            addressing_verdict(&conn(USER).0, &event, None),
            Verdict::Skip
        );
    }

    #[test]
    fn user_id_is_tested_before_channel_id() {
        // The addressed user is someone else. Go returns `false` on `user_id` without asking
        // whether this connection is in the channel — so this must not become a membership
        // question.
        let event = with_broadcast(WEBSOCKET_EVENT_POSTED, |b| {
            b.user_id = OTHER_USER.to_owned();
            b.channel_id = CHANNEL.to_owned();
        });
        assert_eq!(
            addressing_verdict(&conn(USER).0, &event, None),
            Verdict::Skip
        );
    }

    #[test]
    fn omit_users_subtracts_from_a_channel_broadcast() {
        let mut omit = BTreeMap::new();
        omit.insert(USER.to_owned(), true);
        let event = with_broadcast(WEBSOCKET_EVENT_POSTED, |b| {
            b.omit_users = Some(omit);
            b.channel_id = CHANNEL.to_owned();
        });
        assert_eq!(
            addressing_verdict(&conn(USER).0, &event, None),
            Verdict::Skip
        );
    }

    #[test]
    fn omit_users_tests_presence_not_the_bool() {
        // Go's `if _, ok := OmitUsers[wc.UserId]; ok` ignores the value. A port that read the
        // bool would deliver an event Go withholds.
        let mut omit = BTreeMap::new();
        omit.insert(USER.to_owned(), false);
        let event = with_broadcast(WEBSOCKET_EVENT_POSTED, |b| {
            b.omit_users = Some(omit);
        });
        assert_eq!(
            addressing_verdict(&conn(USER).0, &event, None),
            Verdict::Skip
        );
    }

    #[test]
    fn a_channel_scoped_event_becomes_a_membership_question() {
        let event = with_broadcast(WEBSOCKET_EVENT_POSTED, |b| {
            b.channel_id = CHANNEL.to_owned();
            b.team_id = TEAM.to_owned();
        });
        assert_eq!(
            addressing_verdict(&conn(USER).0, &event, None),
            Verdict::RequiresChannelMembership(CHANNEL.to_owned())
        );
    }

    #[test]
    fn team_id_is_only_reached_when_there_is_no_channel_id() {
        let event = with_broadcast(WEBSOCKET_EVENT_PREFERENCES_CHANGED, |b| {
            b.team_id = TEAM.to_owned();
        });
        // No team memberships on the session.
        assert_eq!(
            addressing_verdict(&conn(USER).0, &event, None),
            Verdict::Skip
        );

        let (member_conn, _queue) = WebConn::new(
            CONN.to_owned(),
            Session {
                team_members: Some(vec![TeamMember {
                    team_id: TEAM.to_owned(),
                    user_id: USER.to_owned(),
                    ..Default::default()
                }]),
                ..session(USER)
            },
            false,
        );
        assert_eq!(
            addressing_verdict(&member_conn, &event, None),
            Verdict::Send
        );
    }

    #[test]
    fn typing_needs_the_channel_open_and_membership_is_not_enough() {
        let event = with_broadcast(WEBSOCKET_EVENT_TYPING, |b| {
            b.channel_id = CHANNEL.to_owned();
        });
        let (conn, _queue) = conn(USER);

        // Presence unset (`<>`): Go's `notInChannel` is false, so the extra scoping does not
        // apply and the event falls through to the membership question.
        assert_eq!(
            addressing_verdict(&conn, &event, None),
            Verdict::RequiresChannelMembership(CHANNEL.to_owned())
        );

        // A *different* channel open, and no thread indicators set. `notInChannel` is now true
        // and `notInThread` is also true — its two arms are ANDed and an unset indicator makes
        // its own arm false, so both threads unset means... both arms false, giving false.
        conn.set_presence(Presence::Channel, "0000000000000000000000000a");
        assert_eq!(
            addressing_verdict(&conn, &event, None),
            Verdict::RequiresChannelMembership(CHANNEL.to_owned()),
            "notInThread is false while both thread indicators are unset, so the typing scope \
             does not exclude on the channel alone"
        );

        // Now a thread is open on a different channel too, so both arms hold and typing is
        // withheld.
        conn.set_presence(Presence::RhsThreadChannel, "0000000000000000000000000b");
        conn.set_presence(
            Presence::ThreadViewThreadChannel,
            "0000000000000000000000000c",
        );
        assert_eq!(addressing_verdict(&conn, &event, None), Verdict::Skip);

        // Opening the event's channel in the thread view brings it back.
        conn.set_presence(Presence::ThreadViewThreadChannel, CHANNEL);
        assert_eq!(
            addressing_verdict(&conn, &event, None),
            Verdict::RequiresChannelMembership(CHANNEL.to_owned())
        );
    }

    #[test]
    fn a_posted_event_is_not_scoped_by_presence() {
        // Same shape as the typing test, but `posted` is not in the scoped set: an open channel
        // elsewhere must not withhold a post.
        let event = with_broadcast(WEBSOCKET_EVENT_POSTED, |b| {
            b.channel_id = CHANNEL.to_owned();
        });
        let (conn, _queue) = conn(USER);
        conn.set_presence(Presence::Channel, "0000000000000000000000000a");
        conn.set_presence(Presence::RhsThreadChannel, "0000000000000000000000000b");
        conn.set_presence(
            Presence::ThreadViewThreadChannel,
            "0000000000000000000000000c",
        );
        assert_eq!(
            addressing_verdict(&conn, &event, None),
            Verdict::RequiresChannelMembership(CHANNEL.to_owned())
        );
    }

    #[test]
    fn sanitized_data_goes_to_everyone_except_admins() {
        let event = with_broadcast(WEBSOCKET_EVENT_USER_UPDATED, |b| {
            b.contains_sanitized_data = true;
        });
        assert_eq!(
            addressing_verdict(&conn(USER).0, &event, Some(false)),
            Verdict::Send
        );
        assert_eq!(
            addressing_verdict(&conn(USER).0, &event, Some(true)),
            Verdict::Skip
        );
    }

    #[test]
    fn sensitive_data_goes_to_admins_only() {
        let event = with_broadcast(WEBSOCKET_EVENT_USER_UPDATED, |b| {
            b.contains_sensitive_data = true;
        });
        assert_eq!(
            addressing_verdict(&conn(USER).0, &event, Some(true)),
            Verdict::Send
        );
        assert_eq!(
            addressing_verdict(&conn(USER).0, &event, Some(false)),
            Verdict::Skip
        );
    }

    #[test]
    fn setting_both_data_flags_sends_to_nobody() {
        // Go documents this as a caller bug that reaches no one, and unit-tests it. Both values
        // of the permission, because "nobody" is the whole claim.
        let event = with_broadcast(WEBSOCKET_EVENT_USER_UPDATED, |b| {
            b.contains_sanitized_data = true;
            b.contains_sensitive_data = true;
        });
        assert_eq!(
            addressing_verdict(&conn(USER).0, &event, Some(true)),
            Verdict::Skip
        );
        assert_eq!(
            addressing_verdict(&conn(USER).0, &event, Some(false)),
            Verdict::Skip
        );
    }

    #[test]
    fn a_guest_is_scoped_on_exactly_two_events_and_each_names_its_user_its_own_way() {
        let mut updated = event(WEBSOCKET_EVENT_USER_UPDATED);
        updated.add(
            "user",
            serde_json::json!({"id": OTHER_USER, "username": "x"}),
        );
        assert_eq!(
            guest_subject(&updated),
            GuestSubject::User(OTHER_USER.to_owned())
        );

        let mut anonymous = event(WEBSOCKET_EVENT_USER_UPDATED);
        anonymous.add("user", serde_json::json!({"username": "x"}));
        assert_eq!(guest_subject(&anonymous), GuestSubject::User(String::new()));

        // Not the user object: the assertion fails, and so does `user_id` in its place.
        for not_a_user in [serde_json::json!(OTHER_USER), serde_json::json!(null)] {
            let mut odd = event(WEBSOCKET_EVENT_USER_UPDATED);
            odd.add("user", not_a_user);
            assert_eq!(guest_subject(&odd), GuestSubject::Nobody);
        }
        let mut wrong_key = event(WEBSOCKET_EVENT_USER_UPDATED);
        wrong_key.add("user_id", serde_json::json!(OTHER_USER));
        assert_eq!(guest_subject(&wrong_key), GuestSubject::Nobody);

        let mut new_user = event(WEBSOCKET_EVENT_NEW_USER);
        new_user.add("user_id", serde_json::json!(OTHER_USER));
        assert_eq!(
            guest_subject(&new_user),
            GuestSubject::User(OTHER_USER.to_owned())
        );
        let mut new_user_object = event(WEBSOCKET_EVENT_NEW_USER);
        new_user_object.add("user", serde_json::json!({"id": OTHER_USER}));
        assert_eq!(guest_subject(&new_user_object), GuestSubject::Nobody);

        assert_eq!(
            guest_subject(&event(WEBSOCKET_EVENT_POSTED)),
            GuestSubject::Anyone
        );
        assert_eq!(
            guest_subject(&event(WEBSOCKET_EVENT_PREFERENCES_CHANGED)),
            GuestSubject::Anyone
        );
    }

    #[test]
    fn reactions_are_presence_scoped_the_way_typing_is() {
        // The scoped set has three members and the tests reached only one of them, so a mutation
        // that added or dropped a *different* member could not be seen.
        let conn = conn(USER);
        conn.0
            .set_presence(Presence::Channel, "0000000000000000000000000a");
        conn.0
            .set_presence(Presence::RhsThreadChannel, "0000000000000000000000000b");
        conn.0.set_presence(
            Presence::ThreadViewThreadChannel,
            "0000000000000000000000000c",
        );

        for scoped in [
            WEBSOCKET_EVENT_TYPING,
            WEBSOCKET_EVENT_REACTION_ADDED,
            WEBSOCKET_EVENT_REACTION_REMOVED,
        ] {
            let event = with_broadcast(scoped, |b| b.channel_id = CHANNEL.to_owned());
            assert_eq!(
                addressing_verdict(&conn.0, &event, None),
                Verdict::Skip,
                "{scoped} is withheld from a connection with another channel open"
            );
        }

        for unscoped in [WEBSOCKET_EVENT_POSTED, WEBSOCKET_EVENT_STATUS_CHANGE] {
            let event = with_broadcast(unscoped, |b| b.channel_id = CHANNEL.to_owned());
            assert_eq!(
                addressing_verdict(&conn.0, &event, None),
                Verdict::RequiresChannelMembership(CHANNEL.to_owned()),
                "{unscoped} is not presence-scoped"
            );
        }
    }

    #[test]
    fn a_guest_with_an_unaddressed_event_falls_through_to_the_guest_rule() {
        let mut guest_session = session(USER);
        guest_session.add_prop("is_guest", "true");
        let (guest, _queue) = WebConn::new(CONN.to_owned(), guest_session, false);
        assert_eq!(
            addressing_verdict(&guest, &event(WEBSOCKET_EVENT_USER_UPDATED), None),
            Verdict::GuestVisibility
        );
        // A non-guest on the same event does not.
        assert_eq!(
            addressing_verdict(&conn(USER).0, &event(WEBSOCKET_EVENT_USER_UPDATED), None),
            Verdict::Send
        );
    }

    #[test]
    fn a_slow_queue_drops_only_the_three_cheap_event_types() {
        // The fill count is the literal 128, not `SEND_SLOW_WARN`. Written symbolically, a
        // mutation of the constant moves the threshold *and* the fill together and the test
        // cannot see it — which is exactly what happened on the first mutation run.
        const HALF_A_QUEUE: usize = 128;
        assert_eq!(
            SEND_SLOW_WARN, HALF_A_QUEUE,
            "Go's sendSlowWarn is half the queue"
        );

        let (conn, mut rx) = WebConn::new(CONN.to_owned(), session(USER), false);
        for _ in 0..HALF_A_QUEUE {
            conn.try_send(OutgoingFrame::Event {
                event: Box::new(event(WEBSOCKET_EVENT_POSTED)),
                precomputed: true,
            })
            .expect("queue has room");
        }
        assert!(conn.queue_is_slow());

        for dropped in [
            WEBSOCKET_EVENT_TYPING,
            WEBSOCKET_EVENT_STATUS_CHANGE,
            WEBSOCKET_EVENT_MULTIPLE_CHANNELS_VIEWED,
        ] {
            assert_eq!(
                addressing_verdict(&conn, &event(dropped), None),
                Verdict::Skip,
                "{dropped} should be dropped on a slow queue"
            );
        }
        for kept in [WEBSOCKET_EVENT_POSTED, WEBSOCKET_EVENT_REACTION_ADDED] {
            assert_eq!(
                addressing_verdict(&conn, &event(kept), None),
                Verdict::Send,
                "{kept} is never dropped for queue pressure"
            );
        }

        // Drain one below the threshold and typing comes back.
        rx.try_recv().expect("a queued frame");
        assert!(!conn.queue_is_slow());
        assert_eq!(
            addressing_verdict(&conn, &event(WEBSOCKET_EVENT_TYPING), None),
            Verdict::Send
        );
    }

    #[test]
    fn register_queues_hello_and_close_and_remove_takes_the_connection_out() {
        let hub = Hub::new();
        let (conn, mut rx) = WebConn::new(CONN.to_owned(), session(USER), false);
        hub.register(
            conn.clone(),
            event(mm_model::websocket_message::WEBSOCKET_EVENT_HELLO),
        );

        assert_eq!(hub.conn_count(), 1);
        assert_eq!(hub.conn_count_for_user(USER), 1);
        match rx.try_recv().expect("hello was queued") {
            OutgoingFrame::Event { event, precomputed } => {
                assert_eq!(event.event_type(), "hello");
                assert!(!precomputed, "hello does not take Go's precompute path");
            }
            other => panic!("expected an event, got {other:?}"),
        }

        hub.close_and_remove(&conn);
        assert_eq!(hub.conn_count(), 0);
        assert_eq!(hub.conn_count_for_user(USER), 0);
        // Not the same assertion: `conn_count_for_user` filters on `is_active`, so a connection
        // left behind in the user index is invisible to it. `for_user` is what a broadcast
        // iterates, and a stale entry there means every later publish to this user walks a dead
        // connection.
        assert!(
            hub.for_user(USER).is_empty(),
            "removal must take the connection out of the user index, not just deactivate it"
        );
        // Removing twice is a no-op, as Go's is.
        hub.close_and_remove(&conn);
        assert_eq!(hub.conn_count(), 0);
    }

    fn hello() -> WebSocketEvent {
        event(mm_model::websocket_message::WEBSOCKET_EVENT_HELLO)
    }

    fn parked(rx: mpsc::Receiver<OutgoingFrame>) -> ParkedQueues {
        ParkedQueues {
            active: rx,
            dead: DeadQueue::new(),
        }
    }

    #[test]
    fn a_parked_connection_stays_indexed_inactive_uncounted_and_keeps_receiving() {
        let hub = Hub::new();
        let (conn, rx) = WebConn::new(CONN.to_owned(), session(USER), false);
        hub.register(conn.clone(), hello());
        hub.park(&conn, parked(rx));

        assert!(!conn.is_active());
        assert_eq!(hub.conn_count(), 0, "a parked connection is not counted");
        assert_eq!(hub.conn_count_for_user(USER), 0);
        assert_eq!(hub.for_user(USER).len(), 1, "but it is still indexed");

        // A frame sent while it is parked waits in the queue.
        hub.send_message(
            &conn,
            OutgoingFrame::Response(Box::new(WebSocketResponse::new("OK", 9, None))),
        );
        let mut found = hub.check_conn(USER, CONN).expect("the parked connection");
        let mut kinds = Vec::new();
        while let Ok(frame) = found.queues.active.try_recv() {
            kinds.push(match frame {
                OutgoingFrame::Event { event, .. } => event.event_type().to_owned(),
                OutgoingFrame::Response(response) => format!("response {}", response.seq_reply),
            });
        }
        assert_eq!(kinds, ["hello", "response 9"]);
    }

    #[test]
    fn check_conn_takes_only_an_inactive_connection_of_that_user_with_that_id() {
        let hub = Hub::new();
        let (conn, rx) = WebConn::new(CONN.to_owned(), session(USER), false);
        hub.register(conn.clone(), hello());

        assert!(
            hub.check_conn(USER, CONN).is_none(),
            "an active connection's client never left"
        );
        hub.park(&conn, parked(rx));
        assert!(hub.check_conn(OTHER_USER, CONN).is_none(), "another user's");
        assert!(hub.check_conn(USER, CHANNEL).is_none(), "another id");
        assert!(hub.check_conn("", CONN).is_none(), "no user at all");

        let found = hub.check_conn(USER, CONN).expect("the match");
        assert_eq!(found.reuse_count, 1);
        assert!(hub.for_user(USER).is_empty(), "taken out of the index");
        assert!(hub.check_conn(USER, CONN).is_none(), "and so found once");
    }

    #[test]
    fn a_resumed_connection_is_registered_without_hello_and_counts_its_reuse() {
        let hub = Hub::new();
        let (conn, rx) = WebConn::new(CONN.to_owned(), session(USER), false);
        hub.register(conn.clone(), hello());
        hub.park(&conn, parked(rx));

        let found = hub.check_conn(USER, CONN).expect("parked");
        let (resumed, mut queues) = WebConn::resume(CONN.to_owned(), session(USER), false, found);
        assert!(!resumed.is_active(), "not active until registered");
        hub.register(resumed.clone(), hello());
        assert!(resumed.is_active());
        assert_eq!(resumed.reuse_count, 1);

        // The first registration's hello is still queued; the second registration added none.
        let mut hellos = 0;
        while queues.active.try_recv().is_ok() {
            hellos += 1;
        }
        assert_eq!(hellos, 1);

        // A second round counts again.
        hub.park(&resumed, queues);
        assert_eq!(hub.check_conn(USER, CONN).map(|f| f.reuse_count), Some(2));
    }

    #[test]
    fn a_full_queue_closes_and_removes_a_parked_connection() {
        let hub = Hub::new();
        let (conn, rx) = WebConn::new(CONN.to_owned(), session(USER), false);
        hub.register(conn.clone(), hello());
        hub.park(&conn, parked(rx));

        for seq in 0..=SEND_QUEUE_SIZE as i64 {
            hub.send_message(
                &conn,
                OutgoingFrame::Response(Box::new(WebSocketResponse::new("OK", seq, None))),
            );
        }
        assert!(hub.for_user(USER).is_empty());
        assert!(hub.check_conn(USER, CONN).is_none());
    }

    #[test]
    fn a_frame_for_a_connection_the_hub_does_not_hold_is_dropped() {
        let hub = Hub::new();
        let (conn, mut rx) = WebConn::new(CONN.to_owned(), session(USER), false);
        hub.send_message(
            &conn,
            OutgoingFrame::Response(Box::new(WebSocketResponse::new("OK", 1, None))),
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn the_reaper_drops_only_inactive_connections_idle_past_the_interval() {
        let hub = Hub::new();
        let now = get_millis();
        let mk = |id: &str| WebConn::new(id.to_owned(), session(USER), false);

        let (stale, stale_rx) = mk("stalestalestalestalestale1");
        let (recent, recent_rx) = mk("recentrecentrecentrecentre");
        let (active, _active_rx) = mk("activeactiveactiveactiveac");
        for c in [&stale, &recent, &active] {
            hub.register(c.clone(), hello());
        }
        hub.park(&stale, parked(stale_rx));
        hub.park(&recent, parked(recent_rx));
        stale.last_user_activity_at.store(
            now - INACTIVE_CONN_REAPER_INTERVAL - 1_000,
            Ordering::Release,
        );
        recent.last_user_activity_at.store(
            now - INACTIVE_CONN_REAPER_INTERVAL + 1_000,
            Ordering::Release,
        );
        active
            .last_user_activity_at
            .store(now - 10 * INACTIVE_CONN_REAPER_INTERVAL, Ordering::Release);

        // Not due yet: nothing is reaped.
        assert!(hub.check_conn(USER, NOBODY_CONN).is_none());
        assert_eq!(hub.for_user(USER).len(), 3);

        hub.last_reap
            .store(now - INACTIVE_CONN_REAPER_INTERVAL, Ordering::Release);
        assert!(hub.check_conn(USER, NOBODY_CONN).is_none());
        let left: Vec<String> = hub
            .for_user(USER)
            .iter()
            .map(|c| c.connection_id())
            .collect();
        assert_eq!(left.len(), 2, "{left:?}");
        assert!(!left.contains(&stale.connection_id()));
    }

    #[test]
    fn update_activity_touches_only_active_connections_holding_that_token() {
        let hub = Hub::new();
        let (a, _a_rx) = WebConn::new(CONN.to_owned(), session(USER), false);
        let (b, b_rx) = WebConn::new(CHANNEL.to_owned(), session(USER), false);
        let mut other = session(USER);
        other.token = "another".to_owned();
        let (c, _c_rx) = WebConn::new(TEAM.to_owned(), other, false);
        for conn in [&a, &b, &c] {
            hub.register(conn.clone(), hello());
            conn.last_user_activity_at.store(1, Ordering::Release);
        }
        hub.park(&b, parked(b_rx));

        hub.update_activity(USER, "token", 42);
        assert_eq!(a.last_user_activity_at(), 42);
        assert_eq!(b.last_user_activity_at(), 1, "inactive");
        assert_eq!(c.last_user_activity_at(), 1, "another token");
    }

    // -----------------------------------------------------------------------------------------
    // the dead queue
    // -----------------------------------------------------------------------------------------

    const NOBODY_CONN: &str = "nobodynobodynobodynobodyno";

    fn entry(seq: i64) -> DeadQueueEntry {
        DeadQueueEntry {
            event: event(WEBSOCKET_EVENT_POSTED).set_sequence(seq),
            precomputed: seq % 2 == 0,
        }
    }

    fn filled(seqs: std::ops::Range<i64>) -> DeadQueue {
        let mut queue = DeadQueue::new();
        for seq in seqs {
            queue.add(entry(seq));
        }
        queue
    }

    fn seqs(entries: Vec<&DeadQueueEntry>) -> Vec<i64> {
        entries.iter().map(|e| e.event.get_sequence()).collect()
    }

    #[test]
    fn an_empty_dead_queue_has_lost_nothing_and_holds_nothing() {
        let queue = DeadQueue::new();
        assert!(!queue.has_msg_loss(5));
        assert!(!queue.has_msg_loss(0));
        assert_eq!(queue.position_of(0), None);
        assert!(queue.drain_from(0).is_empty());
    }

    #[test]
    fn loss_is_judged_against_the_newest_entry_only() {
        let queue = filled(0..4);
        assert!(!queue.has_msg_loss(4), "the next one after the newest");
        assert!(queue.has_msg_loss(3), "the newest itself");
        assert!(queue.has_msg_loss(2));
        assert!(queue.has_msg_loss(40));
    }

    #[test]
    fn a_queue_whose_pointer_wrapped_to_zero_judges_loss_by_its_last_slot() {
        let queue = filled(0..DEAD_QUEUE_SIZE as i64);
        assert_eq!(queue.pointer, 0);
        assert!(!queue.has_msg_loss(DEAD_QUEUE_SIZE as i64));
        assert!(queue.has_msg_loss(DEAD_QUEUE_SIZE as i64 + 1));
    }

    #[test]
    fn position_of_finds_a_slot_and_stops_at_the_first_gap() {
        let queue = filled(5..8);
        assert_eq!(queue.position_of(6), Some(1));
        assert_eq!(queue.position_of(8), None);
        assert_eq!(queue.position_of(4), None);

        let wrapped = filled(0..130);
        assert_eq!(wrapped.position_of(128), Some(0));
        assert_eq!(wrapped.position_of(2), Some(2));
        assert_eq!(wrapped.position_of(1), None, "overwritten");
    }

    #[test]
    fn a_drain_before_the_wrap_runs_from_the_index_to_the_pointer() {
        let queue = filled(0..5);
        assert_eq!(seqs(queue.drain_from(2)), [2, 3, 4]);
        assert_eq!(seqs(queue.drain_from(0)), [0, 1, 2, 3, 4]);
        let kept: Vec<bool> = queue.drain_from(3).iter().map(|e| e.precomputed).collect();
        assert_eq!(
            kept,
            [false, true],
            "each frame keeps the encoding it left in"
        );
    }

    #[test]
    fn a_drain_after_the_wrap_runs_through_the_end_to_the_newest() {
        let wrapped = filled(0..130); // slots 0 and 1 hold 128 and 129; slot 2 the oldest, 2
        let from_126 = wrapped.position_of(126).expect("present");
        assert_eq!(seqs(wrapped.drain_from(from_126)), [126, 127, 128, 129]);
        assert_eq!(seqs(wrapped.drain_from(0)), [128, 129]);
        let everything = seqs(wrapped.drain_from(2));
        assert_eq!(everything.len(), DEAD_QUEUE_SIZE);
        assert_eq!(everything.first(), Some(&2));
        assert_eq!(everything.last(), Some(&129));

        // Exactly full: the pointer is back at 0 and the stop is the wrap to slot 0.
        let full = filled(0..DEAD_QUEUE_SIZE as i64);
        assert_eq!(seqs(full.drain_from(125)), [125, 126, 127]);
    }

    #[test]
    fn clear_empties_the_queue_and_resets_the_pointer() {
        let mut queue = filled(0..3);
        queue.clear();
        assert_eq!(queue.pointer, 0);
        assert!(queue.slots.iter().all(Option::is_none));
        assert!(!queue.has_msg_loss(7));
        queue.add(entry(0));
        assert_eq!(queue.position_of(0), Some(0));
    }

    // -----------------------------------------------------------------------------------------
    // the broadcast-hook runner
    // -----------------------------------------------------------------------------------------

    use crate::broadcast_hooks::{
        BROADCAST_ADD_MENTIONS, BROADCAST_POSTED_ACK, BroadcastHookError,
    };
    use serde_json::json;

    const POSTER: &str = "p0sterp0sterp0sterp0sterp0";

    /// A suite that resolves nothing — the runner tests exercise the pure hooks only.
    pub(crate) struct DenyAllSuite;

    impl BroadcastHookSuite for DenyAllSuite {
        fn has_permission_to_resolve_channel_mention<'a>(
            &'a self,
            _user_id: &'a str,
            _channel_id: &'a str,
        ) -> HookFuture<'a, bool> {
            Box::pin(async { false })
        }
    }

    fn posted_event() -> WebSocketEvent {
        let mut event = WebSocketEvent::new(WEBSOCKET_EVENT_POSTED, "", CHANNEL, "", None, "");
        event.add("post", json!("{}"));
        event
    }

    fn map(value: serde_json::Value) -> StringInterface {
        match value {
            serde_json::Value::Object(map) => map,
            other => panic!("not an object: {other}"),
        }
    }

    /// `add_mentions` naming `USER`, then `posted_ack` for an open channel that lists nobody: the
    /// second hook acks only because the first one wrote `mentions`.
    fn mentions_then_ack() -> (Vec<String>, Vec<StringInterface>) {
        (
            vec![
                BROADCAST_ADD_MENTIONS.to_owned(),
                BROADCAST_POSTED_ACK.to_owned(),
            ],
            vec![
                map(json!({ "mentions": [USER] })),
                map(json!({ "posted_user_id": POSTER, "channel_type": "O", "users": [] })),
            ],
        )
    }

    #[tokio::test]
    async fn hooks_run_in_the_order_attached_and_posted_ack_sees_what_add_mentions_wrote() {
        let hub = Hub::new();
        let (conn, _rx) = WebConn::new(CONN.to_owned(), session(USER), true);
        let event = posted_event();

        let (ids, args) = mentions_then_ack();
        let out = hub
            .run_broadcast_hooks(&event, &conn, &ids, &args, &DenyAllSuite)
            .await
            .expect("add_mentions modified the event");
        let data = out.get_data().unwrap();
        assert_eq!(data["mentions"], json!(format!("[\"{USER}\"]")));
        assert_eq!(data["should_ack"], json!(true), "{data:?}");

        // Reversed, `posted_ack` runs first, finds no `mentions`, and — open channel, nobody
        // listed — adds nothing. `add_mentions` still writes its key afterwards.
        let (mut ids, mut args) = mentions_then_ack();
        ids.reverse();
        args.reverse();
        let out = hub
            .run_broadcast_hooks(&event, &conn, &ids, &args, &DenyAllSuite)
            .await
            .expect("add_mentions still modifies the event");
        let data = out.get_data().unwrap();
        assert_eq!(data["mentions"], json!(format!("[\"{USER}\"]")));
        assert!(
            data.get("should_ack").is_none(),
            "posted_ack before add_mentions cannot see the mention: {data:?}"
        );
    }

    #[tokio::test]
    async fn a_connection_the_hooks_do_not_touch_gets_no_copy() {
        let hub = Hub::new();
        // Not mentioned, and no ack flag.
        let (conn, _rx) = WebConn::new(CONN.to_owned(), session(OTHER_USER), false);
        let (ids, args) = mentions_then_ack();
        assert!(
            hub.run_broadcast_hooks(&posted_event(), &conn, &ids, &args, &DenyAllSuite)
                .await
                .is_none()
        );
        // No hooks at all: the early return.
        assert!(
            hub.run_broadcast_hooks(&posted_event(), &conn, &[], &[], &DenyAllSuite)
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn an_unknown_hook_id_is_skipped_and_the_rest_still_run() {
        let hub = Hub::new();
        let (conn, _rx) = WebConn::new(CONN.to_owned(), session(USER), false);
        let event = posted_event();

        // Alone: nothing happens, no copy.
        assert!(
            hub.run_broadcast_hooks(
                &event,
                &conn,
                &["no_such_hook".to_owned()],
                &[StringInterface::new()],
                &DenyAllSuite
            )
            .await
            .is_none()
        );

        // Ahead of a known hook: the known hook still runs, with *its own* args — the pairing
        // is by index, so a skipped id must not shift the args of the one after it.
        let out = hub
            .run_broadcast_hooks(
                &event,
                &conn,
                &["no_such_hook".to_owned(), BROADCAST_ADD_MENTIONS.to_owned()],
                &[
                    map(json!({ "mentions": [OTHER_USER] })),
                    map(json!({ "mentions": [USER] })),
                ],
                &DenyAllSuite,
            )
            .await
            .expect("add_mentions ran");
        assert_eq!(
            out.get_data().unwrap()["mentions"],
            json!(format!("[\"{USER}\"]"))
        );
    }

    #[tokio::test]
    async fn a_failing_hook_is_logged_and_the_rest_still_run() {
        let hub = Hub::new();
        let (conn, _rx) = WebConn::new(CONN.to_owned(), session(USER), false);
        // `add_mentions` with no args fails; the `add_followers` after it runs.
        let out = hub
            .run_broadcast_hooks(
                &posted_event(),
                &conn,
                &[
                    BROADCAST_ADD_MENTIONS.to_owned(),
                    crate::broadcast_hooks::BROADCAST_ADD_FOLLOWERS.to_owned(),
                ],
                &[StringInterface::new(), map(json!({ "followers": [USER] }))],
                &DenyAllSuite,
            )
            .await
            .expect("add_followers ran");
        let data = out.get_data().unwrap();
        assert!(data.get("mentions").is_none());
        assert_eq!(data["followers"], json!(format!("[\"{USER}\"]")));

        // Fewer args than ids: the hook sees an empty map, not a panic.
        assert!(
            hub.run_broadcast_hooks(
                &posted_event(),
                &conn,
                &[BROADCAST_ADD_MENTIONS.to_owned()],
                &[],
                &DenyAllSuite
            )
            .await
            .is_none()
        );
    }

    #[test]
    fn a_hooked_copy_leaves_unprecomputed_and_an_untouched_event_precomputed() {
        let event = posted_event();

        match broadcast_frame(&event, None) {
            OutgoingFrame::Event {
                event: sent,
                precomputed,
            } => {
                assert!(precomputed, "the shared event takes Go's precompute path");
                assert_eq!(*sent, event);
            }
            other => panic!("{other:?}"),
        }

        let mut modified = event.deep_copy_like_go();
        modified.add("should_ack", json!(true));
        match broadcast_frame(&event, Some(modified.clone())) {
            OutgoingFrame::Event {
                event: sent,
                precomputed,
            } => {
                assert!(
                    !precomputed,
                    "a copy has had its precomputed JSON removed and goes through json.Encoder"
                );
                assert_eq!(*sent, modified);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_hooked_event_copies_on_first_write_and_reads_through_to_the_copy() {
        let mut original = posted_event();
        original.add("mentions", json!(null));
        let mut hooked = HookedWebSocketEvent::new(&original);

        assert_eq!(hooked.event_type(), WEBSOCKET_EVENT_POSTED);
        assert_eq!(hooked.get("post"), Some(&json!("{}")));
        // A JSON null reads as absent, as Go's `!= nil` would have it.
        assert!(hooked.get("mentions").is_none());
        assert!(hooked.get("absent").is_none());

        hooked.add("should_ack", json!(true));
        assert_eq!(hooked.get("should_ack"), Some(&json!(true)));
        // The original is untouched — it is the frame every other connection gets.
        assert!(original.get_data().unwrap().get("should_ack").is_none());

        let copy = hooked.into_copy().expect("a write made a copy");
        assert_eq!(copy.get_data().unwrap()["should_ack"], json!(true));
        assert_eq!(copy.get_data().unwrap()["post"], json!("{}"));
    }

    #[tokio::test]
    async fn a_hub_without_a_registry_knows_no_hook() {
        // `with_hooks` is what the tests above bypass; make sure the registry is the only source.
        let hub = Hub::with_hooks(HashMap::new());
        let (conn, _rx) = WebConn::new(CONN.to_owned(), session(USER), true);
        let (ids, args) = mentions_then_ack();
        assert!(
            hub.run_broadcast_hooks(&posted_event(), &conn, &ids, &args, &DenyAllSuite)
                .await
                .is_none()
        );
        // And the error type is nameable from here, which is all the trait needs of it.
        let _: Option<BroadcastHookError> = None;
    }
}
