//! Public additive discovery. No account, storage path or secret is disclosed.

use crate::{ApiState, AuthSettings};
use axum::{Json, extract::State};
use craftcloud_core::{
    Capabilities,
    formats::{FormatRegistration, REGISTRY},
};
use serde::Serialize;

#[derive(Serialize)]
pub(crate) struct Discovery {
    #[serde(flatten)]
    capabilities: Capabilities,
    server_version: &'static str,
    feature_flags: FeatureFlags,
    limits: Limits,
    formats: &'static [FormatRegistration],
}

#[derive(Serialize)]
struct FeatureFlags {
    device_login: bool,
    two_factor: bool,
    grants: bool,
    locks: bool,
    figcraft_live: bool,
}

#[derive(Serialize)]
struct Limits {
    max_file_size: u64,
    block_size: usize,
}

pub(crate) async fn discover(State(state): State<ApiState>) -> Json<Discovery> {
    let max_file_size = state.services.as_ref().map_or_else(
        || AuthSettings::default().max_file_size_bytes,
        |services| services.settings.max_file_size_bytes,
    );
    Json(Discovery {
        capabilities: state.capabilities,
        server_version: env!("CARGO_PKG_VERSION"),
        feature_flags: FeatureFlags {
            device_login: true,
            // True only when the 2FA encryption key is loaded: without it new
            // setups and second-factor sign-ins answer 503.
            two_factor: state
                .services
                .as_ref()
                .is_some_and(|services| services.two_factor_available()),
            grants: true,
            locks: true,
            figcraft_live: state
                .services
                .as_ref()
                .is_some_and(|services| services.figcraft_available()),
        },
        limits: Limits {
            max_file_size,
            block_size: craftcloud_storage::CHUNK_BYTES,
        },
        formats: REGISTRY,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[tokio::test]
    async fn discovery_exposes_configured_limit_without_credentials() {
        let services = crate::Services::new(
            sqlx::postgres::PgPoolOptions::new()
                .connect_lazy("postgres://synthetic:synthetic@127.0.0.1:1/unused")
                .unwrap(),
            None,
            AuthSettings {
                max_file_size_bytes: 1234567,
                ..AuthSettings::default()
            },
        );
        let Json(discovery) = discover(State(ApiState {
            capabilities: Capabilities::development_defaults(),
            services: Some(Arc::new(services)),
        }))
        .await;
        let value = serde_json::to_value(discovery).unwrap();
        assert_eq!(value["limits"]["max_file_size"], 1234567);
        assert!(!value.to_string().contains("synthetic"));
        assert_eq!(value["feature_flags"]["locks"], true);
    }
}
