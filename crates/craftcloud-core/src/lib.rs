//! CraftCloud domain types and deterministic validation.
//!
//! This crate performs no IO and has no HTTP, database or runtime
//! dependencies; it only uses `serde` for the wire representation.

mod capabilities;
mod identity;

pub use capabilities::{
    API_VERSION, Capabilities, FEATURE_ID_MAX_LEN, FeatureId, FeatureIdError, ServerMode,
};
pub use identity::{
    DISPLAY_NAME_MAX_CHARS, DisplayName, DisplayNameError, EMAIL_MAX_CHARS, EmailError,
    NormalizedEmail, SLUG_MAX_CHARS, Slug, SlugError,
};
