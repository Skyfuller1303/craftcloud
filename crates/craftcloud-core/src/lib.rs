//! CraftCloud domain types and deterministic validation.
//!
//! This crate performs no IO and has no HTTP, database or runtime
//! dependencies; it only uses `serde` for the wire representation.

mod capabilities;
pub mod formats;
mod identity;
mod names;
pub mod policy;

pub use capabilities::{
    API_VERSION, Capabilities, FEATURE_ID_MAX_LEN, FeatureId, FeatureIdError, ServerMode,
};
pub use identity::{
    DISPLAY_NAME_MAX_CHARS, DisplayName, DisplayNameError, EMAIL_MAX_CHARS, EmailError,
    NormalizedEmail, SLUG_MAX_CHARS, Slug, SlugError,
};
pub use names::{
    FILE_APP_MAX_LEN, FILE_FORMAT_MAX_LEN, FILE_NAME_MAX_BYTES, FileApp, FileFormat, FileName,
    FileNameError, IdentifierError, ProjectName, name_key,
};
