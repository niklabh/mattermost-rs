//! Port of `model/property_access_control.go` — the caller identity that rides with a property
//! request.
//!
//! # The `context.Context` plumbing has no Rust counterpart
//!
//! Go stores the caller id and the acting-as scope as `context.Context` values, keyed by an
//! unexported string type, and offers `WithCallerID`/`CallerIDFromContext` and the scope pair to
//! put them in and take them out. Rust has no ambient request context: the values travel as
//! ordinary arguments, so those four functions are not ported. What is ported is the vocabulary —
//! the well-known caller ids and [`PropertyRequestOptions`] — which is the part that has to agree
//! with the Go server.
//!
//! # The `system:` prefix is a security boundary
//!
//! `IsValidPluginId` rejects `:`, so a plugin's manifest id — which is used verbatim as its
//! caller id — can never collide with one of these. That is what makes the internal sync services
//! unforgeable by a plugin.

/// Port of `model.CallerIDLDAPSync` (property_access_control.go:32).
pub const CALLER_ID_LDAP_SYNC: &str = "system:ldap_sync";
/// Port of `model.CallerIDSAMLSync` (property_access_control.go:33).
pub const CALLER_ID_SAML_SYNC: &str = "system:saml_sync";
/// Port of `model.CallerIDLocalAdmin` (property_access_control.go:34) — a local-mode session,
/// which has an **empty `Session.UserId`** but full admin privileges. Handlers tag the request
/// with it when `Session().IsUnrestricted()`, so the permission checker can grant admin without a
/// user lookup.
pub const CALLER_ID_LOCAL_ADMIN: &str = "system:local_admin";

/// Port of `model.AccessControlCallerIDContextKey` (property_access_control.go:10). Kept as a
/// constant so a caller that does thread these through a map uses the same key the Go server does.
pub const ACCESS_CONTROL_CALLER_ID_CONTEXT_KEY: &str = "access_control_caller_id";
/// Port of `model.AccessControlScopeContextKey` (property_access_control.go:15).
pub const ACCESS_CONTROL_SCOPE_CONTEXT_KEY: &str = "access_control_scope";

/// Port of `model.PropertyRequestOptions` (property_access_control.go:70).
///
/// The scope subdivides one owner's access per external system — the SCIM plugin acting as
/// `entra`, say. **Empty means the caller is not acting as any scope**, which is a different
/// thing from acting as the empty scope: `WithPropertyRequestOptions` does not set the context
/// value at all in that case.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PropertyRequestOptions {
    pub acting_as_scope: String,
}
