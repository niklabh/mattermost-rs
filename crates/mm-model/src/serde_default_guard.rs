//! The guard for [D-192]: every `Deserialize` struct in the server crates zero-fills an absent key.
//!
//! Go's `encoding/json` leaves a field the document does not mention at its **zero value**; a
//! serde derive without `#[serde(default)]` makes the same document a `missing field` error. The
//! divergence is a 400 where Go answers 200, it has been found three times by a write route after
//! the code was written (`Post`, `OAuthAppRequest`, `Team`), and every round-trip fixture decodes a
//! *complete* document, so no serialisation test can see it.
//!
//! So this test reads the source rather than the types: it parses every module reachable from each
//! server crate's root with `syn` (following `mod` declarations, skipping `#[cfg(test)]` ones, the
//! way the compiler does) and fails on any named-field struct that derives `Deserialize` without a
//! container-level `default`, `transparent`, `from` or `try_from`. A new type cannot be forgotten
//! the way a registry of body types could be. Enum struct variants have no container switch, so
//! each of their fields must be an `Option` or carry a field-level `default`.
//!
//! What it cannot check, and the sweep did by reading: that the `Default` the attribute calls is
//! Go's zero value rather than a "sensible" one, and the hand-written `Deserialize` impls.

use std::path::{Path, PathBuf};

/// Types that deliberately keep serde's `missing field` error, each with the reason. A key here is
/// `<crate>::<TypeName>`.
const EXEMPT: &[(&str, &str)] = &[
    (
        "mm-model::AutocompleteArgWire",
        "Go's own `(*AutocompleteArg).UnmarshalJSON` (command_autocomplete.go:306) errors on each \
         missing key, so serde's `missing field` is the parity answer",
    ),
    (
        "mm-app::Entry",
        "go-i18n's `NewTranslation` errors on a missing `id` or `translation` \
         (translation.go:37), so a missing field failing the file is Go's behaviour",
    ),
    (
        "mm-app::EcdsaKeyRow",
        "the Systems row Go writes itself; a missing `x`/`y` is a nil `*big.Int` that Go cannot \
         marshal into a public key either, and the caller answers `None` for both",
    ),
];

/// The crates whose types are decoded from something Go wrote or a client sent.
const CRATES: &[(&str, &str)] = &[
    ("mm-model", "src/lib.rs"),
    ("mm-api", "src/lib.rs"),
    ("mm-api", "src/main.rs"),
    ("mm-app", "src/lib.rs"),
    ("mm-store", "src/lib.rs"),
    ("mm-ws", "src/lib.rs"),
    ("mm-plugin", "src/lib.rs"),
];

fn is_cfg_test(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg")
            && attr.meta.require_list().is_ok_and(|list| {
                list.tokens
                    .to_string()
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .any(|word| word == "test")
            })
    })
}

fn derives_deserialize(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("derive")
            && attr.meta.require_list().is_ok_and(|list| {
                list.tokens
                    .to_string()
                    .split(',')
                    .any(|path| path.rsplit("::").next().map(str::trim) == Some("Deserialize"))
            })
    })
}

/// The leading identifier of each comma-separated entry of every `#[serde(...)]` attribute:
/// `#[serde(default, rename = "x")]` gives `["default", "rename"]`.
fn serde_keys(attrs: &[syn::Attribute]) -> Vec<String> {
    let mut keys = Vec::new();
    for attr in attrs.iter().filter(|attr| attr.path().is_ident("serde")) {
        let Ok(list) = attr.meta.require_list() else {
            continue;
        };
        let mut at_start = true;
        for tree in list.tokens.clone() {
            match tree {
                proc_macro2::TokenTree::Ident(ident) if at_start => {
                    keys.push(ident.to_string());
                    at_start = false;
                }
                proc_macro2::TokenTree::Punct(punct) if punct.as_char() == ',' => at_start = true,
                _ => at_start = false,
            }
        }
    }
    keys
}

fn is_option(ty: &syn::Type) -> bool {
    matches!(ty, syn::Type::Path(path)
        if path.path.segments.last().is_some_and(|segment| segment.ident == "Option"))
}

/// A field serde fills when its key is absent: one with a field-level `default`, or an `Option`
/// without a `deserialize_with` (serde's derive only supplies `None` for an `Option` it decodes
/// itself).
fn field_zero_fills(field: &syn::Field) -> bool {
    let keys = serde_keys(&field.attrs);
    keys.iter().any(|key| key == "default")
        || (is_option(&field.ty)
            && !keys
                .iter()
                .any(|key| key == "deserialize_with" || key == "with"))
}

fn path_attr(attrs: &[syn::Attribute]) -> Option<String> {
    attrs.iter().find_map(|attr| match &attr.meta {
        syn::Meta::NameValue(nv) if nv.path.is_ident("path") => match &nv.value {
            syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(value),
                ..
            }) => Some(value.value()),
            _ => None,
        },
        _ => None,
    })
}

struct Walk<'a> {
    krate: &'a str,
    offenders: Vec<String>,
    seen: Vec<String>,
}

impl Walk<'_> {
    fn file(&mut self, path: &Path, child_dir: &Path) {
        let source = std::fs::read_to_string(path).unwrap_or_else(|err| panic!("{path:?}: {err}"));
        let file = syn::parse_file(&source).unwrap_or_else(|err| panic!("{path:?}: {err}"));
        self.items(&file.items, path, child_dir);
    }

    fn items(&mut self, items: &[syn::Item], file: &Path, child_dir: &Path) {
        for item in items {
            match item {
                syn::Item::Mod(module) if !is_cfg_test(&module.attrs) => {
                    let name = module.ident.to_string();
                    if let Some((_, inline)) = &module.content {
                        self.items(inline, file, &child_dir.join(&name));
                        continue;
                    }
                    let base = file.parent().unwrap_or(Path::new("."));
                    let path = match path_attr(&module.attrs) {
                        Some(explicit) => base.join(explicit),
                        None if child_dir.join(format!("{name}.rs")).exists() => {
                            child_dir.join(format!("{name}.rs"))
                        }
                        None => child_dir.join(&name).join("mod.rs"),
                    };
                    let next_dir = if path.file_name().is_some_and(|f| f == "mod.rs") {
                        path.parent().map(Path::to_path_buf).unwrap_or_default()
                    } else {
                        path.with_extension("")
                    };
                    self.file(&path, &next_dir);
                }
                syn::Item::Struct(item) if derives_deserialize(&item.attrs) => {
                    let syn::Fields::Named(fields) = &item.fields else {
                        continue;
                    };
                    let keys = serde_keys(&item.attrs);
                    let zero_fills = keys.iter().any(|key| {
                        matches!(
                            key.as_str(),
                            "default" | "transparent" | "from" | "try_from"
                        )
                    }) || fields.named.iter().all(field_zero_fills);
                    self.judge(
                        zero_fills,
                        &item.ident,
                        file,
                        "no container `#[serde(default)]`",
                    );
                }
                syn::Item::Enum(item) if derives_deserialize(&item.attrs) => {
                    let keys = serde_keys(&item.attrs);
                    let zero_fills = keys
                        .iter()
                        .any(|key| matches!(key.as_str(), "from" | "try_from"))
                        || item.variants.iter().all(|variant| match &variant.fields {
                            syn::Fields::Named(fields) => fields.named.iter().all(field_zero_fills),
                            _ => true,
                        });
                    self.judge(
                        zero_fills,
                        &item.ident,
                        file,
                        "a struct variant has a required field",
                    );
                }
                _ => {}
            }
        }
    }

    fn judge(&mut self, zero_fills: bool, ident: &syn::Ident, file: &Path, why: &str) {
        let key = format!("{}::{ident}", self.krate);
        self.seen.push(key.clone());
        if !zero_fills && !EXEMPT.iter().any(|(exempt, _)| *exempt == key) {
            self.offenders
                .push(format!("{key} ({}): {why}", file.display()));
        }
    }
}

#[test]
fn every_deserialize_struct_zero_fills_an_absent_key() {
    let crates_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .unwrap();
    let mut offenders = Vec::new();
    let mut seen = Vec::new();
    for (krate, root) in CRATES {
        let root = crates_dir.join(krate).join(root);
        let mut walk = Walk {
            krate,
            offenders: Vec::new(),
            seen: Vec::new(),
        };
        let dir = root.parent().map(Path::to_path_buf).unwrap();
        walk.file(&root, &dir);
        offenders.append(&mut walk.offenders);
        seen.append(&mut walk.seen);
    }
    // A walk that found nothing proves nothing: the crate layout moved under the test. Known
    // types from four crates are the proof the walk reached each of them.
    assert!(
        seen.len() > 500,
        "only {} Deserialize types found",
        seen.len()
    );
    for known in [
        "mm-model::Post",
        "mm-api::ChannelSearch",
        "mm-app::TokenExtra",
        "mm-model::MessageAttachment",
    ] {
        assert!(
            seen.iter().any(|key| key == known),
            "{known} was not visited"
        );
    }
    // A stale exemption silently widens the hole it was granted for.
    for (key, reason) in EXEMPT {
        assert!(!reason.is_empty(), "{key} is exempt without a reason");
        assert!(
            seen.iter().any(|seen| seen == key),
            "{key} is exempt but no longer exists"
        );
    }
    assert!(
        offenders.is_empty(),
        "{} Deserialize type(s) would reject a body Go accepts (D-192):\n{}",
        offenders.len(),
        offenders.join("\n")
    );
}
