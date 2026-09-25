//! The hook recorder's property script: the plugin API's group, field and value methods and the
//! five `*WithOptions` variants, each written down with what the host answered, for
//! `parity::plugin_hooks`' property tranche (docs/PLUGIN_PLAN.md, Phase 6).
//!
//! It runs when a post's message is [`PROPERTIES_SCRIPT`], from inside `MessageWillBePosted`.
//!
//! # What the suite hands it
//!
//! Through the environment: this side's **own** user and the **side** tag. Everything it makes
//! lives in a group of its own, `prop_<side>`, a PSAv1 group as a plugin registers one — the
//! groups the property hooks manage are not touched (the Rust host answers those not
//! implemented, D-1331).
//!
//! Then the eight access-control methods, which read two policy rows the suite plants under
//! [`OWN_POLICY`] and [`OTHER_POLICY`].

use go_netrpc::Client;
use mm_plugin::wire::model::{
    PropertyField, PropertyFieldSearchOpts, PropertyRequestOptions, PropertyValue,
    PropertyValueSearchOpts,
};
use mm_plugin::wire::plugin::*;
use serde_json::Value as Json;

use crate::core::{MISSING, call};

/// The message that runs the script.
pub const PROPERTIES_SCRIPT: &str = "!properties-script";

/// What the suite put in the environment.
pub struct Inputs {
    pub own: String,
    pub side: String,
}

impl Inputs {
    pub fn from_env() -> Self {
        let var = |name: &str| std::env::var(name).unwrap_or_default();
        Self {
            own: var("HOOK_RECORDER_PROPS_OWN"),
            side: var("HOOK_RECORDER_PROPS_SIDE"),
        }
    }

    fn group(&self) -> String {
        format!("prop_{}", self.side)
    }
}

fn attr(value: &str) -> Option<gobwire::Interface> {
    mm_plugin::rpc::json_to_interface(&serde_json::from_str::<Json>(value).unwrap_or(Json::Null))
}

/// Groups: the deprecated name both ways, a miss, an invalid name, a registration twice, reads.
/// Returns this side's group id.
async fn groups(api: &Client, input: &Inputs, out: &mut Vec<Json>) -> String {
    let _: Option<Z_GetPropertyGroupReturns> = call(
        api,
        out,
        "GetPropertyGroup",
        Z_GetPropertyGroupArgs {
            a: "custom_profile_attributes".into(),
        },
    )
    .await;
    for name in [
        "custom_profile_attributes".to_owned(),
        "Bad Name".to_owned(),
        input.group(),
        input.group(),
    ] {
        let _: Option<Z_RegisterPropertyGroupReturns> = call(
            api,
            out,
            "RegisterPropertyGroup",
            Z_RegisterPropertyGroupArgs { a: name },
        )
        .await;
    }
    let mut id = String::new();
    for name in [input.group(), "missing_group".to_owned()] {
        let got: Option<Z_GetPropertyGroupReturns> = call(
            api,
            out,
            "GetPropertyGroup",
            Z_GetPropertyGroupArgs { a: name },
        )
        .await;
        if id.is_empty() {
            id = got.and_then(|g| g.a).map(|g| g.id).unwrap_or_default();
        }
    }
    id
}

fn field(group: &str, name: &str, target_type: &str, target_id: &str) -> PropertyField {
    PropertyField {
        group_id: group.to_owned(),
        name: name.to_owned(),
        r#type: "text".into(),
        target_type: target_type.to_owned(),
        target_id: target_id.to_owned(),
        ..PropertyField::default()
    }
}

/// Fields: two made, six refused, then every read. Returns the two ids.
async fn fields(api: &Client, input: &Inputs, group: &str, out: &mut Vec<Json>) -> [String; 2] {
    let mut colour = field(group, "  colour  ", "user", &input.own);
    colour.attrs = [
        ("label".to_owned(), attr(r#""Colour""#)),
        ("weight".to_owned(), attr("2.5")),
        ("tags".to_owned(), attr(r#"["a", "b"]"#)),
        ("on".to_owned(), attr("true")),
    ]
    .into_iter()
    .collect();
    let size = field(group, "size", "post", &input.own);
    let mut with_id = field(group, "withid", "user", &input.own);
    with_id.id = MISSING.into();
    let mut protected = field(group, "guarded", "user", &input.own);
    protected.protected = true;
    let mut v2 = field(group, "vtwo", "system", "");
    v2.object_type = "user".into();
    let lost = field(MISSING, "lost", "user", &input.own);
    let blank = field(group, "   ", "user", &input.own);
    let mut ids = Vec::new();
    for body in [
        None,
        Some(colour),
        Some(size.clone()),
        Some(size),
        Some(with_id),
        Some(protected),
        Some(v2),
        Some(lost),
        Some(blank),
    ] {
        let made: Option<Z_CreatePropertyFieldReturns> = call(
            api,
            out,
            "CreatePropertyField",
            Z_CreatePropertyFieldArgs {
                a: body.map(Box::new),
            },
        )
        .await;
        if let Some(id) = made.and_then(|m| m.a).map(|f| f.id) {
            ids.push(id);
        }
    }
    let colour = ids.first().cloned().unwrap_or_default();
    let size = ids.get(1).cloned().unwrap_or_default();

    for (g, id) in [
        (group, colour.as_str()),
        ("", colour.as_str()),
        (group, MISSING),
        (MISSING, colour.as_str()),
    ] {
        let _: Option<Z_GetPropertyFieldReturns> = call(
            api,
            out,
            "GetPropertyField",
            Z_GetPropertyFieldArgs {
                a: g.to_owned(),
                b: id.to_owned(),
            },
        )
        .await;
    }
    for list in [
        vec![colour.clone()],
        vec![colour.clone(), MISSING.to_owned()],
        vec![],
    ] {
        let _: Option<Z_GetPropertyFieldsReturns> = call(
            api,
            out,
            "GetPropertyFields",
            Z_GetPropertyFieldsArgs {
                a: group.to_owned(),
                b: list,
            },
        )
        .await;
    }
    for name in ["colour", "nope"] {
        let _: Option<Z_GetPropertyFieldByNameReturns> = call(
            api,
            out,
            "GetPropertyFieldByName",
            Z_GetPropertyFieldByNameArgs {
                a: group.to_owned(),
                b: input.own.clone(),
                c: name.to_owned(),
            },
        )
        .await;
    }
    for opts in [
        PropertyFieldSearchOpts {
            // Replaced by the group argument.
            group_id: MISSING.into(),
            target_type: "user".into(),
            target_i_ds: vec![input.own.clone()],
            per_page: 10,
            ..PropertyFieldSearchOpts::default()
        },
        PropertyFieldSearchOpts::default(),
        PropertyFieldSearchOpts {
            object_type: "bogus".into(),
            per_page: 10,
            ..PropertyFieldSearchOpts::default()
        },
    ] {
        let _: Option<Z_SearchPropertyFieldsReturns> = call(
            api,
            out,
            "SearchPropertyFields",
            Z_SearchPropertyFieldsArgs {
                a: group.to_owned(),
                b: opts,
            },
        )
        .await;
    }
    for deleted in [false, true] {
        let _: Option<Z_CountPropertyFieldsReturns> = call(
            api,
            out,
            "CountPropertyFields",
            Z_CountPropertyFieldsArgs {
                a: group.to_owned(),
                b: deleted,
            },
        )
        .await;
    }
    for target_type in ["user", "channel"] {
        let _: Option<Z_CountPropertyFieldsForTargetReturns> = call(
            api,
            out,
            "CountPropertyFieldsForTarget",
            Z_CountPropertyFieldsForTargetArgs {
                a: group.to_owned(),
                b: target_type.to_owned(),
                c: input.own.clone(),
                d: false,
            },
        )
        .await;
    }
    [colour, size]
}

fn value(group: &str, field: &str, target: &str, json: &str) -> PropertyValue {
    PropertyValue {
        target_id: target.to_owned(),
        target_type: "user".into(),
        group_id: group.to_owned(),
        field_id: field.to_owned(),
        value: json.as_bytes().to_vec(),
        ..PropertyValue::default()
    }
}

/// Values: made, read, searched, updated (with a `CreateAt`, which `IsValid` requires of an
/// update), upserted. Returns the two made values' ids.
async fn values(
    api: &Client,
    input: &Inputs,
    group: &str,
    [colour, size]: &[String; 2],
    out: &mut Vec<Json>,
) -> [String; 2] {
    let own = input.own.as_str();
    let mut with_id = value(group, colour, own, r#""x""#);
    with_id.id = MISSING.into();
    let mut untyped = value(group, colour, MISSING, r#""x""#);
    untyped.target_type.clear();
    let mut ids = Vec::new();
    for body in [
        None,
        Some(value(group, colour, own, r#""  <blue>  ""#)),
        Some(value(group, size, own, r#"[" a ", "", "b"]"#)),
        Some(value(group, colour, own, r#""again""#)),
        Some(value(group, MISSING, own, r#""x""#)),
        Some(with_id),
        Some(untyped),
    ] {
        let made: Option<Z_CreatePropertyValueReturns> = call(
            api,
            out,
            "CreatePropertyValue",
            Z_CreatePropertyValueArgs {
                a: body.map(Box::new),
            },
        )
        .await;
        if let Some(id) = made.and_then(|m| m.a).map(|v| v.id) {
            ids.push(id);
        }
    }
    let first = ids.first().cloned().unwrap_or_default();
    let second = ids.get(1).cloned().unwrap_or_default();

    for (g, id) in [
        (group, first.as_str()),
        ("", first.as_str()),
        (group, MISSING),
    ] {
        let _: Option<Z_GetPropertyValueReturns> = call(
            api,
            out,
            "GetPropertyValue",
            Z_GetPropertyValueArgs {
                a: g.to_owned(),
                b: id.to_owned(),
            },
        )
        .await;
    }
    for list in [vec![first.clone()], vec![first.clone(), MISSING.to_owned()]] {
        let _: Option<Z_GetPropertyValuesReturns> = call(
            api,
            out,
            "GetPropertyValues",
            Z_GetPropertyValuesArgs {
                a: group.to_owned(),
                b: list,
            },
        )
        .await;
    }
    for opts in [
        PropertyValueSearchOpts {
            target_i_ds: vec![own.to_owned()],
            field_id: colour.clone(),
            per_page: 10,
            ..PropertyValueSearchOpts::default()
        },
        PropertyValueSearchOpts {
            field_id: size.clone(),
            value: br#"["a","b"]"#.to_vec(),
            per_page: 10,
            ..PropertyValueSearchOpts::default()
        },
        PropertyValueSearchOpts::default(),
    ] {
        let _: Option<Z_SearchPropertyValuesReturns> = call(
            api,
            out,
            "SearchPropertyValues",
            Z_SearchPropertyValuesArgs {
                a: group.to_owned(),
                b: opts,
            },
        )
        .await;
    }

    // Updates hand back what they were given, sanitised; a missing id is the store's 500.
    let mut changed = value(group, colour, own, r#"{"k":  [1, 2]}"#);
    changed.id.clone_from(&first);
    changed.create_at = 1;
    let mut missing = value(group, colour, own, r#""x""#);
    missing.id = MISSING.into();
    missing.create_at = 1;
    for body in [None, Some(changed.clone()), Some(missing)] {
        let _: Option<Z_UpdatePropertyValueReturns> = call(
            api,
            out,
            "UpdatePropertyValue",
            Z_UpdatePropertyValueArgs {
                a: group.to_owned(),
                b: body.map(Box::new),
            },
        )
        .await;
    }
    let mut trimmed = value(group, size, own, r#"["  c  "]"#);
    trimmed.id.clone_from(&second);
    trimmed.create_at = 1;
    for list in [vec![], vec![changed, trimmed]] {
        let _: Option<Z_UpdatePropertyValuesReturns> = call(
            api,
            out,
            "UpdatePropertyValues",
            Z_UpdatePropertyValuesArgs {
                a: group.to_owned(),
                b: list,
            },
        )
        .await;
    }

    // Upserts hand back the row as stored.
    for body in [None, Some(value(group, colour, own, r#""  green ""#))] {
        let _: Option<Z_UpsertPropertyValueReturns> = call(
            api,
            out,
            "UpsertPropertyValue",
            Z_UpsertPropertyValueArgs {
                a: body.map(Box::new),
            },
        )
        .await;
    }
    let _: Option<Z_UpsertPropertyValueWithOptionsReturns> = call(
        api,
        out,
        "UpsertPropertyValueWithOptions",
        Z_UpsertPropertyValueWithOptionsArgs {
            a: Some(Box::new(value(group, size, own, r#"{"b": 1, "a": 2}"#))),
            b: PropertyRequestOptions {
                acting_as_scope: "scope".into(),
            },
        },
    )
    .await;
    let mut other_group = value(group, size, own, "1");
    other_group.group_id = MISSING.into();
    for list in [
        vec![],
        vec![value(group, "short", own, "1")],
        vec![
            value(group, colour, own, "1"),
            value(group, colour, own, "2"),
        ],
        vec![value(group, colour, own, "1"), other_group],
        vec![
            value(group, colour, own, r#""red""#),
            value(group, size, own, "[ ]"),
        ],
    ] {
        let _: Option<Z_UpsertPropertyValuesReturns> = call(
            api,
            out,
            "UpsertPropertyValues",
            Z_UpsertPropertyValuesArgs { a: list },
        )
        .await;
    }
    let _: Option<Z_UpsertPropertyValuesWithOptionsReturns> = call(
        api,
        out,
        "UpsertPropertyValuesWithOptions",
        Z_UpsertPropertyValuesWithOptionsArgs {
            a: vec![value(group, colour, own, "3.25")],
            b: PropertyRequestOptions::default(),
        },
    )
    .await;
    [first, second]
}

/// Field updates: one renamed, none, one missing, and a type change that clears the field's
/// values. Then the deletes, the field's twice.
async fn updates_and_deletes(
    api: &Client,
    input: &Inputs,
    group: &str,
    [colour, size]: &[String; 2],
    [first, second]: &[String; 2],
    out: &mut Vec<Json>,
) {
    let own = input.own.as_str();
    let current: Option<Z_GetPropertyFieldReturns> = call(
        api,
        out,
        "GetPropertyField",
        Z_GetPropertyFieldArgs {
            a: group.to_owned(),
            b: colour.clone(),
        },
    )
    .await;
    let mut renamed = current.and_then(|c| c.a).map(|f| *f).unwrap_or_default();
    renamed.name = " hue ".into();
    renamed.attrs.remove("tags");
    let mut missing = renamed.clone();
    missing.id = MISSING.into();
    // An empty group reads the field from any group, then fails the version check's
    // `GroupByID("")`.
    for (g, body) in [(group, renamed.clone()), (group, missing), ("", renamed)] {
        let _: Option<Z_UpdatePropertyFieldReturns> = call(
            api,
            out,
            "UpdatePropertyField",
            Z_UpdatePropertyFieldArgs {
                a: g.to_owned(),
                b: Some(Box::new(body)),
            },
        )
        .await;
    }
    let current: Option<Z_GetPropertyFieldReturns> = call(
        api,
        out,
        "GetPropertyField",
        Z_GetPropertyFieldArgs {
            a: group.to_owned(),
            b: size.clone(),
        },
    )
    .await;
    let mut retyped = current.and_then(|c| c.a).map(|f| *f).unwrap_or_default();
    retyped.r#type = "date".into();
    for list in [vec![], vec![retyped]] {
        let _: Option<Z_UpdatePropertyFieldsReturns> = call(
            api,
            out,
            "UpdatePropertyFields",
            Z_UpdatePropertyFieldsArgs {
                a: group.to_owned(),
                b: list,
            },
        )
        .await;
    }
    let _: Option<Z_GetPropertyValueReturns> = call(
        api,
        out,
        "GetPropertyValue",
        Z_GetPropertyValueArgs {
            a: group.to_owned(),
            b: second.clone(),
        },
    )
    .await;

    for id in [second.as_str(), MISSING] {
        let _: Option<Z_DeletePropertyValueReturns> = call(
            api,
            out,
            "DeletePropertyValue",
            Z_DeletePropertyValueArgs {
                a: group.to_owned(),
                b: id.to_owned(),
            },
        )
        .await;
    }
    let _: Option<Z_DeletePropertyValueWithOptionsReturns> = call(
        api,
        out,
        "DeletePropertyValueWithOptions",
        Z_DeletePropertyValueWithOptionsArgs {
            a: group.to_owned(),
            b: first.clone(),
            c: PropertyRequestOptions::default(),
        },
    )
    .await;
    let _: Option<Z_CreatePropertyValueReturns> = call(
        api,
        out,
        "CreatePropertyValue",
        Z_CreatePropertyValueArgs {
            a: Some(Box::new(value(group, colour, own, r#""late""#))),
        },
    )
    .await;
    for (target_type, target) in [("user", own), ("", own)] {
        let _: Option<Z_DeletePropertyValuesForTargetReturns> = call(
            api,
            out,
            "DeletePropertyValuesForTarget",
            Z_DeletePropertyValuesForTargetArgs {
                a: group.to_owned(),
                b: target_type.to_owned(),
                c: target.to_owned(),
            },
        )
        .await;
    }
    let _: Option<Z_DeletePropertyValuesForTargetWithOptionsReturns> = call(
        api,
        out,
        "DeletePropertyValuesForTargetWithOptions",
        Z_DeletePropertyValuesForTargetWithOptionsArgs {
            a: group.to_owned(),
            b: "channel".into(),
            c: MISSING.into(),
            d: PropertyRequestOptions::default(),
        },
    )
    .await;
    let _: Option<Z_DeletePropertyValuesForFieldReturns> = call(
        api,
        out,
        "DeletePropertyValuesForField",
        Z_DeletePropertyValuesForFieldArgs {
            a: group.to_owned(),
            b: colour.clone(),
        },
    )
    .await;
    let _: Option<Z_DeletePropertyValuesForFieldWithOptionsReturns> = call(
        api,
        out,
        "DeletePropertyValuesForFieldWithOptions",
        Z_DeletePropertyValuesForFieldWithOptionsArgs {
            a: group.to_owned(),
            b: MISSING.into(),
            c: PropertyRequestOptions::default(),
        },
    )
    .await;
    let _: Option<Z_GetPropertyValuesReturns> = call(
        api,
        out,
        "GetPropertyValues",
        Z_GetPropertyValuesArgs {
            a: group.to_owned(),
            b: vec![first.clone()],
        },
    )
    .await;

    for (g, id) in [
        (group, size.as_str()),
        (group, size.as_str()),
        (group, MISSING),
        ("", colour.as_str()),
    ] {
        let _: Option<Z_DeletePropertyFieldReturns> = call(
            api,
            out,
            "DeletePropertyField",
            Z_DeletePropertyFieldArgs {
                a: g.to_owned(),
                b: id.to_owned(),
            },
        )
        .await;
    }
    for deleted in [false, true] {
        let _: Option<Z_CountPropertyFieldsReturns> = call(
            api,
            out,
            "CountPropertyFields",
            Z_CountPropertyFieldsArgs {
                a: group.to_owned(),
                b: deleted,
            },
        )
        .await;
    }
}

/// A policy id the suite plants with this plugin's `doc` type, and one it plants with a
/// foreign-looking type (`mmrs.hookrecorder:other`); both are shared by the two sides.
pub const OWN_POLICY: &str = "acpolicyownacpolicyownacpo";
pub const OTHER_POLICY: &str = "acpolicyothacpolicyothacpo";
const DOC: &str = "mmrs.hookrecorder:doc";

/// Access control, as Go's public build answers it: every refusal before the engine, the raw
/// existence read `EvaluateAccessControl` falls back on, and the engine's 501s.
async fn access_control(api: &Client, input: &Inputs, out: &mut Vec<Json>) {
    let own = input.own.as_str();
    for (user, resource_type, resource, action) in [
        (own, "no colon", MISSING, "view"),
        (own, "other.plugin:doc", MISSING, "view"),
        (own, DOC, MISSING, "Bad Action!"),
        (MISSING, DOC, "short", "view"),
        (own, DOC, MISSING, "view"),
        (own, DOC, OWN_POLICY, "view"),
        (own, DOC, OTHER_POLICY, "view"),
    ] {
        let _: Option<Z_EvaluateAccessControlReturns> = call(
            api,
            out,
            "EvaluateAccessControl",
            Z_EvaluateAccessControlArgs {
                a: user.to_owned(),
                b: resource_type.to_owned(),
                c: resource.to_owned(),
                d: action.to_owned(),
            },
        )
        .await;
    }
    let _: Option<Z_SaveAccessControlPolicyReturns> = call(
        api,
        out,
        "SaveAccessControlPolicy",
        Z_SaveAccessControlPolicyArgs {
            a: own.to_owned(),
            b: None,
        },
    )
    .await;
    let _: Option<Z_GetAccessControlPolicyReturns> = call(
        api,
        out,
        "GetAccessControlPolicy",
        Z_GetAccessControlPolicyArgs {
            a: OWN_POLICY.to_owned(),
        },
    )
    .await;
    let _: Option<Z_DeleteAccessControlPolicyReturns> = call(
        api,
        out,
        "DeleteAccessControlPolicy",
        Z_DeleteAccessControlPolicyArgs {
            a: own.to_owned(),
            b: DOC.to_owned(),
            c: OWN_POLICY.to_owned(),
        },
    )
    .await;
    for (user, resource_type) in [
        (own, "no colon"),
        (own, "other.plugin:doc"),
        ("short", DOC),
        (MISSING, DOC),
        (own, DOC),
    ] {
        let _: Option<Z_CheckAccessControlExpressionReturns> = call(
            api,
            out,
            "CheckAccessControlExpression",
            Z_CheckAccessControlExpressionArgs {
                a: user.to_owned(),
                b: resource_type.to_owned(),
                c: "true".to_owned(),
            },
        )
        .await;
    }
    for (user, resource_type) in [(MISSING, DOC), (own, DOC)] {
        let _: Option<Z_QueryUsersForAccessControlExpressionReturns> = call(
            api,
            out,
            "QueryUsersForAccessControlExpression",
            Z_QueryUsersForAccessControlExpressionArgs {
                a: user.to_owned(),
                b: resource_type.to_owned(),
                c: "true".to_owned(),
                d: String::new(),
                e: String::new(),
                f: 500,
            },
        )
        .await;
    }
    let _: Option<Z_GetAccessControlFieldsAutocompleteReturns> = call(
        api,
        out,
        "GetAccessControlFieldsAutocomplete",
        Z_GetAccessControlFieldsAutocompleteArgs {
            a: MISSING.to_owned(),
            b: String::new(),
            c: 0,
        },
    )
    .await;
    for (user, resource_type) in [(own, "other.plugin:doc"), (own, DOC)] {
        let _: Option<Z_GetAccessControlVisualASTReturns> = call(
            api,
            out,
            "GetAccessControlVisualAST",
            Z_GetAccessControlVisualASTArgs {
                a: user.to_owned(),
                b: resource_type.to_owned(),
                c: "true".to_owned(),
            },
        )
        .await;
    }
}

/// The whole script, in a fixed order.
pub async fn run(api: &Client, input: &Inputs) -> Vec<Json> {
    let mut out = Vec::new();
    let group = groups(api, input, &mut out).await;
    let fields = fields(api, input, &group, &mut out).await;
    let values = values(api, input, &group, &fields, &mut out).await;
    updates_and_deletes(api, input, &group, &fields, &values, &mut out).await;
    access_control(api, input, &mut out).await;
    out
}
