//! Hand-maintained API DTOs. Field names match `contracts/openapi.yaml`.
use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Discovery {
    pub server_version: String,
    pub api_version: String,
    pub feature_flags: FeatureFlags,
    pub limits: ServerLimits,
    pub formats: Vec<FormatInfo>,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub server_mode: Option<String>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FeatureFlags {
    pub device_login: bool,
    pub two_factor: bool,
    pub grants: bool,
    pub locks: bool,
    pub figcraft_live: bool,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ServerLimits {
    pub max_file_size: u64,
    pub block_size: u64,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FormatInfo {
    pub app: String,
    pub extensions: Vec<String>,
}
#[derive(Clone, Deserialize, Serialize)]
pub struct DeviceLoginRequest {
    pub email: String,
    pub password: String,
    pub client_kind: String,
    pub client_id: String,
    pub device_name: String,
}
impl fmt::Debug for DeviceLoginRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceLoginRequest")
            .field("email", &"***")
            .field("password", &"***")
            .field("client_kind", &self.client_kind)
            .field("client_id", &self.client_id)
            .field("device_name", &self.device_name)
            .finish()
    }
}
#[derive(Clone, Deserialize, Serialize)]
pub struct DeviceLoginResponse {
    pub access_token: String,
    pub access_expires_at: String,
    pub refresh_token: String,
    pub refresh_expires_at: String,
    pub device_session_id: Uuid,
    pub user: DeviceUser,
}
impl fmt::Debug for DeviceLoginResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceLoginResponse")
            .field("access_token", &"***")
            .field("refresh_token", &"***")
            .field("access_expires_at", &self.access_expires_at)
            .field("refresh_expires_at", &self.refresh_expires_at)
            .field("device_session_id", &self.device_session_id)
            .field("user", &self.user)
            .finish()
    }
}
#[derive(Clone, Deserialize, Serialize)]
pub struct DeviceTokenResponse {
    pub access_token: String,
    pub access_expires_at: String,
    pub refresh_token: String,
    pub refresh_expires_at: String,
    pub device_session_id: Uuid,
}
impl fmt::Debug for DeviceTokenResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceTokenResponse")
            .field("access_token", &"***")
            .field("refresh_token", &"***")
            .field("access_expires_at", &self.access_expires_at)
            .field("refresh_expires_at", &self.refresh_expires_at)
            .field("device_session_id", &self.device_session_id)
            .finish()
    }
}
#[derive(Clone, Deserialize, Serialize)]
pub struct DeviceUser {
    pub user_id: Uuid,
    pub email: String,
    pub display_name: String,
    pub instance_role: Option<String>,
}
impl fmt::Debug for DeviceUser {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceUser")
            .field("user_id", &self.user_id)
            .field("email", &"***")
            .field("display_name", &"***")
            .field("instance_role", &self.instance_role)
            .finish()
    }
}
#[derive(Clone, Deserialize, Serialize)]
pub struct SessionInfo {
    pub user_id: Uuid,
    pub email: String,
    pub display_name: String,
    pub instance_role: Option<String>,
    pub csrf_token: String,
    pub created_at: String,
    pub expires_at: String,
}
impl fmt::Debug for SessionInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionInfo")
            .field("user_id", &self.user_id)
            .field("email", &"***")
            .field("display_name", &"***")
            .field("instance_role", &self.instance_role)
            .field("csrf_token", &"***")
            .field("created_at", &self.created_at)
            .field("expires_at", &self.expires_at)
            .finish()
    }
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Organization {
    pub id: Uuid,
    pub slug: String,
    pub name: String,
    pub role: String,
    pub version: u64,
    #[serde(default)]
    pub workspace_admin_invites_enabled: Option<bool>,
    pub created_at: String,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Workspace {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub slug: String,
    pub name: String,
    pub workspace_role: Option<String>,
    pub version: u64,
    pub created_at: String,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Project {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub workspace_id: Uuid,
    pub name: String,
    pub version: u64,
    pub created_at: String,
    #[serde(default)]
    pub can_share: bool,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct File {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub workspace_id: Uuid,
    pub project_id: Uuid,
    pub name: String,
    pub app: String,
    pub format: String,
    pub size_bytes: u64,
    pub sha256: Option<String>,
    pub head_version_id: Option<Uuid>,
    pub version: u64,
    pub created_at: String,
    #[serde(default)]
    pub can_share: bool,
    #[serde(default)]
    pub can_force_lock: bool,
    #[serde(default)]
    pub lock: Option<FileLock>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VersionInfo {
    pub id: Uuid,
    pub file_id: Uuid,
    pub number: u64,
    pub size_bytes: u64,
    pub sha256: String,
    pub created_at: String,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Grant {
    pub id: Uuid,
    pub organization_id: Uuid,
    pub workspace_id: Uuid,
    pub target_kind: String,
    pub target_id: Uuid,
    pub grantee: Grantee,
    pub role: String,
    pub created_by: Uuid,
    pub created_at: String,
    pub version: u64,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Grantee {
    pub user_id: Uuid,
    pub display_name: String,
    pub email: String,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LockAcquireRequest {
    pub client_id: String,
    #[serde(default)]
    pub takeover: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next_cursor: Option<String>,
}
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FileLock {
    pub holder_display_name: String,
    pub since: String,
    pub expires_at: String,
    pub mine: bool,
    pub held_by_current_session: bool,
}
