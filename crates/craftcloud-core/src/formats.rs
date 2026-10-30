//! Descriptive app/extension registry. File contents remain opaque bytes.

use serde::Serialize;

/// A known application and the extensions it can open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct FormatRegistration {
    pub app: &'static str,
    pub extensions: &'static [&'static str],
}

/// `generic` is the fallback for extensions absent from the registry.
pub const REGISTRY: &[FormatRegistration] = &[
    FormatRegistration {
        app: "photocraft",
        extensions: &["pcraft", "psd", "psb", "ora"],
    },
    FormatRegistration {
        app: "figcraft",
        extensions: &["figcraft", "fig"],
    },
    FormatRegistration {
        app: "vectorcraft",
        extensions: &["svg", "ai", "vcraft"],
    },
    FormatRegistration {
        app: "generic",
        extensions: &[],
    },
];

/// Suggests an application for a filename extension without parsing content.
/// Unknown extensions remain supported through `generic`.
pub fn app_for_extension(extension: &str) -> &'static str {
    let extension = extension.strip_prefix('.').unwrap_or(extension);
    REGISTRY
        .iter()
        .find(|entry| {
            entry
                .extensions
                .iter()
                .any(|known| known.eq_ignore_ascii_case(extension))
        })
        .map_or("generic", |entry| entry.app)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{FileApp, FileFormat};

    #[test]
    fn registry_uses_existing_validated_identifiers() {
        for entry in REGISTRY {
            assert!(FileApp::parse(entry.app).is_ok());
            for extension in entry.extensions {
                assert!(FileFormat::parse(extension).is_ok());
                assert_eq!(app_for_extension(extension), entry.app);
            }
        }
        assert_eq!(app_for_extension(".PSD"), "photocraft");
        assert_eq!(app_for_extension("unknown"), "generic");
        assert_eq!(app_for_extension("../svg"), "generic");
    }
}
