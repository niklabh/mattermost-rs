//! The clients Go writes by hand (client_rpc.go), where the wire structs are plain but the
//! behaviour is not: the hooks that keep the caller's value as the default answer, the log
//! methods, and `LoadPluginConfiguration`.
//!
//! Their servers are generated like any other, because only the client side differs.

use std::collections::HashMap;

use go_netrpc::ServiceError;
use gobwire::Interface;
use serde_json::{Map, Value as Json};

use super::{ApiClient, HooksClient, NotImplemented, PluginApi, hook_id};
use crate::wire::logr::Level;
use crate::wire::model::{AuditRecord, ChannelMember, Post, TeamMember};
use crate::wire::plugin::{
    Z_ChannelMemberWillBeAddedArgs, Z_ChannelMemberWillBeAddedReturns,
    Z_LoadPluginConfigurationArgsArgs, Z_LoadPluginConfigurationArgsReturns, Z_LogAuditRecArgs,
    Z_LogAuditRecReturns, Z_LogAuditRecWithLevelArgs, Z_LogAuditRecWithLevelReturns,
    Z_LogDebugArgs, Z_LogDebugReturns, Z_LogErrorArgs, Z_LogErrorReturns, Z_LogInfoArgs,
    Z_LogInfoReturns, Z_LogWarnArgs, Z_LogWarnReturns, Z_MessageWillBePostedArgs,
    Z_MessageWillBePostedReturns, Z_MessageWillBeUpdatedArgs, Z_MessageWillBeUpdatedReturns,
    Z_MessagesWillBeConsumedArgs, Z_MessagesWillBeConsumedReturns,
    Z_MessagesWillBeConsumedWithContextArgs, Z_MessagesWillBeConsumedWithContextReturns,
    Z_TeamMemberWillBeAddedArgs, Z_TeamMemberWillBeAddedReturns,
};
use crate::wire::{interface_to_json, registered};

/// The four hooks whose answer defaults to what the caller passed in, so that a plugin which
/// sends back a partial value does not silently drop the fields it left out.
///
/// Go seeds the reply struct with the argument and lets gob decode **into** it: a field the
/// plugin omits keeps the caller's value (client_rpc.go, "the difficulty of identifying which
/// fields need special behaviour"). `HooksClient::call_merging` does the same. Go's seed aliases
/// the caller's value; Rust clones it, because the argument is still needed to make the call.
macro_rules! merging_hook {
    ($(#[$doc:meta])* $method:ident, $with_rpc_err:ident, $name:literal, $id:ident, $args:ty, $returns:ty, $value:ty) => {
        $(#[$doc])*
        pub async fn $method(&self, args: $args) -> $returns {
            let seed = <$returns>::default_from(args.b.clone());
            self.call_merging(hook_id::$id, $name, &args, seed).await
        }

        $(#[$doc])*
        ///
        /// The `WithRPCErr` companion, which does **not** seed the answer: a transport failure
        /// gives zero values and the error (client_rpc.go).
        pub async fn $with_rpc_err(&self, args: $args) -> ($returns, Option<go_netrpc::Error>) {
            self.call_with_rpc_err(hook_id::$id, $name, &args).await
        }
    };
}

/// A returns struct seeded with the argument the caller passed.
trait DefaultFrom<T> {
    fn default_from(value: T) -> Self;
}

macro_rules! default_from {
    ($returns:ty, $value:ty) => {
        impl DefaultFrom<::std::option::Option<::std::boxed::Box<$value>>> for $returns {
            fn default_from(value: ::std::option::Option<::std::boxed::Box<$value>>) -> Self {
                Self {
                    a: value,
                    b: String::new(),
                }
            }
        }
    };
}
default_from!(Z_MessageWillBePostedReturns, Post);
default_from!(Z_MessageWillBeUpdatedReturns, Post);
default_from!(Z_ChannelMemberWillBeAddedReturns, ChannelMember);
default_from!(Z_TeamMemberWillBeAddedReturns, TeamMember);

impl HooksClient {
    merging_hook!(
        /// Go: `MessageWillBePosted(c *Context, post *model.Post) (*model.Post, string)`.
        message_will_be_posted,
        message_will_be_posted_with_rpc_err,
        "MessageWillBePosted",
        MESSAGE_WILL_BE_POSTED,
        Z_MessageWillBePostedArgs,
        Z_MessageWillBePostedReturns,
        Post
    );

    merging_hook!(
        /// Go: `ChannelMemberWillBeAdded(c *Context, channelMember *model.ChannelMember)
        /// (*model.ChannelMember, string)`.
        channel_member_will_be_added,
        channel_member_will_be_added_with_rpc_err,
        "ChannelMemberWillBeAdded",
        CHANNEL_MEMBER_WILL_BE_ADDED,
        Z_ChannelMemberWillBeAddedArgs,
        Z_ChannelMemberWillBeAddedReturns,
        ChannelMember
    );

    merging_hook!(
        /// Go: `TeamMemberWillBeAdded(c *Context, teamMember *model.TeamMember)
        /// (*model.TeamMember, string)`.
        team_member_will_be_added,
        team_member_will_be_added_with_rpc_err,
        "TeamMemberWillBeAdded",
        TEAM_MEMBER_WILL_BE_ADDED,
        Z_TeamMemberWillBeAddedArgs,
        Z_TeamMemberWillBeAddedReturns,
        TeamMember
    );

    /// Go: `MessageWillBeUpdated(c *Context, newPost, oldPost *model.Post) (*model.Post, string)`.
    ///
    /// The new post is the default answer, but — unlike [`HooksClient::message_will_be_posted`] —
    /// a plugin that answers replaces it outright rather than merging into it (client_rpc.go).
    pub async fn message_will_be_updated(
        &self,
        args: Z_MessageWillBeUpdatedArgs,
    ) -> Z_MessageWillBeUpdatedReturns {
        let default = Z_MessageWillBeUpdatedReturns::default_from(args.b.clone());
        if !self.implements(hook_id::MESSAGE_WILL_BE_UPDATED) {
            return default;
        }
        match self.rpc("MessageWillBeUpdated", &args).await {
            Ok(returns) => returns,
            Err(e) => {
                tracing::error!(error = %e, "RPC call MessageWillBeUpdated to plugin failed.");
                default
            }
        }
    }

    /// [`HooksClient::message_will_be_updated`], also returning the transport error. It does not
    /// seed the answer: a transport failure gives zero values, not the new post.
    pub async fn message_will_be_updated_with_rpc_err(
        &self,
        args: Z_MessageWillBeUpdatedArgs,
    ) -> (Z_MessageWillBeUpdatedReturns, Option<go_netrpc::Error>) {
        self.call_with_rpc_err(
            hook_id::MESSAGE_WILL_BE_UPDATED,
            "MessageWillBeUpdated",
            &args,
        )
        .await
    }

    /// Go: `MessagesWillBeConsumed(posts []*model.Post) []*model.Post`.
    ///
    /// Unlike the hooks above this one keeps no default: a plugin that does not implement it, or
    /// a failed call, answers with no posts at all (client_rpc.go).
    pub async fn messages_will_be_consumed(
        &self,
        args: Z_MessagesWillBeConsumedArgs,
    ) -> Z_MessagesWillBeConsumedReturns {
        self.call(
            hook_id::MESSAGES_WILL_BE_CONSUMED,
            "MessagesWillBeConsumed",
            &args,
        )
        .await
    }

    /// Go: `MessagesWillBeConsumedWithContext(c *Context, posts []*model.Post) []*model.Post`.
    pub async fn messages_will_be_consumed_with_context(
        &self,
        args: Z_MessagesWillBeConsumedWithContextArgs,
    ) -> Z_MessagesWillBeConsumedWithContextReturns {
        self.call(
            hook_id::MESSAGES_WILL_BE_CONSUMED_WITH_CONTEXT,
            "MessagesWillBeConsumedWithContext",
            &args,
        )
        .await
    }
}

/// Go's `stringifyToObjects` (stringifier.go): the key/value pairs cross as strings, because the
/// plugin formats them with `%+v` before sending. A Rust caller formats its own values, so this
/// takes them already formatted.
fn stringified(pairs: &[String]) -> Vec<Option<Interface>> {
    pairs.iter().map(|s| Some(Interface::string(s))).collect()
}

macro_rules! log_method {
    ($method:ident, $name:literal, $args:ident, $returns:ty) => {
        #[doc = concat!("Go: `", $name, "(msg string, keyValuePairs ...any)`. A failed call is logged, never returned.")]
        pub async fn $method(&self, msg: &str, pairs: &[String]) {
            let args = $args {
                a: msg.to_owned(),
                b: stringified(pairs),
            };
            let _: $returns = self.call($name, &args).await;
        }
    };
}

impl ApiClient {
    log_method!(log_debug, "LogDebug", Z_LogDebugArgs, Z_LogDebugReturns);
    log_method!(log_info, "LogInfo", Z_LogInfoArgs, Z_LogInfoReturns);
    log_method!(log_warn, "LogWarn", Z_LogWarnArgs, Z_LogWarnReturns);
    log_method!(log_error, "LogError", Z_LogErrorArgs, Z_LogErrorReturns);

    /// Go: `LoadPluginConfiguration(dest any) error`, the plugin's own configuration as JSON.
    ///
    /// Go unmarshals it into the caller's value and logs a failure; this hands back the bytes,
    /// because the shape is the plugin's own. The host answers `null` when it has no
    /// configuration for this plugin.
    pub async fn load_plugin_configuration(&self) -> Vec<u8> {
        let returns: Z_LoadPluginConfigurationArgsReturns = self
            .call(
                "LoadPluginConfiguration",
                &Z_LoadPluginConfigurationArgsArgs {},
            )
            .await;
        returns.a
    }
}

/// `Plugin.LoadPluginConfiguration`, whose server is hand-written because it answers `null`
/// rather than a not-implemented error when the host has no configuration (client_rpc.go).
pub(super) fn register_load_plugin_configuration<T: PluginApi>(
    server: &mut go_netrpc::Server,
    implementation: &std::sync::Arc<T>,
) {
    let this = std::sync::Arc::clone(implementation);
    server.register(
        "Plugin.LoadPluginConfiguration",
        move |args: Z_LoadPluginConfigurationArgsArgs| {
            let this = std::sync::Arc::clone(&this);
            async move {
                Ok::<_, ServiceError>(match this.load_plugin_configuration(args).await {
                    Ok(returns) => returns,
                    // Go marshals a nil `any`, which is the four bytes `null`.
                    Err(NotImplemented) => Z_LoadPluginConfigurationArgsReturns {
                        a: b"null".to_vec(),
                    },
                })
            }
        },
    );
}

/// Go's `makeAuditRecordGobSafe` (audit.go): the record's four `map[string]any` fields go through
/// a JSON round trip, which drops the nil pointers inside interfaces that gob refuses to encode.
///
/// A map that cannot be marshalled becomes Go's one-key error map, as Go's does.
fn make_audit_record_gob_safe(mut record: AuditRecord) -> AuditRecord {
    record.event_data.parameters = make_map_gob_safe(&record.event_data.parameters);
    record.event_data.prior_state = make_map_gob_safe(&record.event_data.prior_state);
    record.event_data.result_state = make_map_gob_safe(&record.event_data.result_state);
    record.meta = make_map_gob_safe(&record.meta);
    record
}

type AnyMap = HashMap<String, Option<Interface>>;

fn make_map_gob_safe(m: &AnyMap) -> AnyMap {
    let failed = |what: &str| AnyMap::from([("error".into(), Some(Interface::string(what)))]);
    let mut json = Map::new();
    for (key, value) in m {
        let Some(value) = (match value {
            Some(i) => interface_to_json(i),
            None => Some(Json::Null),
        }) else {
            return failed("failed to serialize audit data");
        };
        json.insert(key.clone(), value);
    }
    // Go unmarshals into a fresh `map[string]any`, which is what changes the types: every number
    // becomes a float64, every object a map, every array a slice.
    json.into_iter()
        .map(|(k, v)| (k, json_to_interface(&v)))
        .collect()
}

/// What Go's `json.Unmarshal` into an `any` leaves behind, as gob then sends it.
fn json_to_interface(value: &Json) -> Option<Interface> {
    Some(match value {
        Json::Null => return None,
        Json::Bool(b) => Interface::bool(*b),
        // Every JSON number arrives as a float64, however it was written.
        Json::Number(n) => Interface::float64(n.as_f64().unwrap_or(f64::NAN)),
        Json::String(s) => Interface::string(s),
        Json::Array(items) => {
            let values: Vec<Option<Interface>> = items.iter().map(json_to_interface).collect();
            Interface::new(registered::ANY_SLICE, &values).ok()?
        }
        Json::Object(fields) => {
            let values: AnyMap = fields
                .iter()
                .map(|(k, v)| (k.clone(), json_to_interface(v)))
                .collect();
            Interface::new(registered::STRING_ANY_MAP, &values).ok()?
        }
    })
}

impl ApiClient {
    /// Go: `LogAuditRec(rec *model.AuditRecord)`. The record is made gob-safe first.
    pub async fn log_audit_rec(&self, record: AuditRecord) {
        let args = Z_LogAuditRecArgs {
            a: Some(Box::new(make_audit_record_gob_safe(record))),
        };
        let _: Z_LogAuditRecReturns = self.call("LogAuditRec", &args).await;
    }

    /// Go: `LogAuditRecWithLevel(rec *model.AuditRecord, level mlog.Level)`.
    pub async fn log_audit_rec_with_level(&self, record: AuditRecord, level: Level) {
        let args = Z_LogAuditRecWithLevelArgs {
            a: Some(Box::new(make_audit_record_gob_safe(record))),
            b: level,
        };
        let _: Z_LogAuditRecWithLevelReturns = self.call("LogAuditRecWithLevel", &args).await;
    }
}

#[cfg(test)]
mod tests {
    use gobwire::{Type, Value};

    use super::*;

    /// A value no JSON can be made of: Go's `json.Marshal` fails and the whole map becomes the
    /// one-key error map (audit.go, `makeMapGobSafe`).
    #[test]
    fn a_map_that_cannot_be_marshalled_becomes_the_error_map() {
        let unmarshalable = Interface {
            name: "*model.Unknown".into(),
            ty: Type::Marshaler(gobwire::MarshalKind::Binary, "unknown".into()),
            value: Value::Marshaled(vec![1, 2, 3]),
        };
        let map = AnyMap::from([
            ("fine".into(), Some(Interface::string("kept"))),
            ("broken".into(), Some(unmarshalable)),
        ]);

        let safe = make_map_gob_safe(&map);
        assert_eq!(
            safe,
            AnyMap::from([(
                "error".into(),
                Some(Interface::string("failed to serialize audit data"))
            )])
        );
    }

    /// The round trip changes types exactly as Go's does.
    #[test]
    fn the_round_trip_makes_numbers_floats_and_drops_nils() {
        let map = AnyMap::from([
            ("int".into(), Some(Interface::int(7))),
            ("text".into(), Some(Interface::string("kept"))),
            ("nil".into(), None),
        ]);

        let safe = make_map_gob_safe(&map);
        assert_eq!(safe["int"], Some(Interface::float64(7.0)));
        assert_eq!(safe["text"], Some(Interface::string("kept")));
        assert_eq!(safe["nil"], None, "a nil stays nil through JSON's null");
    }
}
