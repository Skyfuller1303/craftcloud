//! OIDC authorization DTOs shared by native CraftCloud clients.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Application identity and PKCE data sent to the OIDC authorization endpoint.
#[derive(Clone, Serialize, Deserialize)]
pub struct OidcStartRequest {
    pub organization_id: Uuid,
    pub desktop: OidcDesktopRequest,
}

impl std::fmt::Debug for OidcStartRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcStartRequest")
            .field("organization_id", &self.organization_id)
            .field("desktop", &self.desktop)
            .finish()
    }
}

/// The PKCE challenge and state are one-time flow values: they stay out of diagnostics.
#[derive(Clone, Serialize, Deserialize)]
pub struct OidcDesktopRequest {
    pub client_kind: String,
    pub client_id: String,
    pub device_name: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub state: String,
}

impl std::fmt::Debug for OidcDesktopRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcDesktopRequest")
            .field("client_kind", &self.client_kind)
            .field("client_id", &self.client_id)
            .field("device_name", &self.device_name)
            .field("redirect_uri", &"<redacted>")
            .field("code_challenge", &"<redacted>")
            .field("state", &"<redacted>")
            .finish()
    }
}

/// Validated authorization URL returned by the CraftCloud server. The URL embeds
/// the single-use flow capability, so `Debug` never prints it.
#[derive(Clone, Serialize, Deserialize)]
pub struct OidcAuthorization {
    pub authorization_url: String,
}

impl std::fmt::Debug for OidcAuthorization {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OidcAuthorization")
            .field("authorization_url", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_output_never_contains_the_flow_capability_or_pkce_values() {
        let authorization = OidcAuthorization {
            authorization_url: "https://cloud.example/api/v1/auth/oidc/authorize?flow=SECRETFLOW"
                .to_owned(),
        };
        let request = OidcStartRequest {
            organization_id: Uuid::nil(),
            desktop: OidcDesktopRequest {
                client_kind: "launcher".to_owned(),
                client_id: "launcher".to_owned(),
                device_name: "Studio PC".to_owned(),
                redirect_uri: "http://127.0.0.1:43121/oidc/callback".to_owned(),
                code_challenge: "SECRETCHALLENGE".to_owned(),
                state: "SECRETSTATE".to_owned(),
            },
        };
        let text = format!("{authorization:?} {request:?} {:?}", Some(&authorization));
        for secret in ["SECRETFLOW", "SECRETCHALLENGE", "SECRETSTATE", "43121"] {
            assert!(!text.contains(secret), "{secret} leaked: {text}");
        }
        assert!(text.contains("Studio PC"));
    }
}

#[derive(Serialize)]
pub(crate) struct OidcExchangeRequest<'a> {
    pub code: &'a str,
    pub code_verifier: &'a str,
}
