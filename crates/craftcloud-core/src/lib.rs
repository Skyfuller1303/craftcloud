//! CraftCloud domain types and deterministic validation.
//!
//! This crate performs no IO and has no HTTP, database or runtime
//! dependencies; it only uses `serde` for the wire representation.

mod capabilities;

pub use capabilities::{
    API_VERSION, Capabilities, FEATURE_ID_MAX_LEN, FeatureId, FeatureIdError, ServerMode,
};
