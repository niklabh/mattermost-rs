//! Port of `model/access_control_masking.go` — per-caller visibility of attribute literals.
//!
//! # Fail closed
//!
//! [`MaskingFieldInfo::is_value_hidden`] is the single decision point shared by the masking,
//! validation and merge walkers, and its `default` arm hides the value. `Unknown` — the zero
//! value, which is what an uninitialised struct carries — therefore masks everything. Adding a
//! mode without adding an arm keeps that property; changing the fallback loses it.
//!
//! The one value that is never hidden is [`MASKING_TOKEN_VALUE`] itself: it is the server's own
//! stand-in from an earlier read, not a literal the caller supplied.

use std::collections::HashSet;

/// Port of `model.MaskingFieldAccessMode` (access_control_masking.go:6).
///
/// Go declares it as `int` with `iota`, but it carries no `json:` tag anywhere and never reaches
/// the wire, so it is a real enum here. `Unknown` is `iota`'s zero and stays the `Default`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum MaskingFieldAccessMode {
    /// The zero value. Fails closed.
    #[default]
    Unknown,
    /// Every value is visible to every caller.
    Public,
    /// The caller sees only the values they themselves hold.
    SharedOnly,
    /// Values are never visible to callers.
    SourceOnly,
}

/// Port of `model.MaskingTokenValue` (access_control_masking.go:20) — eight hyphens, written into
/// a masked CEL expression in place of one or more hidden values.
pub const MASKING_TOKEN_VALUE: &str = "--------";

/// Port of `model.MaskingFieldInfo` (access_control_masking.go:24).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MaskingFieldInfo {
    pub access: MaskingFieldAccessMode,
    /// The literals this caller may see. Populated for [`MaskingFieldAccessMode::SharedOnly`];
    /// empty for the other three, where it is never consulted.
    pub visible_values: HashSet<String>,
}

impl MaskingFieldInfo {
    /// Port of `(*MaskingFieldInfo).IsValueHidden` (access_control_masking.go:41).
    pub fn is_value_hidden(&self, lit: &str) -> bool {
        if lit == MASKING_TOKEN_VALUE {
            return false;
        }
        match self.access {
            MaskingFieldAccessMode::Public => false,
            MaskingFieldAccessMode::SourceOnly => true,
            MaskingFieldAccessMode::SharedOnly => !self.visible_values.contains(lit),
            // Go's `default:` arm — unknown modes mask.
            MaskingFieldAccessMode::Unknown => true,
        }
    }
}

/// Port of the `model.MaskingFieldResolver` interface (access_control_masking.go:64).
///
/// `field_name` is the suffix after `user.attributes.`, e.g. `department`. Implementations must
/// fail closed: any lookup that cannot be proven safe is an `Err`, and the walker masks every
/// literal for that field.
///
/// Go's return is a bare `error`, so there is no typed error to preserve here.
pub trait MaskingFieldResolver {
    fn resolve(
        &self,
        field_name: &str,
    ) -> Result<MaskingFieldInfo, Box<dyn std::error::Error + Send + Sync>>;
}
