//! Deserialize specimens generated from the actual OpenAPI required properties.
use craftcloud_client::*;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

fn specimen(document: &Value, schema: &Value) -> Value {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        return specimen(
            document,
            &document
                .pointer(reference.trim_start_matches('#'))
                .cloned()
                .unwrap_or(Value::Null),
        );
    }
    if let Some(parts) = schema.get("allOf").and_then(Value::as_array) {
        let mut result = serde_json::Map::new();
        for part in parts {
            if let Value::Object(fields) = specimen(document, part) {
                result.extend(fields);
            }
        }
        return Value::Object(result);
    }
    if let Some(types) = schema.get("type").and_then(Value::as_array)
        && types.iter().any(|kind| kind.as_str() == Some("null"))
    {
        return Value::Null;
    }
    if let Some(value) = schema
        .get("enum")
        .and_then(Value::as_array)
        .and_then(|values| values.first())
    {
        return value.clone();
    }
    match schema.get("type").and_then(Value::as_str) {
        Some("object") => {
            let mut fields = serde_json::Map::new();
            if let Some(required) = schema.get("required").and_then(Value::as_array) {
                for name in required.iter().filter_map(Value::as_str) {
                    let property = schema
                        .get("properties")
                        .and_then(|fields| fields.get(name))
                        .unwrap_or(&Value::Null);
                    fields.insert(name.into(), specimen(document, property));
                }
            }
            Value::Object(fields)
        }
        Some("array") => json!([]),
        Some("boolean") => json!(false),
        Some("integer") => json!(1),
        Some("string") => match schema.get("format").and_then(Value::as_str) {
            Some("uuid") => json!("00000000-0000-0000-0000-000000000001"),
            Some("date-time") => json!("2026-10-30T00:00:00Z"),
            _ => json!("example"),
        },
        _ => Value::Null,
    }
}

fn check<T: DeserializeOwned>(
    document: &Value,
    name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let schema = document
        .pointer(&format!("/components/schemas/{name}"))
        .ok_or("schema missing")?;
    let _: T = serde_json::from_value(specimen(document, schema))?;
    Ok(())
}

#[test]
fn public_response_dtos_accept_required_openapi_shapes() -> Result<(), Box<dyn std::error::Error>> {
    let document: Value = serde_yaml_ng::from_str(include_str!("../../../contracts/openapi.yaml"))?;
    check::<DeviceLoginResponse>(&document, "DeviceLoginResponse")?;
    check::<DeviceTokenResponse>(&document, "DeviceTokenResponse")?;
    check::<SessionInfo>(&document, "SessionInfo")?;
    check::<Organization>(&document, "Organization")?;
    check::<Workspace>(&document, "Workspace")?;
    check::<Project>(&document, "Project")?;
    check::<File>(&document, "File")?;
    check::<Grant>(&document, "Grant")?;
    check::<Version>(&document, "Version")?;
    check::<UploadSession>(&document, "Upload")?;
    check::<FileLock>(&document, "FileLock")?;
    Ok(())
}

#[test]
fn organization_list_accepts_the_minimal_server_projection() -> Result<(), serde_json::Error> {
    let _: Organization = serde_json::from_value(
        json!({"id":"00000000-0000-0000-0000-000000000001", "slug":"demo", "name":"Demo", "role":"owner", "version":1, "created_at":"2026-10-30T00:00:00Z"}),
    )?;
    Ok(())
}
