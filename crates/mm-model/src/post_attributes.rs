//! Port of `model/post_attributes.go` (post_attributes.go:1–12).
//!
//! Two constants and nothing else. They name the property group backing the Post Attributes
//! feature, which has no ported consumer yet — the property-group types (`property_field.go`,
//! 735 lines) are unported. They land here because they are their own Go file and because a
//! transcribed constant drifts silently; both are pinned against Go by
//! `post_info::go_parity::the_post_attributes_constants_match_go`.

/// Port of `model.PostAttributesPropertyGroupName` (post_attributes.go:8).
pub const POST_ATTRIBUTES_PROPERTY_GROUP_NAME: &str = "post_attributes";

/// Port of `model.PostAttributesPropertyGroupSchemaVersion` (post_attributes.go:12).
///
/// An untyped constant in Go, so it defaults to `int` at its use sites. `i64` here for the same
/// reason `SearchParams::time_zone_offset` is: Go's `int` is 64-bit on every platform the server
/// targets.
pub const POST_ATTRIBUTES_PROPERTY_GROUP_SCHEMA_VERSION: i64 = 1;
