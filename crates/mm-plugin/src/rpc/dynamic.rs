//! The four API methods whose Go map may be **empty and non-nil**: `GetConfig`,
//! `GetUnsanitizedConfig` and `GetPluginConfig` answer one, and `SavePluginConfig` is sent one
//! (client_rpc_generated.go).
//!
//! Go's encoder omits a nil map from a struct and **sends** an empty non-nil one ("the receiver
//! might want to use the map", encode.go), so the plugin can tell the two apart — and does:
//! `GetPluginConfig` answers `map[string]any{}` for a plugin with no settings, and a plugin that
//! writes into it works, where writing into the nil map a missing field decodes to panics. The
//! same holds for `Config.PluginSettings.Plugins` and `PluginStates`, which `SetDefaults` makes
//! non-nil, and which a plugin edits before `SaveConfig`. A Rust `HashMap` cannot be nil, so the
//! generated wire structs send an empty one as nil (gobwire's documented divergence).
//!
//! The same holds the other way: `SavePluginConfig(map[string]any{})` stores `{}` in Go's
//! document and `SavePluginConfig(nil)` stores `null`, and only the arguments' gob says which.
//!
//! So these four are served by hand, through [`PluginApiDynamic`]: the three answers may be a
//! [`gobwire::Dynamic`] built from what Go would hold — a JSON `{}` becomes a sent empty map and
//! a `null` an omitted one — and `SavePluginConfig`'s arguments arrive as one. Each method's
//! default is the typed [`PluginApi`] method, so an implementation that only has the typed form
//! (the conformance fakes) is served as before.

use std::future::Future;
use std::sync::Arc;

use go_netrpc::{Server, ServiceError};
use gobwire::{Describer, Dynamic, Encode, ValueEncoder};

use super::{NotImplemented, PluginApi};
use crate::wire::plugin::{
    Z_GetConfigArgs, Z_GetConfigReturns, Z_GetPluginConfigArgs, Z_GetPluginConfigReturns,
    Z_GetUnsanitizedConfigArgs, Z_GetUnsanitizedConfigReturns, Z_SavePluginConfigArgs,
    Z_SavePluginConfigReturns,
};

/// A `Z_<Method>Returns` as the generated struct, or as a dynamic value of the same Go shape.
#[derive(Debug, Clone, PartialEq)]
pub enum Answer<T> {
    /// The generated struct: an empty map in it is sent as nil.
    Typed(T),
    /// The whole returns struct as gob will carry it, empty maps included.
    Dynamic(Dynamic),
}

impl<T: Encode> Encode for Answer<T> {
    fn describe_value(&self, d: &mut Describer<'_>) -> gobwire::Result<i64> {
        match self {
            Answer::Typed(t) => t.describe_value(d),
            Answer::Dynamic(v) => v.describe_value(d),
        }
    }

    fn is_zero(&self) -> bool {
        match self {
            Answer::Typed(t) => t.is_zero(),
            Answer::Dynamic(v) => v.is_zero(),
        }
    }

    fn frames_as_struct(&self) -> bool {
        match self {
            Answer::Typed(t) => t.frames_as_struct(),
            Answer::Dynamic(v) => v.frames_as_struct(),
        }
    }

    fn encode(&self, e: &mut ValueEncoder<'_>) -> gobwire::Result<()> {
        match self {
            Answer::Typed(t) => t.encode(e),
            Answer::Dynamic(v) => v.encode(e),
        }
    }
}

/// The server API's four map-carrying methods, as the host serves them. A host implements
/// this beside [`PluginApi`]; the defaults defer to the typed methods.
pub trait PluginApiDynamic: PluginApi {
    /// Go: `GetConfig() *model.Config`.
    fn get_config_dynamic(
        &self,
        args: Z_GetConfigArgs,
    ) -> impl Future<Output = Result<Answer<Z_GetConfigReturns>, NotImplemented>> + Send {
        async move { self.get_config(args).await.map(Answer::Typed) }
    }

    /// Go: `GetUnsanitizedConfig() *model.Config`.
    fn get_unsanitized_config_dynamic(
        &self,
        args: Z_GetUnsanitizedConfigArgs,
    ) -> impl Future<Output = Result<Answer<Z_GetUnsanitizedConfigReturns>, NotImplemented>> + Send
    {
        async move { self.get_unsanitized_config(args).await.map(Answer::Typed) }
    }

    /// Go: `GetPluginConfig() map[string]any`.
    fn get_plugin_config_dynamic(
        &self,
        args: Z_GetPluginConfigArgs,
    ) -> impl Future<Output = Result<Answer<Z_GetPluginConfigReturns>, NotImplemented>> + Send {
        async move { self.get_plugin_config(args).await.map(Answer::Typed) }
    }

    /// Go: `SavePluginConfig(pluginConfig map[string]any) *model.AppError`, with the arguments as
    /// they arrived: `Z_SavePluginConfigArgs`, whose `A` is absent for a nil map and present for
    /// an empty one. The default declines, and the server then decodes the typed arguments for
    /// [`PluginApi::save_plugin_config`].
    fn save_plugin_config_dynamic(
        &self,
        args: &Dynamic,
    ) -> impl Future<Output = Result<Z_SavePluginConfigReturns, NotImplemented>> + Send {
        let _ = args;
        async { Err(NotImplemented) }
    }
}

/// A dynamic value decoded into its generated struct, as the generated server would have decoded
/// the stream.
fn typed<T: gobwire::Decode + Default>(value: Dynamic) -> Result<T, ServiceError> {
    gobwire::Interface {
        name: String::new(),
        ty: value.ty,
        value: value.value,
    }
    .downcast()
    .map_err(|e| ServiceError(e.to_string()))
}

/// `Plugin.GetConfig`, `Plugin.GetUnsanitizedConfig`, `Plugin.GetPluginConfig` and
/// `Plugin.SavePluginConfig`, served through [`PluginApiDynamic`], with Go's not-implemented
/// error when neither form is provided.
pub(super) fn register_api_dynamic<T: PluginApiDynamic>(
    server: &mut Server,
    implementation: &Arc<T>,
) {
    let this = Arc::clone(implementation);
    server.register("Plugin.GetConfig", move |args: Z_GetConfigArgs| {
        let this = Arc::clone(&this);
        async move {
            this.get_config_dynamic(args)
                .await
                .map_err(|NotImplemented| {
                    ServiceError("API GetConfig called but not implemented.".into())
                })
        }
    });
    let this = Arc::clone(implementation);
    server.register(
        "Plugin.GetUnsanitizedConfig",
        move |args: Z_GetUnsanitizedConfigArgs| {
            let this = Arc::clone(&this);
            async move {
                this.get_unsanitized_config_dynamic(args)
                    .await
                    .map_err(|NotImplemented| {
                        ServiceError("API GetUnsanitizedConfig called but not implemented.".into())
                    })
            }
        },
    );
    let this = Arc::clone(implementation);
    server.register(
        "Plugin.GetPluginConfig",
        move |args: Z_GetPluginConfigArgs| {
            let this = Arc::clone(&this);
            async move {
                this.get_plugin_config_dynamic(args)
                    .await
                    .map_err(|NotImplemented| {
                        ServiceError("API GetPluginConfig called but not implemented.".into())
                    })
            }
        },
    );
    let this = Arc::clone(implementation);
    server.register("Plugin.SavePluginConfig", move |args: Dynamic| {
        let this = Arc::clone(&this);
        async move {
            let not_implemented =
                || ServiceError("API SavePluginConfig called but not implemented.".into());
            match this.save_plugin_config_dynamic(&args).await {
                Ok(returns) => Ok(returns),
                Err(NotImplemented) => {
                    let args: Z_SavePluginConfigArgs = typed(args)?;
                    this.save_plugin_config(args)
                        .await
                        .map_err(|NotImplemented| not_implemented())
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use gobwire::{Decoder, Encoder, Progress, StructType, Type, Value};

    use super::*;

    fn decode_dynamic(bytes: &[u8]) -> Dynamic {
        let mut dec = Decoder::new();
        let mut rest = bytes;
        loop {
            let (width, n) = gobwire::parse_length_prefix(rest).unwrap().unwrap();
            let body = &rest[width..width + n];
            rest = &rest[width + n..];
            if dec.push_message(body).unwrap() == Progress::Ready {
                return dec.decode().unwrap();
            }
        }
    }

    fn plugin_config_returns(map: Option<Value>) -> Dynamic {
        Dynamic {
            ty: Type::Struct(Arc::new(StructType {
                name: "Z_GetPluginConfigReturns".into(),
                fields: vec![(
                    "A".into(),
                    Type::Map(Arc::new(Type::String), Arc::new(Type::Interface)),
                )],
            })),
            value: Value::Struct(vec![map]),
        }
    }

    /// The typed answer sends an empty map as nil; the dynamic one sends it, as Go sends
    /// `map[string]any{}` — and both still omit a nil one.
    #[test]
    fn only_the_dynamic_answer_sends_an_empty_map() {
        let typed = Answer::<Z_GetPluginConfigReturns>::Typed(Z_GetPluginConfigReturns::default());
        let sent = decode_dynamic(&Encoder::new().encode(&typed).unwrap());
        assert_eq!(sent.value, Value::Struct(vec![None]));

        let empty: Answer<Z_GetPluginConfigReturns> =
            Answer::Dynamic(plugin_config_returns(Some(Value::Map(Vec::new()))));
        let sent = decode_dynamic(&Encoder::new().encode(&empty).unwrap());
        assert_eq!(
            sent.value,
            Value::Struct(vec![Some(Value::Map(Vec::new()))])
        );

        let nil: Answer<Z_GetPluginConfigReturns> = Answer::Dynamic(plugin_config_returns(None));
        let sent = decode_dynamic(&Encoder::new().encode(&nil).unwrap());
        assert_eq!(sent.value, Value::Struct(vec![None]));
    }
}
