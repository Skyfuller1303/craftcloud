//! Run against an existing CraftCloud project:
//! CC_SERVER_URL, CC_EMAIL, CC_PASSWORD, CC_PROJECT_ID, then pass a local file path.
use std::{
    error::Error,
    fs,
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

use craftcloud_client::{CloudClient, UploadContent, dto::File};
use craftcloud_client_native::UreqTransport;
use serde::Serialize;
use uuid::Uuid;

#[derive(Serialize)]
struct CreateFile<'a> {
    name: &'a str,
    app: &'a str,
    format: &'a str,
}

fn required(name: &str) -> Result<String, Box<dyn Error>> {
    std::env::var(name).map_err(|_| format!("missing environment variable {name}").into())
}

fn main() -> Result<(), Box<dyn Error>> {
    let server = required("CC_SERVER_URL")?;
    let email = required("CC_EMAIL")?;
    let password = required("CC_PASSWORD")?;
    let project = Uuid::parse_str(&required("CC_PROJECT_ID")?)?;
    let source = std::env::args_os()
        .nth(1)
        .ok_or("usage: roundtrip <file>")?;
    let path = Path::new(&source);
    let bytes = fs::read(path)?;
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("bin");
    let epoch = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let name = format!("roundtrip-{}-{epoch}.{ext}", std::process::id());
    let client = CloudClient::new(&server, UreqTransport::default())?;
    let _ = client.discover()?;
    client.login(
        &email,
        &password,
        "cli",
        "craftcloud-sdk-roundtrip",
        "CraftCloud SDK roundtrip",
    )?;
    let request = CreateFile {
        name: &name,
        app: "generic",
        format: ext,
    };
    let file: File = client.post(&format!("/api/v1/projects/{project}/files"), &request)?;
    let upload = UploadContent {
        file_id: file.id,
        base_version: None,
        idempotency_key: format!("sdk-roundtrip-{}-{epoch}", std::process::id()),
        bytes: bytes.clone(),
    };
    let version = client.upload(&upload)?;
    let held = client.acquire_lock(file.id, "sdk-roundtrip-main")?;
    let renewed = client.acquire_lock(file.id, "sdk-roundtrip-main")?;
    if renewed.expires_at < held.expires_at {
        return Err("lease renewal did not extend expiry".into());
    }
    let second_device = CloudClient::new(&server, UreqTransport::default())?;
    second_device.login(
        &email,
        &password,
        "cli",
        "craftcloud-sdk-roundtrip-second",
        "CraftCloud SDK second device",
    )?;
    if !matches!(
        second_device.acquire_lock(file.id, "sdk-roundtrip-second"),
        Err(craftcloud_client::CloudError::FileLocked(_))
    ) {
        return Err("second device unexpectedly acquired held lease".into());
    }
    client.release_lock(file.id)?;

    let stale = client.create_upload(
        file.id,
        &bytes,
        &format!("sdk-stale-{}-{epoch}", std::process::id()),
    )?;
    client.upload_content(&stale, bytes.clone())?;
    let advance = client.create_upload(
        file.id,
        &bytes,
        &format!("sdk-advance-{}-{epoch}", std::process::id()),
    )?;
    client.upload_content(&advance, bytes.clone())?;
    let head = client.commit_version(&advance, Some(version.id))?;
    if !matches!(client.commit_version(&stale,Some(version.id)),Err(craftcloud_client::CloudError::VersionConflict(Some(id))) if id==head.id)
    {
        return Err("stale save did not report the current head".into());
    }

    let person = client
        .authenticated_user()
        .map(|u| u.display_name)
        .unwrap_or_else(|| email.clone());
    let person = person
        .chars()
        .take(40)
        .map(|c| {
            if "/\\:*?\"<>|".contains(c) || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect::<String>();
    let date = time::OffsetDateTime::now_utc().date().to_string();
    let stem = Path::new(&name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Craft file");
    let copy_name = format!("{stem} (Kopie von {person}, {date}).{ext}");
    let copy_request = CreateFile {
        name: &copy_name,
        app: "generic",
        format: ext,
    };
    let copy: File = client.post(&format!("/api/v1/projects/{project}/files"), &copy_request)?;
    let keep_both = client.create_upload(
        copy.id,
        &bytes,
        &format!("sdk-copy-{}-{epoch}", std::process::id()),
    )?;
    client.upload_content(&keep_both, bytes.clone())?;
    let copy_version = client.commit_version(&keep_both, None)?;
    let downloaded = client.download(copy.id, Some(copy_version.id))?;
    if downloaded != bytes {
        return Err("roundtrip bytes differ".into());
    }
    second_device.logout()?;
    client.logout()?;
    println!(
        "Roundtrip, lease and keep-both verified: file {} copy {} head {} ({} bytes)",
        file.id,
        copy.id,
        head.id,
        bytes.len()
    );
    Ok(())
}
