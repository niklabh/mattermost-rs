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
//! Go registers nine (`makeBroadcastHooks`, web_broadcast_hooks.go:31). Three are ported — the
//! three `SendNotifications` attaches to every `posted` event:
//!
//! | id | args (JSON types, as `add_hook` must supply them) | effect on a connection |
//! |---|---|---|
//! | [`BROADCAST_ADD_MENTIONS`] | `mentions`: array of user ids | user in the list → `data.mentions = "[\"<user>\"]"` (a **stringified** array) |
//! | [`BROADCAST_ADD_FOLLOWERS`] | `followers`: array of user ids | same, key `followers` |
//! | [`BROADCAST_POSTED_ACK`] | `posted_user_id`: string, `channel_type`: string, `users`: array of user ids | `data.should_ack = true` for a `?posted_ack=true` connection that is not the poster's, when the frame already carries `mentions`/`followers`, or the channel is a DM, or the user is in `users` |
//!
//! `posted_ack` reads what `add_mentions` and `add_followers` wrote, so **attach it after them**
//! — Go's own comment says this "works since we currently do have an order for broadcast hooks".
//!
//! The other six — `permalink`, `channel_mentions`, `burn_on_read`, `burn_on_read_reaction`,
//! `abac_files`, `only_channel_admins` — are not registered. An event carrying one of their ids
//! reaches the runner, which logs Go's `Unable to find broadcast hook` warning and skips it, so
//! the frame leaves unmodified and precomputed. Their ids are declared below so a raiser can
//! attach them today; `channel_join_request` already attaches `only_channel_admins`. See [D-183].
//!
//! # `getTypedArg` in a single process
//!
//! Go re-marshals an argument through JSON when its runtime type is not the one the hook wants,
//! because in a cluster the args arrive JSON-decoded and untyped. Here they are always
//! `serde_json::Value`, so [`get_typed_arg`] is only the decode half — and a `null` decodes the
//! way Go's `json.Unmarshal` decodes it into a slice or string: to the empty value, not an error.

use std::collections::HashMap;

use mm_model::channel::CHANNEL_TYPE_DIRECT;
use mm_model::utils::{StringInterface, array_to_json};
use serde::de::DeserializeOwned;

use crate::hub::{BroadcastHook, HookedWebSocketEvent, WebConn};

/// `broadcastAddMentions` (web_broadcast_hooks.go:20).
pub const BROADCAST_ADD_MENTIONS: &str = "add_mentions";
/// `broadcastAddFollowers` (web_broadcast_hooks.go:21).
pub const BROADCAST_ADD_FOLLOWERS: &str = "add_followers";
/// `broadcastPostedAck` (web_broadcast_hooks.go:22).
pub const BROADCAST_POSTED_ACK: &str = "posted_ack";
/// `broadcastPermalink` (web_broadcast_hooks.go:23). Declared, not registered.
pub const BROADCAST_PERMALINK: &str = "permalink";
/// `broadcastChannelMentions` (web_broadcast_hooks.go:24). Declared, not registered.
pub const BROADCAST_CHANNEL_MENTIONS: &str = "channel_mentions";
/// `broadcastBurnOnRead` (web_broadcast_hooks.go:25). Declared, not registered.
pub const BROADCAST_BURN_ON_READ: &str = "burn_on_read";
/// `broadcastBurnOnReadReaction` (web_broadcast_hooks.go:26). Declared, not registered.
pub const BROADCAST_BURN_ON_READ_REACTION: &str = "burn_on_read_reaction";
/// `broadcastAbacFiles` (web_broadcast_hooks.go:27). Declared, not registered.
pub const BROADCAST_ABAC_FILES: &str = "abac_files";
/// `broadcastOnlyChannelAdmins` (web_broadcast_hooks.go:28). Declared, not registered.
pub const BROADCAST_ONLY_CHANNEL_ADMINS: &str = "only_channel_admins";

/// Port of `Server.makeBroadcastHooks` (web_broadcast_hooks.go:31), reduced to the three hooks
/// that are ported. The map is what `hubStart` hands each hub (web_hub.go:124).
pub fn make_broadcast_hooks() -> HashMap<&'static str, Box<dyn BroadcastHook>> {
    let mut hooks: HashMap<&'static str, Box<dyn BroadcastHook>> = HashMap::new();
    hooks.insert(BROADCAST_ADD_MENTIONS, Box::new(AddMentionsBroadcastHook));
    hooks.insert(BROADCAST_ADD_FOLLOWERS, Box::new(AddFollowersBroadcastHook));
    hooks.insert(BROADCAST_POSTED_ACK, Box::new(PostedAckBroadcastHook));
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
    fn process(
        &self,
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

        if !mentions.is_empty() && mentions.contains(&conn.user_id) {
            // Note that the client expects this field to be stringified
            msg.add(
                "mentions",
                serde_json::Value::String(array_to_json(Some(std::slice::from_ref(&conn.user_id)))),
            );
        }

        Ok(())
    }
}

/// Port of `addFollowersBroadcastHook` (web_broadcast_hooks.go:70).
#[derive(Debug)]
struct AddFollowersBroadcastHook;

impl BroadcastHook for AddFollowersBroadcastHook {
    fn process(
        &self,
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

        if !followers.is_empty() && followers.contains(&conn.user_id) {
            // Note that the client expects this field to be stringified
            msg.add(
                "followers",
                serde_json::Value::String(array_to_json(Some(std::slice::from_ref(&conn.user_id)))),
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
    fn process(
        &self,
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
        if posted_user_id == conn.user_id {
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

        if !users.is_empty() && users.contains(&conn.user_id) {
            msg.add("should_ack", serde_json::Value::Bool(true));
            increment_websocket_counter(conn);
        }

        Ok(())
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

    /// Run one hook and return the modified copy, if the hook made one.
    fn run(
        hook: &dyn BroadcastHook,
        event: &WebSocketEvent,
        conn: &WebConn,
        args: &StringInterface,
    ) -> Result<Option<WebSocketEvent>, BroadcastHookError> {
        let mut hooked = HookedWebSocketEvent::new(event);
        hook.process(&mut hooked, conn, args)?;
        Ok(hooked.into_copy())
    }

    fn data_key<'a>(event: &'a Option<WebSocketEvent>, key: &str) -> Option<&'a serde_json::Value> {
        event.as_ref()?.get_data()?.get(key)
    }

    // -----------------------------------------------------------------------------------------
    // add_mentions / add_followers
    // -----------------------------------------------------------------------------------------

    #[test]
    fn add_mentions_stringifies_the_recipient_alone_when_listed() {
        let (conn, _rx) = conn(false);
        let out = run(
            &AddMentionsBroadcastHook,
            &posted(),
            &conn,
            &args(json!({ "mentions": [OTHER, USER] })),
        )
        .unwrap();
        // The client expects a JSON *string* holding an array of exactly this user — not the
        // whole list, and not a JSON array.
        assert_eq!(
            data_key(&out, "mentions"),
            Some(&json!(format!("[\"{USER}\"]"))),
            "{out:?}"
        );
    }

    #[test]
    fn add_mentions_leaves_an_unlisted_recipient_untouched() {
        let (conn, _rx) = conn(false);
        let out = run(
            &AddMentionsBroadcastHook,
            &posted(),
            &conn,
            &args(json!({ "mentions": [OTHER] })),
        )
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
            .unwrap();
            assert!(out.is_none());
        }
    }

    #[test]
    fn add_mentions_reports_a_missing_or_malformed_argument_and_adds_nothing() {
        let (conn, _rx) = conn(false);
        let missing = run(
            &AddMentionsBroadcastHook,
            &posted(),
            &conn,
            &args(json!({})),
        );
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
        );
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

    #[test]
    fn add_followers_is_the_same_shape_under_its_own_key() {
        let (conn, _rx) = conn(false);
        let out = run(
            &AddFollowersBroadcastHook,
            &posted(),
            &conn,
            &args(json!({ "followers": [USER] })),
        )
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
        .unwrap();
        assert!(out.is_none());

        // The key it reads is `followers`, not `mentions`.
        let wrong_key = run(
            &AddFollowersBroadcastHook,
            &posted(),
            &conn,
            &args(json!({ "mentions": [USER] })),
        );
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

    #[test]
    fn posted_ack_is_only_for_a_connection_that_asked_for_it() {
        // Listed in `users` and a DM — every reason to ack — but the flag is off.
        let (conn, _rx) = conn(false);
        let out = run(
            &PostedAckBroadcastHook,
            &posted(),
            &conn,
            &ack_args(CHANNEL_TYPE_DIRECT, &[USER]),
        )
        .unwrap();
        assert!(out.is_none(), "{out:?}");

        // The flag test comes before the arguments are read: no args, no error.
        let out = run(&PostedAckBroadcastHook, &posted(), &conn, &args(json!({}))).unwrap();
        assert!(out.is_none());
    }

    #[test]
    fn posted_ack_skips_an_inactive_connection() {
        let (conn, _rx) = conn(true);
        conn.set_active(false);
        let out = run(
            &PostedAckBroadcastHook,
            &posted(),
            &conn,
            &ack_args(CHANNEL_TYPE_DIRECT, &[USER]),
        )
        .unwrap();
        assert!(out.is_none(), "{out:?}");
    }

    #[test]
    fn posted_ack_never_acks_the_poster_to_themselves() {
        // The connection belongs to the post's author, flag on, DM, listed: still nothing.
        let (conn, _rx) = WebConn::new(CONN.to_owned(), session(POSTER), true);
        let out = run(
            &PostedAckBroadcastHook,
            &posted(),
            &conn,
            &ack_args(CHANNEL_TYPE_DIRECT, &[POSTER]),
        )
        .unwrap();
        assert!(out.is_none(), "{out:?}");
    }

    #[test]
    fn posted_ack_acks_when_the_frame_already_carries_mentions_or_followers() {
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
            .unwrap();
            assert_eq!(should_ack(&out), Some(&json!(true)), "{key}: {out:?}");
        }
    }

    #[test]
    fn posted_ack_always_acks_a_direct_channel() {
        let (conn, _rx) = conn(true);
        let out = run(
            &PostedAckBroadcastHook,
            &posted(),
            &conn,
            &ack_args(CHANNEL_TYPE_DIRECT, &[]),
        )
        .unwrap();
        assert_eq!(should_ack(&out), Some(&json!(true)), "{out:?}");

        // A group message is not a direct channel here — only `users` can ack it.
        let out = run(
            &PostedAckBroadcastHook,
            &posted(),
            &conn,
            &ack_args("G", &[]),
        )
        .unwrap();
        assert!(out.is_none(), "{out:?}");
    }

    #[test]
    fn posted_ack_acks_a_listed_user_in_an_open_channel_and_nobody_else() {
        let (conn, _rx) = conn(true);
        let out = run(
            &PostedAckBroadcastHook,
            &posted(),
            &conn,
            &ack_args("O", &[OTHER, USER]),
        )
        .unwrap();
        assert_eq!(should_ack(&out), Some(&json!(true)), "{out:?}");

        let out = run(
            &PostedAckBroadcastHook,
            &posted(),
            &conn,
            &ack_args("O", &[OTHER]),
        )
        .unwrap();
        assert!(out.is_none(), "not listed, open channel: {out:?}");

        let out = run(
            &PostedAckBroadcastHook,
            &posted(),
            &conn,
            &ack_args("O", &[]),
        )
        .unwrap();
        assert!(out.is_none(), "empty list: {out:?}");
    }

    #[test]
    fn posted_ack_reports_a_missing_poster_once_the_connection_qualifies() {
        let (conn, _rx) = conn(true);
        let err = run(&PostedAckBroadcastHook, &posted(), &conn, &args(json!({})));
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
        );
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
        );
        assert!(
            matches!(
                err,
                Err(BroadcastHookError::InvalidArg { key: "users", .. })
            ),
            "{err:?}"
        );
    }

    #[test]
    fn the_registry_holds_exactly_the_three_ported_hooks() {
        let hooks = make_broadcast_hooks();
        let mut ids: Vec<_> = hooks.keys().copied().collect();
        ids.sort_unstable();
        assert_eq!(
            ids,
            [
                BROADCAST_ADD_FOLLOWERS,
                BROADCAST_ADD_MENTIONS,
                BROADCAST_POSTED_ACK
            ]
        );
    }

    #[test]
    fn get_typed_arg_decodes_null_the_way_go_unmarshals_it() {
        let a = args(json!({ "s": null, "l": null, "n": 1 }));
        assert_eq!(string_arg(&a, "s").unwrap(), "");
        assert_eq!(string_array_arg(&a, "l").unwrap(), Vec::<String>::new());
        assert!(matches!(string_arg(&a, "n"), Err(ArgError::Json(_))));
        assert!(matches!(string_arg(&a, "absent"), Err(ArgError::Missing(k)) if k == "absent"));
    }
}
