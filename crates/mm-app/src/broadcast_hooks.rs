//! Port of `channels/app/web_broadcast_hooks.go` — the hooks the hub runs **per connection** on
//! the way out of a broadcast.
//!
//! A hook is attached to an event by the code that raises it (`WebsocketBroadcast::add_hook`,
//! Go's `useXxxHook` helpers) and run by [`crate::hub::Hub::run_broadcast_hooks`] for each
//! connection the event reaches, in the order attached. The runner is in `hub.rs` because Go keeps
//! it in `platform`; the hooks are here because Go keeps them in `app`.
//!
//! # Which hooks exist here
//!
//! Go registers nine (`makeBroadcastHooks`, web_broadcast_hooks.go:31). Four are ported — the
//! three `SendNotifications` attaches to every `posted` event, and the one
//! `publishWebsocketEventForPost` attaches when the post mentions a channel:
//!
//! | id | args (JSON types, as `add_hook` must supply them) | effect on a connection |
//! |---|---|---|
//! | [`BROADCAST_ADD_MENTIONS`] | `mentions`: array of user ids | user in the list → `data.mentions = "[\"<user>\"]"` (a **stringified** array) |
//! | [`BROADCAST_ADD_FOLLOWERS`] | `followers`: array of user ids | same, key `followers` |
//! | [`BROADCAST_POSTED_ACK`] | `posted_user_id`: string, `channel_type`: string, `users`: array of user ids | `data.should_ack = true` for a `?posted_ack=true` connection that is not the poster's, when the frame already carries `mentions`/`followers`, or the channel is a DM, or the user is in `users` |
//! | [`BROADCAST_CHANNEL_MENTIONS`] | `channel_mentions`: object, name → `{display_name, team_name, id}` | re-decodes `data.post`, puts back under `props.channel_mentions` only the entries whose `id` the recipient may resolve, and re-encodes |
//!
//! `posted_ack` reads what `add_mentions` and `add_followers` wrote, so **attach it after them**
//! — Go's own comment says this "works since we currently do have an order for broadcast hooks".
//! `channel_mentions` re-encodes the post, so a hooked recipient's `post` string is a fresh
//! marshal rather than the precomputed one — the same bytes, since both are `Post.ToJSON`.
//!
//! The other five — `permalink`, `burn_on_read`, `burn_on_read_reaction`, `abac_files`,
//! `only_channel_admins` — are not registered. An event carrying one of their ids reaches the
//! runner, which logs Go's `Unable to find broadcast hook` warning and skips it, so the frame
//! leaves unmodified and precomputed. Their ids are declared below so a raiser can attach them
//! today; `channel_join_request` already attaches `only_channel_admins`. See [D-183].
//!
//! # `getTypedArg` in a single process
//!
//! Go re-marshals an argument through JSON when its runtime type is not the one the hook wants,
//! because in a cluster the args arrive JSON-decoded and untyped. Here they are always
//! `serde_json::Value`, so [`get_typed_arg`] is only the decode half — and a `null` decodes the
//! way Go's `json.Unmarshal` decodes it into a slice or string: to the empty value, not an error.

use std::collections::HashMap;

use mm_model::channel::CHANNEL_TYPE_DIRECT;
use mm_model::post::{POST_PROPS_CHANNEL_MENTIONS, Post};
use mm_model::utils::{StringInterface, array_to_json};
use serde::de::DeserializeOwned;

use crate::hub::{BroadcastHook, BroadcastHookSuite, HookFuture, HookedWebSocketEvent, WebConn};

/// `broadcastAddMentions` (web_broadcast_hooks.go:20).
pub const BROADCAST_ADD_MENTIONS: &str = "add_mentions";
/// `broadcastAddFollowers` (web_broadcast_hooks.go:21).
pub const BROADCAST_ADD_FOLLOWERS: &str = "add_followers";
/// `broadcastPostedAck` (web_broadcast_hooks.go:22).
pub const BROADCAST_POSTED_ACK: &str = "posted_ack";
/// `broadcastPermalink` (web_broadcast_hooks.go:23). Declared, not registered.
pub const BROADCAST_PERMALINK: &str = "permalink";
/// `broadcastChannelMentions` (web_broadcast_hooks.go:24).
pub const BROADCAST_CHANNEL_MENTIONS: &str = "channel_mentions";
/// `broadcastBurnOnRead` (web_broadcast_hooks.go:25). Declared, not registered.
pub const BROADCAST_BURN_ON_READ: &str = "burn_on_read";
/// `broadcastBurnOnReadReaction` (web_broadcast_hooks.go:26). Declared, not registered.
pub const BROADCAST_BURN_ON_READ_REACTION: &str = "burn_on_read_reaction";
/// `broadcastAbacFiles` (web_broadcast_hooks.go:27). Declared, not registered.
pub const BROADCAST_ABAC_FILES: &str = "abac_files";
/// `broadcastOnlyChannelAdmins` (web_broadcast_hooks.go:28). Declared, not registered.
pub const BROADCAST_ONLY_CHANNEL_ADMINS: &str = "only_channel_admins";

/// Port of `Server.makeBroadcastHooks` (web_broadcast_hooks.go:31), reduced to the four hooks
/// that are ported. The map is what `hubStart` hands each hub (web_hub.go:124).
pub fn make_broadcast_hooks() -> HashMap<&'static str, Box<dyn BroadcastHook>> {
    let mut hooks: HashMap<&'static str, Box<dyn BroadcastHook>> = HashMap::new();
    hooks.insert(BROADCAST_ADD_MENTIONS, Box::new(AddMentionsBroadcastHook));
    hooks.insert(BROADCAST_ADD_FOLLOWERS, Box::new(AddFollowersBroadcastHook));
    hooks.insert(BROADCAST_POSTED_ACK, Box::new(PostedAckBroadcastHook));
    hooks.insert(
        BROADCAST_CHANNEL_MENTIONS,
        Box::new(ChannelMentionsBroadcastHook),
    );
    hooks
}

/// What a hook returns to the runner, which logs it as Go does (`Error processing hook`) and
/// carries on with the next hook. Nothing a hook can fail on stops the broadcast.
#[derive(Debug, thiserror::Error)]
pub enum BroadcastHookError {
    /// Go's `errors.Wrap(err, "Invalid <key> value passed to <hook>")` around a `getTypedArg`
    /// failure.
    #[error("Invalid {key} value passed to {hook}: {source}")]
    InvalidArg {
        hook: &'static str,
        key: &'static str,
        #[source]
        source: ArgError,
    },
    /// `getPostFromMessage` (web_broadcast_hooks.go:551): the frame carries no `post`, or one
    /// that is not a string, or one that does not decode.
    #[error("{hook} failed to get post from message: {source}")]
    PostFromMessage {
        hook: &'static str,
        #[source]
        source: PostFromMessageError,
    },
    /// `post.ToJSON()` failed after the hook rewrote the post.
    #[error("Failed to marshal post in {hook}: {source}")]
    PostToJson {
        hook: &'static str,
        #[source]
        source: serde_json::Error,
    },
}

/// The three ways `getPostFromMessage` fails, in Go's words.
#[derive(Debug, thiserror::Error)]
pub enum PostFromMessageError {
    #[error("No post found in message")]
    Missing,
    #[error("Invalid post type in message")]
    NotAString,
    #[error("Failed to unmarshal post: {0}")]
    Decode(#[source] serde_json::Error),
}

/// Port of `getPostFromMessage` (web_broadcast_hooks.go:551): the post **as the hooks so far
/// have left it**, decoded from the `post` string in the event data.
fn get_post_from_message(msg: &HookedWebSocketEvent<'_>) -> Result<Post, PostFromMessageError> {
    let current = msg.get("post").ok_or(PostFromMessageError::Missing)?;
    let json = current.as_str().ok_or(PostFromMessageError::NotAString)?;
    serde_json::from_str::<Post>(json).map_err(PostFromMessageError::Decode)
}

/// The two ways `getTypedArg` (web_broadcast_hooks.go:566) fails.
#[derive(Debug, thiserror::Error)]
pub enum ArgError {
    #[error("No argument found with key: {0}")]
    Missing(String),
    /// Go's `json.Unmarshal` into the wanted type refused the value.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Port of `getTypedArg[T]` (web_broadcast_hooks.go:566): the argument under `key`, decoded as
/// `T`. See the module docs for why this is only the decode half of Go's function.
pub fn get_typed_arg<T: DeserializeOwned>(
    args: &StringInterface,
    key: &str,
) -> Result<T, ArgError> {
    let untyped = args
        .get(key)
        .ok_or_else(|| ArgError::Missing(key.to_owned()))?;
    Ok(T::deserialize(untyped)?)
}

/// `getTypedArg[model.StringArray]`. A JSON `null` is Go's nil slice — empty, not an error.
fn string_array_arg(args: &StringInterface, key: &str) -> Result<Vec<String>, ArgError> {
    get_typed_arg::<Option<Vec<String>>>(args, key).map(Option::unwrap_or_default)
}

/// `getTypedArg[string]` (and `[model.ChannelType]`, which is a string). A JSON `null` is `""`.
fn string_arg(args: &StringInterface, key: &str) -> Result<String, ArgError> {
    get_typed_arg::<Option<String>>(args, key).map(Option::unwrap_or_default)
}

/// Port of `addMentionsBroadcastHook` (web_broadcast_hooks.go:46).
#[derive(Debug)]
struct AddMentionsBroadcastHook;

impl BroadcastHook for AddMentionsBroadcastHook {
    fn process<'a, 'e>(
        &'a self,
        msg: &'a mut HookedWebSocketEvent<'e>,
        conn: &'a WebConn,
        args: &'a StringInterface,
        _suite: &'a dyn BroadcastHookSuite,
    ) -> HookFuture<'a, Result<(), BroadcastHookError>>
    where
        'e: 'a,
    {
        Box::pin(std::future::ready(Self::process_sync(msg, conn, args)))
    }
}

impl AddMentionsBroadcastHook {
    fn process_sync(
        msg: &mut HookedWebSocketEvent<'_>,
        conn: &WebConn,
        args: &StringInterface,
    ) -> Result<(), BroadcastHookError> {
        let mentions = string_array_arg(args, "mentions").map_err(|source| {
            BroadcastHookError::InvalidArg {
                hook: "addMentionsBroadcastHook",
                key: "mentions",
                source,
            }
        })?;

        if !mentions.is_empty() && mentions.contains(&conn.user_id()) {
            // Note that the client expects this field to be stringified
            msg.add(
                "mentions",
                serde_json::Value::String(array_to_json(Some(std::slice::from_ref(
                    &conn.user_id(),
                )))),
            );
        }

        Ok(())
    }
}

/// Port of `addFollowersBroadcastHook` (web_broadcast_hooks.go:70).
#[derive(Debug)]
struct AddFollowersBroadcastHook;

impl BroadcastHook for AddFollowersBroadcastHook {
    fn process<'a, 'e>(
        &'a self,
        msg: &'a mut HookedWebSocketEvent<'e>,
        conn: &'a WebConn,
        args: &'a StringInterface,
        _suite: &'a dyn BroadcastHookSuite,
    ) -> HookFuture<'a, Result<(), BroadcastHookError>>
    where
        'e: 'a,
    {
        Box::pin(std::future::ready(Self::process_sync(msg, conn, args)))
    }
}

impl AddFollowersBroadcastHook {
    fn process_sync(
        msg: &mut HookedWebSocketEvent<'_>,
        conn: &WebConn,
        args: &StringInterface,
    ) -> Result<(), BroadcastHookError> {
        let followers = string_array_arg(args, "followers").map_err(|source| {
            BroadcastHookError::InvalidArg {
                hook: "addFollowersBroadcastHook",
                key: "followers",
                source,
            }
        })?;

        if !followers.is_empty() && followers.contains(&conn.user_id()) {
            // Note that the client expects this field to be stringified
            msg.add(
                "followers",
                serde_json::Value::String(array_to_json(Some(std::slice::from_ref(
                    &conn.user_id(),
                )))),
            );
        }

        Ok(())
    }
}

/// Port of `postedAckBroadcastHook` (web_broadcast_hooks.go:94).
///
/// Three things about its shape that a reader could get wrong:
///
/// - the connection test (`PostedAck && Active`) comes **before** any argument is read, so a
///   connection without the flag never reports a malformed argument;
/// - the poster's own connections are excluded by `posted_user_id`, which is why the caller must
///   pass the post's author and not the session user;
/// - the `mentions`/`followers` test reads the event **as the earlier hooks left it** — attach
///   this hook last.
#[derive(Debug)]
struct PostedAckBroadcastHook;

impl BroadcastHook for PostedAckBroadcastHook {
    fn process<'a, 'e>(
        &'a self,
        msg: &'a mut HookedWebSocketEvent<'e>,
        conn: &'a WebConn,
        args: &'a StringInterface,
        _suite: &'a dyn BroadcastHookSuite,
    ) -> HookFuture<'a, Result<(), BroadcastHookError>>
    where
        'e: 'a,
    {
        Box::pin(std::future::ready(Self::process_sync(msg, conn, args)))
    }
}

impl PostedAckBroadcastHook {
    fn process_sync(
        msg: &mut HookedWebSocketEvent<'_>,
        conn: &WebConn,
        args: &StringInterface,
    ) -> Result<(), BroadcastHookError> {
        // Don't ACK unless we say to explicitly
        if !(conn.posted_ack && conn.is_active()) {
            return Ok(());
        }

        let posted_user_id = string_arg(args, "posted_user_id").map_err(|source| {
            BroadcastHookError::InvalidArg {
                hook: "postedAckBroadcastHook",
                key: "posted_user_id",
                source,
            }
        })?;

        // Don't ACK your own posts
        if posted_user_id == conn.user_id() {
            return Ok(());
        }

        // Add if we have mentions or followers
        // This works since we currently do have an order for broadcast hooks, but this probably
        // should be reworked going forward
        if msg.get("followers").is_some() || msg.get("mentions").is_some() {
            msg.add("should_ack", serde_json::Value::Bool(true));
            increment_websocket_counter(conn);
            return Ok(());
        }

        let channel_type =
            string_arg(args, "channel_type").map_err(|source| BroadcastHookError::InvalidArg {
                hook: "postedAckBroadcastHook",
                key: "channel_type",
                source,
            })?;

        // Always ACK direct channels
        if channel_type == CHANNEL_TYPE_DIRECT {
            msg.add("should_ack", serde_json::Value::Bool(true));
            increment_websocket_counter(conn);
            return Ok(());
        }

        let users =
            string_array_arg(args, "users").map_err(|source| BroadcastHookError::InvalidArg {
                hook: "postedAckBroadcastHook",
                key: "users",
                source,
            })?;

        if !users.is_empty() && users.contains(&conn.user_id()) {
            msg.add("should_ack", serde_json::Value::Bool(true));
            increment_websocket_counter(conn);
        }

        Ok(())
    }
}

/// Port of `channelMentionsBroadcastHook` (web_broadcast_hooks.go:262).
///
/// Works on the post **as the message carries it** — decoded from `data.post`, which an earlier
/// hook may already have rewritten — and keeps, under `props.channel_mentions`, only the entries
/// whose `id` the recipient may resolve ([`BroadcastHookSuite`]). The entries are put back
/// **verbatim** (`display_name`, `team_name`, `id`), unlike the HTTP read path, which rewrites
/// them without the id. No survivor removes the prop; the post is then re-encoded into the frame.
///
/// An entry that is not an object, or has no non-empty string `id`, is dropped without a
/// lookup. An empty argument map returns before the post is decoded, so a raiser attaching the
/// hook with nothing to filter costs nothing per connection.
#[derive(Debug)]
struct ChannelMentionsBroadcastHook;

impl BroadcastHook for ChannelMentionsBroadcastHook {
    fn process<'a, 'e>(
        &'a self,
        msg: &'a mut HookedWebSocketEvent<'e>,
        conn: &'a WebConn,
        args: &'a StringInterface,
        suite: &'a dyn BroadcastHookSuite,
    ) -> HookFuture<'a, Result<(), BroadcastHookError>>
    where
        'e: 'a,
    {
        Box::pin(async move {
            const HOOK: &str = "channelMentionsBroadcastHook";
            let channel_mentions =
                get_typed_arg::<Option<StringInterface>>(args, "channel_mentions")
                    .map(Option::unwrap_or_default)
                    .map_err(|source| BroadcastHookError::InvalidArg {
                        hook: HOOK,
                        key: "channel_mentions",
                        source,
                    })?;

            // If no channel mentions, nothing to filter
            if channel_mentions.is_empty() {
                return Ok(());
            }

            let mut post = get_post_from_message(msg)
                .map_err(|source| BroadcastHookError::PostFromMessage { hook: HOOK, source })?;

            let mut filtered = StringInterface::new();
            for (channel_name, channel_info) in channel_mentions {
                let Some(info) = channel_info.as_object() else {
                    continue;
                };
                let channel_id = match info.get("id").and_then(serde_json::Value::as_str) {
                    Some(id) if !id.is_empty() => id,
                    _ => continue,
                };
                if suite
                    .has_permission_to_resolve_channel_mention(&conn.user_id(), channel_id)
                    .await
                {
                    filtered.insert(channel_name, channel_info);
                }
            }

            if filtered.is_empty() {
                post.del_prop(POST_PROPS_CHANNEL_MENTIONS);
            } else {
                post.add_prop(
                    POST_PROPS_CHANNEL_MENTIONS,
                    serde_json::Value::Object(filtered),
                );
            }

            let updated = post
                .to_json()
                .map_err(|source| BroadcastHookError::PostToJson { hook: HOOK, source })?;
            msg.add("post", serde_json::Value::String(updated));
            Ok(())
        })
    }
}

/// Port of `incrementWebsocketCounter` (web_broadcast_hooks.go:519). Its first line returns when
/// `Platform.Metrics()` is nil, and there is no metrics service here, so that is the whole
/// function. Kept as a call so the three sites read like Go's.
fn increment_websocket_counter(_conn: &WebConn) {}

#[cfg(test)]
mod tests {
    use super::*;
    use mm_model::session::Session;
    use mm_model::utils::get_millis;
    use mm_model::websocket_message::{WEBSOCKET_EVENT_POSTED, WebSocketEvent};
    use serde_json::json;

    const USER: &str = "6rtg4qbe5bn55mw5t6gphxyaxa";
    const POSTER: &str = "p0sterp0sterp0sterp0sterp0";
    const OTHER: &str = "0therusero0therusero0ther0";
    const CONN: &str = "cccccccccccccccccccccccccc";

    fn session(user_id: &str) -> Session {
        Session {
            id: "sessionid".to_owned(),
            token: "token".to_owned(),
            user_id: user_id.to_owned(),
            expires_at: get_millis() + 60_000,
            roles: "system_user".to_owned(),
            ..Default::default()
        }
    }

    /// A connection for `USER`. The receiver rides along so the queue stays open — see the note on
    /// `hub::tests::conn`.
    fn conn(
        posted_ack: bool,
    ) -> (
        std::sync::Arc<WebConn>,
        tokio::sync::mpsc::Receiver<crate::hub::OutgoingFrame>,
    ) {
        WebConn::new(CONN.to_owned(), session(USER), posted_ack)
    }

    fn posted() -> WebSocketEvent {
        let mut event = WebSocketEvent::new(WEBSOCKET_EVENT_POSTED, "", "chan", "", None, "");
        event.add("post", json!("{}"));
        event
    }

    fn args(value: serde_json::Value) -> StringInterface {
        match value {
            serde_json::Value::Object(map) => map,
            other => panic!("args must be an object, got {other}"),
        }
    }

    /// A suite that resolves exactly the channel ids it was built with, for any user.
    struct ResolvesOnly(Vec<&'static str>);

    impl BroadcastHookSuite for ResolvesOnly {
        fn has_permission_to_resolve_channel_mention<'a>(
            &'a self,
            _user_id: &'a str,
            channel_id: &'a str,
        ) -> HookFuture<'a, bool> {
            Box::pin(async move { self.0.contains(&channel_id) })
        }
    }

    /// Run one hook and return the modified copy, if the hook made one.
    async fn run(
        hook: &dyn BroadcastHook,
        event: &WebSocketEvent,
        conn: &WebConn,
        args: &StringInterface,
    ) -> Result<Option<WebSocketEvent>, BroadcastHookError> {
        run_with(hook, event, conn, args, &ResolvesOnly(Vec::new())).await
    }

    async fn run_with(
        hook: &dyn BroadcastHook,
        event: &WebSocketEvent,
        conn: &WebConn,
        args: &StringInterface,
        suite: &dyn BroadcastHookSuite,
    ) -> Result<Option<WebSocketEvent>, BroadcastHookError> {
        let mut hooked = HookedWebSocketEvent::new(event);
        hook.process(&mut hooked, conn, args, suite).await?;
        Ok(hooked.into_copy())
    }

    fn data_key<'a>(event: &'a Option<WebSocketEvent>, key: &str) -> Option<&'a serde_json::Value> {
        event.as_ref()?.get_data()?.get(key)
    }

    // -----------------------------------------------------------------------------------------
    // add_mentions / add_followers
    // -----------------------------------------------------------------------------------------

    #[tokio::test]
    async fn add_mentions_stringifies_the_recipient_alone_when_listed() {
        let (conn, _rx) = conn(false);
        let out = run(
            &AddMentionsBroadcastHook,
            &posted(),
            &conn,
            &args(json!({ "mentions": [OTHER, USER] })),
        )
        .await
        .unwrap();
        // The client expects a JSON *string* holding an array of exactly this user — not the
        // whole list, and not a JSON array.
        assert_eq!(
            data_key(&out, "mentions"),
            Some(&json!(format!("[\"{USER}\"]"))),
            "{out:?}"
        );
    }

    #[tokio::test]
    async fn add_mentions_leaves_an_unlisted_recipient_untouched() {
        let (conn, _rx) = conn(false);
        let out = run(
            &AddMentionsBroadcastHook,
            &posted(),
            &conn,
            &args(json!({ "mentions": [OTHER] })),
        )
        .await
        .unwrap();
        assert!(
            out.is_none(),
            "no copy is made when nothing is added: {out:?}"
        );

        // An empty list, and Go's nil slice as JSON null, are both "not listed".
        for empty in [json!([]), json!(null)] {
            let out = run(
                &AddMentionsBroadcastHook,
                &posted(),
                &conn,
                &args(json!({ "mentions": empty })),
            )
            .await
            .unwrap();
            assert!(out.is_none());
        }
    }

    #[tokio::test]
    async fn add_mentions_reports_a_missing_or_malformed_argument_and_adds_nothing() {
        let (conn, _rx) = conn(false);
        let missing = run(
            &AddMentionsBroadcastHook,
            &posted(),
            &conn,
            &args(json!({})),
        )
        .await;
        assert!(
            matches!(
                missing,
                Err(BroadcastHookError::InvalidArg {
                    key: "mentions",
                    source: ArgError::Missing(_),
                    ..
                })
            ),
            "{missing:?}"
        );

        let malformed = run(
            &AddMentionsBroadcastHook,
            &posted(),
            &conn,
            &args(json!({ "mentions": 42 })),
        )
        .await;
        assert!(
            matches!(
                malformed,
                Err(BroadcastHookError::InvalidArg {
                    key: "mentions",
                    source: ArgError::Json(_),
                    ..
                })
            ),
            "{malformed:?}"
        );
    }

    #[tokio::test]
    async fn add_followers_is_the_same_shape_under_its_own_key() {
        let (conn, _rx) = conn(false);
        let out = run(
            &AddFollowersBroadcastHook,
            &posted(),
            &conn,
            &args(json!({ "followers": [USER] })),
        )
        .await
        .unwrap();
        assert_eq!(
            data_key(&out, "followers"),
            Some(&json!(format!("[\"{USER}\"]")))
        );
        assert!(
            data_key(&out, "mentions").is_none(),
            "followers must not write the mentions key"
        );

        let out = run(
            &AddFollowersBroadcastHook,
            &posted(),
            &conn,
            &args(json!({ "followers": [OTHER] })),
        )
        .await
        .unwrap();
        assert!(out.is_none());

        // The key it reads is `followers`, not `mentions`.
        let wrong_key = run(
            &AddFollowersBroadcastHook,
            &posted(),
            &conn,
            &args(json!({ "mentions": [USER] })),
        )
        .await;
        assert!(matches!(
            wrong_key,
            Err(BroadcastHookError::InvalidArg {
                key: "followers",
                ..
            })
        ));
    }

    // -----------------------------------------------------------------------------------------
    // posted_ack
    // -----------------------------------------------------------------------------------------

    fn ack_args(channel_type: &str, users: &[&str]) -> StringInterface {
        args(json!({
            "posted_user_id": POSTER,
            "channel_type": channel_type,
            "users": users,
        }))
    }

    fn should_ack(out: &Option<WebSocketEvent>) -> Option<&serde_json::Value> {
        data_key(out, "should_ack")
    }

    #[tokio::test]
    async fn posted_ack_is_only_for_a_connection_that_asked_for_it() {
        // Listed in `users` and a DM — every reason to ack — but the flag is off.
        let (conn, _rx) = conn(false);
        let out = run(
            &PostedAckBroadcastHook,
            &posted(),
            &conn,
            &ack_args(CHANNEL_TYPE_DIRECT, &[USER]),
        )
        .await
        .unwrap();
        assert!(out.is_none(), "{out:?}");

        // The flag test comes before the arguments are read: no args, no error.
        let out = run(&PostedAckBroadcastHook, &posted(), &conn, &args(json!({})))
            .await
            .unwrap();
        assert!(out.is_none());
    }

    #[tokio::test]
    async fn posted_ack_skips_an_inactive_connection() {
        let (conn, _rx) = conn(true);
        conn.set_active(false);
        let out = run(
            &PostedAckBroadcastHook,
            &posted(),
            &conn,
            &ack_args(CHANNEL_TYPE_DIRECT, &[USER]),
        )
        .await
        .unwrap();
        assert!(out.is_none(), "{out:?}");
    }

    #[tokio::test]
    async fn posted_ack_never_acks_the_poster_to_themselves() {
        // The connection belongs to the post's author, flag on, DM, listed: still nothing.
        let (conn, _rx) = WebConn::new(CONN.to_owned(), session(POSTER), true);
        let out = run(
            &PostedAckBroadcastHook,
            &posted(),
            &conn,
            &ack_args(CHANNEL_TYPE_DIRECT, &[POSTER]),
        )
        .await
        .unwrap();
        assert!(out.is_none(), "{out:?}");
    }

    #[tokio::test]
    async fn posted_ack_acks_when_the_frame_already_carries_mentions_or_followers() {
        let (conn, _rx) = conn(true);
        for key in ["mentions", "followers"] {
            let mut event = posted();
            event.add(key, json!(format!("[\"{USER}\"]")));
            // Open channel, not listed: the earlier hook's write is the only reason.
            let out = run(
                &PostedAckBroadcastHook,
                &event,
                &conn,
                &ack_args("O", &[OTHER]),
            )
            .await
            .unwrap();
            assert_eq!(should_ack(&out), Some(&json!(true)), "{key}: {out:?}");
        }
    }

    #[tokio::test]
    async fn posted_ack_always_acks_a_direct_channel() {
        let (conn, _rx) = conn(true);
        let out = run(
            &PostedAckBroadcastHook,
            &posted(),
            &conn,
            &ack_args(CHANNEL_TYPE_DIRECT, &[]),
        )
        .await
        .unwrap();
        assert_eq!(should_ack(&out), Some(&json!(true)), "{out:?}");

        // A group message is not a direct channel here — only `users` can ack it.
        let out = run(
            &PostedAckBroadcastHook,
            &posted(),
            &conn,
            &ack_args("G", &[]),
        )
        .await
        .unwrap();
        assert!(out.is_none(), "{out:?}");
    }

    #[tokio::test]
    async fn posted_ack_acks_a_listed_user_in_an_open_channel_and_nobody_else() {
        let (conn, _rx) = conn(true);
        let out = run(
            &PostedAckBroadcastHook,
            &posted(),
            &conn,
            &ack_args("O", &[OTHER, USER]),
        )
        .await
        .unwrap();
        assert_eq!(should_ack(&out), Some(&json!(true)), "{out:?}");

        let out = run(
            &PostedAckBroadcastHook,
            &posted(),
            &conn,
            &ack_args("O", &[OTHER]),
        )
        .await
        .unwrap();
        assert!(out.is_none(), "not listed, open channel: {out:?}");

        let out = run(
            &PostedAckBroadcastHook,
            &posted(),
            &conn,
            &ack_args("O", &[]),
        )
        .await
        .unwrap();
        assert!(out.is_none(), "empty list: {out:?}");
    }

    #[tokio::test]
    async fn posted_ack_reports_a_missing_poster_once_the_connection_qualifies() {
        let (conn, _rx) = conn(true);
        let err = run(&PostedAckBroadcastHook, &posted(), &conn, &args(json!({}))).await;
        assert!(
            matches!(
                err,
                Err(BroadcastHookError::InvalidArg {
                    key: "posted_user_id",
                    source: ArgError::Missing(_),
                    ..
                })
            ),
            "{err:?}"
        );

        // `channel_type` is read only once the mentions test fails, and `users` only once the
        // DM test fails — so each is reported from the branch that reaches it.
        let err = run(
            &PostedAckBroadcastHook,
            &posted(),
            &conn,
            &args(json!({ "posted_user_id": POSTER })),
        )
        .await;
        assert!(
            matches!(
                err,
                Err(BroadcastHookError::InvalidArg {
                    key: "channel_type",
                    ..
                })
            ),
            "{err:?}"
        );
        let err = run(
            &PostedAckBroadcastHook,
            &posted(),
            &conn,
            &args(json!({ "posted_user_id": POSTER, "channel_type": "O" })),
        )
        .await;
        assert!(
            matches!(
                err,
                Err(BroadcastHookError::InvalidArg { key: "users", .. })
            ),
            "{err:?}"
        );
    }

    #[tokio::test]
    async fn the_registry_holds_exactly_the_four_ported_hooks() {
        let hooks = make_broadcast_hooks();
        let mut ids: Vec<_> = hooks.keys().copied().collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            [
                BROADCAST_ADD_FOLLOWERS,
                BROADCAST_ADD_MENTIONS,
                BROADCAST_CHANNEL_MENTIONS,
                BROADCAST_POSTED_ACK
            ]
        );
    }

    #[tokio::test]
    async fn get_typed_arg_decodes_null_the_way_go_unmarshals_it() {
        let a = args(json!({ "s": null, "l": null, "n": 1 }));
        assert_eq!(string_arg(&a, "s").unwrap(), "");
        assert_eq!(string_array_arg(&a, "l").unwrap(), Vec::<String>::new());
        assert!(matches!(string_arg(&a, "n"), Err(ArgError::Json(_))));
        assert!(matches!(string_arg(&a, "absent"), Err(ArgError::Missing(k)) if k == "absent"));
    }

    // -----------------------------------------------------------------------------------------
    // channel_mentions
    // -----------------------------------------------------------------------------------------

    const CHAN_A: &str = "chanaaaaaaaaaaaaaaaaaaaaaa";
    const CHAN_B: &str = "chanbbbbbbbbbbbbbbbbbbbbbb";

    fn mentions_args() -> StringInterface {
        args(json!({ "channel_mentions": {
            "alpha": { "display_name": "Alpha", "team_name": "t", "id": CHAN_A },
            "beta": { "display_name": "Beta", "team_name": "t", "id": CHAN_B },
            "noid": { "display_name": "No Id", "team_name": "t" },
            "emptyid": { "display_name": "Empty Id", "team_name": "t", "id": "" },
            "notamap": "x"
        }}))
    }

    fn posted_with_post(post: &Post) -> WebSocketEvent {
        let mut event = WebSocketEvent::new(WEBSOCKET_EVENT_POSTED, "", "chan", "", None, "");
        event.add("post", json!(post.to_json().unwrap()));
        event
    }

    fn props_of(event: &Option<WebSocketEvent>) -> Option<serde_json::Value> {
        let post = data_key(event, "post")?.as_str()?;
        let post: serde_json::Value = serde_json::from_str(post).unwrap();
        post.get("props").cloned()
    }

    #[tokio::test]
    async fn channel_mentions_keeps_the_resolvable_entries_verbatim_and_drops_the_rest() {
        let (conn, _rx) = conn(false);
        let post = Post {
            id: "p".repeat(26),
            message: "see ~alpha ~beta".to_owned(),
            ..Default::default()
        };
        // The suite would resolve the empty id too — the hook must not ask it.
        let out = run_with(
            &ChannelMentionsBroadcastHook,
            &posted_with_post(&post),
            &conn,
            &mentions_args(),
            &ResolvesOnly(vec![CHAN_A, ""]),
        )
        .await
        .unwrap();
        assert_eq!(
            props_of(&out).unwrap()["channel_mentions"],
            json!({ "alpha": { "display_name": "Alpha", "team_name": "t", "id": CHAN_A } })
        );
    }

    #[tokio::test]
    async fn channel_mentions_removes_the_prop_when_nothing_resolves() {
        let (conn, _rx) = conn(false);
        let mut post = Post {
            id: "p".repeat(26),
            ..Default::default()
        };
        // The raiser strips the prop before precomputing; a stale one on the frame goes too.
        post.add_prop(POST_PROPS_CHANNEL_MENTIONS, json!({ "alpha": {} }));
        let out = run_with(
            &ChannelMentionsBroadcastHook,
            &posted_with_post(&post),
            &conn,
            &mentions_args(),
            &ResolvesOnly(Vec::new()),
        )
        .await
        .unwrap();
        let props = props_of(&out).expect("the post was rewritten");
        assert!(props.get("channel_mentions").is_none(), "{props}");
    }

    #[tokio::test]
    async fn channel_mentions_with_nothing_to_filter_touches_nothing() {
        let (conn, _rx) = conn(false);
        // No post on the frame at all: an empty map returns before the post is read.
        let mut event = WebSocketEvent::new(WEBSOCKET_EVENT_POSTED, "", "chan", "", None, "");
        event.add("other", json!(1));
        let out = run_with(
            &ChannelMentionsBroadcastHook,
            &event,
            &conn,
            &args(json!({ "channel_mentions": {} })),
            &ResolvesOnly(vec![CHAN_A]),
        )
        .await
        .unwrap();
        assert!(out.is_none());
        // A null argument is Go's nil map: the same early return.
        let out = run_with(
            &ChannelMentionsBroadcastHook,
            &event,
            &conn,
            &args(json!({ "channel_mentions": null })),
            &ResolvesOnly(vec![CHAN_A]),
        )
        .await
        .unwrap();
        assert!(out.is_none());
    }

    #[tokio::test]
    async fn channel_mentions_reports_a_frame_without_a_post() {
        let (conn, _rx) = conn(false);
        let event = WebSocketEvent::new(WEBSOCKET_EVENT_POSTED, "", "chan", "", None, "");
        let err = run_with(
            &ChannelMentionsBroadcastHook,
            &event,
            &conn,
            &mentions_args(),
            &ResolvesOnly(vec![CHAN_A]),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("channelMentionsBroadcastHook failed to get post from message: No post found in message"),
            "{err}"
        );
        // A `post` that is not a string.
        let mut event = WebSocketEvent::new(WEBSOCKET_EVENT_POSTED, "", "chan", "", None, "");
        event.add("post", json!({}));
        let err = run_with(
            &ChannelMentionsBroadcastHook,
            &event,
            &conn,
            &mentions_args(),
            &ResolvesOnly(vec![CHAN_A]),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("Invalid post type in message"),
            "{err}"
        );
        // A malformed argument.
        let err = run_with(
            &ChannelMentionsBroadcastHook,
            &event,
            &conn,
            &args(json!({ "channel_mentions": [1] })),
            &ResolvesOnly(vec![CHAN_A]),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("Invalid channel_mentions value passed to channelMentionsBroadcastHook"),
            "{err}"
        );
    }
}
