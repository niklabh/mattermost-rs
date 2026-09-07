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
//!   full.
//!
//! Not ported, each for a stated reason:
//!
//! - **The dead queue and reconnect replay** (`web_conn.go:665-772`, `PopulateWebConnConfig`).
//!   A reconnecting client with a known `connection_id` and `sequence_number` gets a fresh
//!   connection here instead of the frames it missed. See [D-181].
//! - **Cluster send.** `Publish` mirrors an event to other nodes through `clusterIFace`
//!   (`cluster.go:189`); there is one node, and the strangler's *other* process is the Go server,
//!   which has its own hub. See [D-182] — a client connected to this server does not see events
//!   raised by a route still served by Go.
//! - **Broadcast hooks** (`web_broadcast_hook.go`). They rewrite an event per connection — the
//!   only stock hook adds the recipient's own mention count to a `posted` event. Modelled in
//!   `mm-model` (`without_broadcast_hooks`) and stripped before send here, but not run: see
//!   [D-183].
//! - **The MFA arm of `IsAuthenticated`.** `MFARequired` is not ported, so a connection whose user
//!   owes MFA is treated as authenticated. See [D-184].
//! - **`ShouldSendEventToGuest`.** Needs `UserCanSeeOtherUser`, not ported; guests therefore see
//!   `user_updated` and `new_user` for users Go would hide from them. See [D-185].
//!
//! # Where the sequence number is assigned
//!
//! Nowhere in this file. Go assigns `seq` inside `writePump` as it pops the queue
//! (`web_conn.go:583`), so the numbering is per connection and follows *delivery* order, not
//! broadcast order. `mm-api`'s write pump does the same. Assigning it here would be wrong the
//! moment two hubs' broadcasts interleave on one connection.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};

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
    pub connection_id: String,
    pub user_id: String,

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
}

impl WebConn {
    /// Port of `PlatformService.NewWebConn` (web_conn.go:200), minus the TCP_NODELAY tweak and the
    /// plugin connect hook.
    ///
    /// Returns the connection and the receiving half of its queue; `mm-api`'s write pump owns the
    /// receiver, so a dropped receiver is how the hub learns the socket is gone.
    pub fn new(
        connection_id: String,
        session: Session,
    ) -> (Arc<WebConn>, mpsc::Receiver<OutgoingFrame>) {
        let (tx, rx) = mpsc::channel(SEND_QUEUE_SIZE);
        let conn = Arc::new(WebConn {
            connection_id,
            user_id: session.user_id.clone(),
            session: RwLock::new(session),
            active: AtomicBool::new(true),
            send: tx,
            active_channel_id: RwLock::new(UNSET_PRESENCE_INDICATOR.to_owned()),
            active_rhs_thread_channel_id: RwLock::new(UNSET_PRESENCE_INDICATOR.to_owned()),
            active_thread_view_thread_channel_id: RwLock::new(UNSET_PRESENCE_INDICATOR.to_owned()),
            all_channel_members: RwLock::new(None),
        });
        (conn, rx)
    }

    /// A snapshot of the session. Cloned rather than borrowed so no caller holds the lock across
    /// an `.await`.
    pub fn session(&self) -> Session {
        self.session
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
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

    /// Port of `(*WebConn).InvalidateCache` (web_conn.go:774), membership half only.
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

    fn set_active(&self, value: bool) {
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
#[derive(Debug, Default)]
pub struct Hub {
    index: RwLock<HubIndex>,
}

impl Hub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Port of `Hub.Register` (web_hub.go:378) plus the register arm of the hub loop
    /// (web_hub.go:588).
    ///
    /// Sends `hello` as the connection's first frame. Go gates that on `reuseCount == 0`; every
    /// registration here is fresh, because reconnect replay is not ported ([D-181]), so the gate
    /// is always open and is not reproduced as a branch that can only take one value.
    pub fn register(&self, conn: Arc<WebConn>, hello: WebSocketEvent) {
        conn.set_active(true);
        {
            let mut index = self
                .index
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            index
                .by_conn
                .insert(conn.connection_id.clone(), conn.clone());
            index
                .by_user
                .entry(conn.user_id.clone())
                .or_default()
                .push(conn.clone());
        }
        // A queue that is full at registration cannot happen — it was just created — so the
        // error arm is unreachable rather than ignored.
        // `hello` does *not* go through `PrecomputeJSON` — it is queued directly by the hub loop
        // (web_hub.go:604), so it leaves compact and newline-terminated.
        let _ = conn.try_send(OutgoingFrame::Event {
            event: Box::new(hello),
            precomputed: false,
        });
    }

    /// Port of `Hub.Unregister` (web_hub.go:392) and `closeAndRemoveConn` (web_hub.go:825).
    pub fn unregister(&self, connection_id: &str) {
        let mut index = self
            .index
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(conn) = index.by_conn.remove(connection_id) else {
            return;
        };
        conn.set_active(false);
        if let Some(conns) = index.by_user.get_mut(&conn.user_id) {
            conns.retain(|c| c.connection_id != connection_id);
            if conns.is_empty() {
                index.by_user.remove(&conn.user_id);
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

    /// Total live connections. Not a Go method; used by tests and the hub's own logging.
    pub fn conn_count(&self) -> usize {
        self.index
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .by_conn
            .len()
    }

    /// Port of `PlatformService.InvalidateCacheForUser` (web_hub.go:233), membership half.
    pub fn invalidate_channel_members_for_user(&self, user_id: &str) {
        for conn in self.for_user(user_id) {
            conn.invalidate_channel_members();
        }
    }

    /// Send one frame to one connection. Port of `Hub.SendMessage` (web_hub.go:492).
    pub fn send_to_connection(&self, connection_id: &str, frame: OutgoingFrame) {
        if let Some(conn) = self.for_connection(connection_id)
            && conn.try_send(frame).is_err()
        {
            self.unregister(connection_id);
        }
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
        // Go strips the hook fields before precomputing the JSON so they never reach a client.
        // The hooks themselves are not run — see [D-183] — but the stripping is not optional:
        // `broadcast_hooks` on the wire would be a field Go never sends.
        let (event, _hooks, _hook_args) = event.without_broadcast_hooks();

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
            let frame = OutgoingFrame::Event {
                event: Box::new(event.clone()),
                // `Hub.Broadcast` precomputes before fanning out (web_hub.go:718).
                precomputed: true,
            };
            if conn.try_send(frame).is_err() {
                if conn.is_active() {
                    tracing::error!(
                        user_id = %conn.user_id,
                        conn_id = %conn.connection_id,
                        "webhub.broadcast: cannot send, closing websocket for user"
                    );
                }
                self.hub().unregister(&conn.connection_id);
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
            Verdict::GuestVisibility => guest_visibility(event.event_type()),
            Verdict::RequiresChannelMembership(channel_id) => {
                let members = match conn.cached_channel_members() {
                    Some(members) => members,
                    None => match self
                        .store()
                        .channel()
                        .get_all_channel_members_for_user(&conn.user_id, false)
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

    /// Port of `(*WebConn).IsAuthenticated` (web_conn.go:824), basic half.
    ///
    /// Go re-fetches the session by token once it has expired, and gives up — clearing the
    /// connection's session — if the fetch fails. The MFA half is not ported ([D-184]).
    async fn conn_is_authenticated(&self, conn: &WebConn) -> bool {
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
        let mut hello = WebSocketEvent::new(WEBSOCKET_EVENT_HELLO, "", "", &conn.user_id, None, "");
        hello.add(
            "server_version",
            serde_json::Value::String(self.server_version_string()),
        );
        hello.add(
            "connection_id",
            serde_json::Value::String(conn.connection_id.clone()),
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
        return verdict(conn.connection_id == broadcast.connection_id);
    }

    if conn.connection_id == broadcast.omit_connection_id {
        return Verdict::Skip;
    }

    if !broadcast.user_id.is_empty() {
        return verdict(conn.user_id == broadcast.user_id);
    }

    if let Some(omit_users) = &broadcast.omit_users
        && !omit_users.is_empty()
        // Go tests key *presence*, not the bool's value: `omit_users: {u: false}` still omits.
        && omit_users.contains_key(&conn.user_id)
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

/// Port of `(*WebConn).ShouldSendEventToGuest` (web_conn.go:852) — **the default arm only**.
///
/// The two interesting arms need `UserCanSeeOtherUser`, which is not ported, so a guest here
/// receives `user_updated` and `new_user` for users Go would have hidden. Recorded as [D-185]
/// rather than approximated: guessing at the visibility rule would be worse than a stated gap.
///
/// A free function rather than a method: it needs nothing from `App`, and as a method the
/// mutation that made it `true` for every event survived the suite, because no test could reach
/// it without a database.
pub fn guest_visibility(event_type: &str) -> bool {
    !matches!(
        event_type,
        WEBSOCKET_EVENT_USER_UPDATED | WEBSOCKET_EVENT_NEW_USER
    )
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
        WebConn::new(CONN.to_owned(), session(user_id))
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
    fn the_guest_rule_hides_exactly_two_event_types() {
        // The rule itself, separately from the fall-through that reaches it. As a method on `App`
        // this was unreachable from a unit test and a mutation replacing the whole body with
        // `true` survived.
        assert!(!guest_visibility(WEBSOCKET_EVENT_USER_UPDATED));
        assert!(!guest_visibility(
            mm_model::websocket_message::WEBSOCKET_EVENT_NEW_USER
        ));
        assert!(guest_visibility(WEBSOCKET_EVENT_POSTED));
        assert!(guest_visibility(WEBSOCKET_EVENT_PREFERENCES_CHANGED));
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
        let (guest, _queue) = WebConn::new(CONN.to_owned(), guest_session);
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

        let (conn, mut rx) = WebConn::new(CONN.to_owned(), session(USER));
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
    fn register_queues_hello_and_unregister_removes_the_connection() {
        let hub = Hub::new();
        let (conn, mut rx) = WebConn::new(CONN.to_owned(), session(USER));
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

        hub.unregister(CONN);
        assert_eq!(hub.conn_count(), 0);
        assert_eq!(hub.conn_count_for_user(USER), 0);
        // Not the same assertion: `conn_count_for_user` filters on `is_active`, so a connection
        // left behind in the user index is invisible to it. `for_user` is what a broadcast
        // iterates, and a stale entry there means every later publish to this user walks a dead
        // connection.
        assert!(
            hub.for_user(USER).is_empty(),
            "unregister must remove the connection from the user index, not just deactivate it"
        );
        // Unregistering twice is a no-op, as Go's is.
        hub.unregister(CONN);
        assert_eq!(hub.conn_count(), 0);
    }
}
