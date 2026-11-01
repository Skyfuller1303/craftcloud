//! OIDC authorization DTOs shared by native CraftCloud clients.
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Application identity and PKCE data sent to the OIDC authorization endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OidcStartRequest {
    pub organization_id: Uuid,
    pub desktop: OidcDesktopRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OidcDesktopRequest {
    pub client_kind: String,
    pub client_id: String,
    pub device_name: String,
    pub redirect_uri: String,
    pub code_challenge: String,
    pub state: String,
}

/// Validated authorization URL returned by the CraftCloud server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OidcAuthorization {
    pub authorization_url: String,
}

#[derive(Serialize)]
pub(crate) struct OidcExchangeRequest<'a> {
    pub code: &'a str,
    pub code_verifier: &'a str,
}
