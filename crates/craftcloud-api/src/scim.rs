//! SCIM 2.0 protocol boundary. Provisioning credentials never authenticate normal API routes.
use crate::session::AuthenticatedSession;
use crate::{ApiState, Services};
use axum::{
    Json,
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use craftcloud_core::policy::Action;
use craftcloud_core::{DisplayName, NormalizedEmail};
use craftcloud_db::{DbError, scim as db};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;
const USER_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:User";
const GROUP_SCHEMA: &str = "urn:ietf:params:scim:schemas:core:2.0:Group";
const PATCH_SCHEMA: &str = "urn:ietf:params:scim:api:messages:2.0:PatchOp";
fn reply(status: StatusCode, value: Value) -> Response {
    let mut r = (status, Json(value)).into_response();
    r.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/scim+json"),
    );
    r
}
fn error(status: StatusCode, kind: &str) -> Response {
    reply(
        status,
        json!({"schemas":["urn:ietf:params:scim:api:messages:2.0:Error"],"status":status.as_u16().to_string(),"scimType":kind,"detail":"The provisioning request could not be completed."}),
    )
}
fn failure(e: DbError) -> Response {
    match e {
        DbError::UniqueViolation { .. } => error(StatusCode::CONFLICT, "uniqueness"),
        DbError::ForeignKeyViolation { .. } | DbError::CheckViolation { .. } => {
            error(StatusCode::BAD_REQUEST, "invalidValue")
        }
        _ => error(StatusCode::INTERNAL_SERVER_ERROR, "internalError"),
    }
}
async fn begin(
    s: &Services,
    org: Uuid,
    h: &HeaderMap,
) -> Result<Transaction<'static, Postgres>, Box<Response>> {
    if h.contains_key(header::COOKIE) || h.get_all(header::AUTHORIZATION).iter().count() != 1 {
        return Err(rejected(StatusCode::UNAUTHORIZED, "invalidToken"));
    }
    let token = h
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split_once(' '))
        .filter(|(scheme, t)| {
            scheme.eq_ignore_ascii_case("bearer")
                && t.starts_with("cc_scim_")
                && t.len() <= 160
                && !t.bytes().any(|b| b.is_ascii_whitespace())
        })
        .map(|(_, v)| v);
    let Some(token) = token else {
        return Err(rejected(StatusCode::UNAUTHORIZED, "invalidToken"));
    };
    db::begin(&s.pool, org, token)
        .await
        .map_err(|e| Box::new(failure(e)))?
        .ok_or_else(|| rejected(StatusCode::UNAUTHORIZED, "invalidToken"))
}
fn location(s: &Services, org: Uuid, kind: &str, id: Uuid) -> String {
    format!(
        "{}/scim/v2/organizations/{org}/{kind}/{id}",
        s.settings
            .public_url
            .as_deref()
            .unwrap_or("")
            .trim_end_matches('/')
    )
}
fn user(s: &Services, u: &db::User) -> Value {
    json!({"schemas":[USER_SCHEMA],"id":u.id,"externalId":u.external_id,"userName":u.user_name,"displayName":u.display_name,"active":u.active,"emails":[{"value":u.user_name,"primary":true}],"meta":{"resourceType":"User","created":crate::orgs::rfc3339(u.created_at),"lastModified":crate::orgs::rfc3339(u.updated_at),"location":location(s,u.org_id,"Users",u.id)}})
}
fn group(s: &Services, g: &db::Group, members: &[Uuid]) -> Value {
    json!({"schemas":[GROUP_SCHEMA],"id":g.id,"externalId":g.external_id,"displayName":g.display_name,"members":members.iter().map(|id|json!({"value":id})).collect::<Vec<_>>(),"meta":{"resourceType":"Group","created":crate::orgs::rfc3339(g.created_at),"lastModified":crate::orgs::rfc3339(g.updated_at),"location":location(s,g.org_id,"Groups",g.id)}})
}
fn text<'a>(v: &'a Value, k: &str, max: usize) -> Result<&'a str, Box<Response>> {
    v.get(k)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= max && !s.chars().any(char::is_control))
        .ok_or_else(|| rejected(StatusCode::BAD_REQUEST, "invalidValue"))
}
fn external(v: &Value) -> Result<Option<String>, Box<Response>> {
    match v.get("externalId") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s))
            if !s.is_empty() && s.len() <= 255 && !s.chars().any(char::is_control) =>
        {
            Ok(Some(s.clone()))
        }
        _ => Err(rejected(StatusCode::BAD_REQUEST, "invalidValue")),
    }
}
fn shape(v: &Value, schema: &str, allowed: &[&str]) -> Result<(), Box<Response>> {
    let Some(o) = v.as_object() else {
        return Err(rejected(StatusCode::BAD_REQUEST, "invalidSyntax"));
    };
    if o.keys().any(|k| !allowed.contains(&k.as_str()))
        || v.get("schemas") != Some(&json!([schema]))
    {
        return Err(rejected(StatusCode::BAD_REQUEST, "invalidValue"));
    }
    Ok(())
}
fn user_input(v: &Value) -> Result<db::UserInput, Box<Response>> {
    shape(
        v,
        USER_SCHEMA,
        &[
            "schemas",
            "id",
            "meta",
            "externalId",
            "userName",
            "displayName",
            "active",
            "emails",
            "name",
            "groups",
        ],
    )?;
    if v.get("groups")
        .is_some_and(|g| g.as_array().is_none_or(|a| !a.is_empty()))
    {
        return Err(rejected(StatusCode::BAD_REQUEST, "mutability"));
    }
    let email = NormalizedEmail::parse(text(v, "userName", 320)?)
        .map_err(|_| rejected(StatusCode::BAD_REQUEST, "invalidValue"))?;
    let fallback = email.as_str().chars().take(100).collect::<String>();
    let display = v
        .get("displayName")
        .and_then(Value::as_str)
        .or_else(|| v.pointer("/name/formatted").and_then(Value::as_str))
        .unwrap_or(&fallback);
    let display = DisplayName::parse(display)
        .map_err(|_| rejected(StatusCode::BAD_REQUEST, "invalidValue"))?;
    if let Some(emails) = v.get("emails") {
        let Some(a) = emails.as_array().filter(|a| a.len() <= 10) else {
            return Err(rejected(StatusCode::BAD_REQUEST, "invalidValue"));
        };
        if a.iter().any(|e| {
            e.get("value")
                .and_then(Value::as_str)
                .is_none_or(|e| NormalizedEmail::parse(e).is_err())
        }) {
            return Err(rejected(StatusCode::BAD_REQUEST, "invalidValue"));
        }
    }
    let active = match v.get("active") {
        None => true,
        Some(Value::Bool(a)) => *a,
        _ => return Err(rejected(StatusCode::BAD_REQUEST, "invalidValue")),
    };
    Ok(db::UserInput {
        user_name: email.as_str().to_owned(),
        display_name: display.as_str().to_owned(),
        external_id: external(v)?,
        active,
    })
}
fn group_input(v: &Value) -> Result<db::GroupInput, Box<Response>> {
    shape(
        v,
        GROUP_SCHEMA,
        &[
            "schemas",
            "id",
            "meta",
            "externalId",
            "displayName",
            "members",
        ],
    )?;
    let display = DisplayName::parse(text(v, "displayName", 400)?)
        .map_err(|_| rejected(StatusCode::BAD_REQUEST, "invalidValue"))?;
    let mut members = Vec::new();
    if let Some(m) = v.get("members") {
        let Some(a) = m.as_array().filter(|a| a.len() <= 100) else {
            return Err(rejected(StatusCode::BAD_REQUEST, "invalidValue"));
        };
        for member in a {
            let id = Uuid::parse_str(text(member, "value", 36)?)
                .map_err(|_| rejected(StatusCode::BAD_REQUEST, "invalidValue"))?;
            members.push(id)
        }
    }
    members.sort();
    members.dedup();
    Ok(db::GroupInput {
        display_name: display.as_str().to_owned(),
        external_id: external(v)?,
        members,
    })
}
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ListQuery {
    filter: Option<String>,
    #[serde(rename = "startIndex")]
    start: Option<i64>,
    count: Option<i64>,
}
fn list_args(
    q: Result<Query<ListQuery>, QueryRejection>,
    field: &str,
) -> Result<(i64, i64, Option<String>), Box<Response>> {
    let Query(q) = q.map_err(|_| rejected(StatusCode::BAD_REQUEST, "invalidSyntax"))?;
    let start = q.start.unwrap_or(1);
    let count = q.count.unwrap_or(100);
    if start < 1 || count < 0 {
        return Err(rejected(StatusCode::BAD_REQUEST, "invalidValue"));
    }
    let filter = match q.filter {
        None => None,
        Some(raw) => {
            if raw.len() > 1024 {
                return Err(rejected(StatusCode::BAD_REQUEST, "invalidFilter"));
            }
            let mut parts = raw.splitn(3, ' ');
            let a = parts.next().unwrap_or("");
            let op = parts.next().unwrap_or("");
            let value = parts.next().unwrap_or("");
            if !a.eq_ignore_ascii_case(field) || !op.eq_ignore_ascii_case("eq") {
                return Err(rejected(StatusCode::BAD_REQUEST, "invalidFilter"));
            }
            Some(
                serde_json::from_str::<String>(value)
                    .map_err(|_| rejected(StatusCode::BAD_REQUEST, "invalidFilter"))?,
            )
        }
    };
    Ok((start, count.min(100), filter))
}
fn list(total: i64, start: i64, items: Vec<Value>) -> Response {
    reply(
        StatusCode::OK,
        json!({"schemas":["urn:ietf:params:scim:api:messages:2.0:ListResponse"],"totalResults":total,"startIndex":start,"itemsPerPage":items.len(),"Resources":items}),
    )
}
fn patched(mut current: Value, patch: Value, is_group: bool) -> Result<Value, Box<Response>> {
    shape(&patch, PATCH_SCHEMA, &["schemas", "Operations"])?;
    let operations = patch
        .get("Operations")
        .and_then(Value::as_array)
        .filter(|a| !a.is_empty() && a.len() <= 100)
        .ok_or_else(|| rejected(StatusCode::BAD_REQUEST, "invalidSyntax"))?;
    for op in operations {
        let action = text(op, "op", 16)?.to_ascii_lowercase();
        if !["add", "remove", "replace"].contains(&action.as_str()) {
            return Err(rejected(StatusCode::BAD_REQUEST, "invalidSyntax"));
        }
        let path = op.get("path").and_then(Value::as_str).unwrap_or("");
        let value = op.get("value");
        if path.is_empty() {
            if action == "remove" {
                return Err(rejected(StatusCode::BAD_REQUEST, "noTarget"));
            }
            let Some(values) = value.and_then(Value::as_object) else {
                return Err(rejected(StatusCode::BAD_REQUEST, "invalidValue"));
            };
            for (k, v) in values {
                let update = json!({"schemas":[PATCH_SCHEMA],"Operations":[{"op":action,"path":k,"value":v}]});
                current = patched(current, update, is_group)?;
            }
            continue;
        }
        let lower = path.to_ascii_lowercase();
        if is_group && lower.starts_with("members[value eq ") && lower.ends_with(']') {
            if action != "remove" {
                return Err(rejected(StatusCode::BAD_REQUEST, "invalidPath"));
            }
            let id: Uuid = serde_json::from_str(path.get(17..path.len() - 1).unwrap_or(""))
                .map_err(|_| rejected(StatusCode::BAD_REQUEST, "invalidPath"))?;
            let a = current
                .get_mut("members")
                .and_then(Value::as_array_mut)
                .ok_or_else(|| rejected(StatusCode::BAD_REQUEST, "noTarget"))?;
            a.retain(|m| m.get("value").and_then(Value::as_str) != Some(id.to_string().as_str()));
            continue;
        }
        let key = match lower.as_str() {
            "externalid" => "externalId",
            "displayname" => "displayName",
            "username" if !is_group => "userName",
            "active" if !is_group => "active",
            "name.formatted" if !is_group => "displayName",
            "members" if is_group => "members",
            _ => return Err(rejected(StatusCode::BAD_REQUEST, "invalidPath")),
        };
        if action == "remove" {
            match key {
                "externalId" => set_value(&mut current, key, Value::Null)?,
                "members" => set_value(&mut current, key, json!([]))?,
                "displayName" if !is_group => {
                    current
                        .as_object_mut()
                        .ok_or_else(|| rejected(StatusCode::BAD_REQUEST, "invalidValue"))?
                        .remove(key);
                }
                _ => return Err(rejected(StatusCode::BAD_REQUEST, "mutability")),
            }
        } else {
            let value = value.ok_or_else(|| rejected(StatusCode::BAD_REQUEST, "invalidValue"))?;
            if key == "members" && action == "add" {
                let incoming = value
                    .as_array()
                    .ok_or_else(|| rejected(StatusCode::BAD_REQUEST, "invalidValue"))?;
                let a = current
                    .get_mut(key)
                    .and_then(Value::as_array_mut)
                    .ok_or_else(|| rejected(StatusCode::BAD_REQUEST, "invalidValue"))?;
                if a.len() + incoming.len() > 100 {
                    return Err(rejected(StatusCode::BAD_REQUEST, "tooMany"));
                }
                a.extend(incoming.iter().cloned());
            } else {
                set_value(&mut current, key, value.clone())?
            }
        }
    }
    Ok(current)
}

pub(crate) async fn list_users(
    State(state): State<ApiState>,
    Path(org): Path<Uuid>,
    headers: HeaderMap,
    q: Result<Query<ListQuery>, QueryRejection>,
) -> Response {
    let Some(s) = state.services else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "unavailable");
    };
    let mut tx = match begin(&s, org, &headers).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    let (start, count, filter) = match list_args(q, "userName") {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let response = match db::list_users(&mut tx, org, filter.as_deref(), start, count).await {
        Ok((n, items)) => list(n, start, items.iter().map(|u| user(&s, u)).collect()),
        Err(e) => failure(e),
    };
    finish_read(tx, response, org, &headers, "scim.users_listed").await
}
pub(crate) async fn get_user(
    State(state): State<ApiState>,
    Path((org, id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
) -> Response {
    let Some(s) = state.services else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "unavailable");
    };
    let mut tx = match begin(&s, org, &headers).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    let response = match db::get_user(&mut tx, org, id).await {
        Ok(Some(u)) => reply(StatusCode::OK, user(&s, &u)),
        Ok(None) => error(StatusCode::NOT_FOUND, "notFound"),
        Err(e) => failure(e),
    };
    finish_read(tx, response, org, &headers, "scim.user_read").await
}
pub(crate) async fn create_user(
    State(state): State<ApiState>,
    Path(org): Path<Uuid>,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let Some(s) = state.services else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "unavailable");
    };
    let mut tx = match begin(&s, org, &headers).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    let input = match body
        .map_err(|_| rejected(StatusCode::BAD_REQUEST, "invalidSyntax"))
        .and_then(|Json(v)| user_input(&v))
    {
        Ok(v) => v,
        Err(r) => return *r,
    };
    match db::create_user(
        &mut tx,
        org,
        &input,
        s.clock.now(),
        &crate::audit::ctx(&headers, None),
    )
    .await
    {
        Ok(u) => {
            if let Err(e) = tx.commit().await {
                return failure(e.into());
            }
            let mut r = reply(StatusCode::CREATED, user(&s, &u));
            if let Ok(v) = HeaderValue::from_str(&location(&s, org, "Users", u.id)) {
                r.headers_mut().insert(header::LOCATION, v);
            }
            r
        }
        Err(e) => failure(e),
    }
}
async fn update_user_inner(
    state: ApiState,
    org: Uuid,
    id: Uuid,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
    patch: bool,
) -> Response {
    let Some(s) = state.services else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "unavailable");
    };
    let mut tx = match begin(&s, org, &headers).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    let current = match db::get_user(&mut tx, org, id).await {
        Ok(Some(u)) => u,
        Ok(None) => return error(StatusCode::NOT_FOUND, "notFound"),
        Err(e) => return failure(e),
    };
    let mut v = match body {
        Ok(Json(v)) => v,
        Err(_) => return error(StatusCode::BAD_REQUEST, "invalidSyntax"),
    };
    if patch {
        v = match patched(user(&s, &current), v, false) {
            Ok(v) => v,
            Err(r) => return *r,
        }
    }
    if v.get("id")
        .is_some_and(|v| v.as_str() != Some(id.to_string().as_str()))
    {
        return error(StatusCode::BAD_REQUEST, "mutability");
    }
    let input = match user_input(&v) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    match db::replace_user(
        &mut tx,
        org,
        id,
        &input,
        s.clock.now(),
        &crate::audit::ctx(&headers, None),
    )
    .await
    {
        Ok(Some(u)) => {
            if let Err(e) = tx.commit().await {
                return failure(e.into());
            }
            reply(StatusCode::OK, user(&s, &u))
        }
        Ok(None) => error(StatusCode::NOT_FOUND, "notFound"),
        Err(e) => failure(e),
    }
}
pub(crate) async fn put_user(
    State(state): State<ApiState>,
    Path((org, id)): Path<(Uuid, Uuid)>,
    h: HeaderMap,
    b: Result<Json<Value>, JsonRejection>,
) -> Response {
    update_user_inner(state, org, id, h, b, false).await
}
pub(crate) async fn patch_user(
    State(state): State<ApiState>,
    Path((org, id)): Path<(Uuid, Uuid)>,
    h: HeaderMap,
    b: Result<Json<Value>, JsonRejection>,
) -> Response {
    update_user_inner(state, org, id, h, b, true).await
}
pub(crate) async fn delete_user(
    State(state): State<ApiState>,
    Path((org, id)): Path<(Uuid, Uuid)>,
    h: HeaderMap,
) -> Response {
    let Some(s) = state.services else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "unavailable");
    };
    let mut tx = match begin(&s, org, &h).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    match db::delete_user(
        &mut tx,
        org,
        id,
        s.clock.now(),
        &crate::audit::ctx(&h, None),
    )
    .await
    {
        Ok(true) => match tx.commit().await {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(e) => failure(e.into()),
        },
        Ok(false) => error(StatusCode::NOT_FOUND, "notFound"),
        Err(e) => failure(e),
    }
}

pub(crate) async fn list_groups(
    State(state): State<ApiState>,
    Path(org): Path<Uuid>,
    headers: HeaderMap,
    q: Result<Query<ListQuery>, QueryRejection>,
) -> Response {
    let Some(s) = state.services else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "unavailable");
    };
    let mut tx = match begin(&s, org, &headers).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    let (start, count, filter) = match list_args(q, "displayName") {
        Ok(v) => v,
        Err(r) => return *r,
    };
    let response = match db::list_groups(&mut tx, org, filter.as_deref(), start, count).await {
        Ok((n, items)) => list(
            n,
            start,
            items.iter().map(|(g, m)| group(&s, g, m)).collect(),
        ),
        Err(e) => failure(e),
    };
    finish_read(tx, response, org, &headers, "scim.groups_listed").await
}
pub(crate) async fn get_group(
    State(state): State<ApiState>,
    Path((org, id)): Path<(Uuid, Uuid)>,
    headers: HeaderMap,
) -> Response {
    let Some(s) = state.services else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "unavailable");
    };
    let mut tx = match begin(&s, org, &headers).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    let response = match db::get_group(&mut tx, org, id).await {
        Ok(Some(u)) => reply(StatusCode::OK, group(&s, &u.0, &u.1)),
        Ok(None) => error(StatusCode::NOT_FOUND, "notFound"),
        Err(e) => failure(e),
    };
    finish_read(tx, response, org, &headers, "scim.group_read").await
}
pub(crate) async fn create_group(
    State(state): State<ApiState>,
    Path(org): Path<Uuid>,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> Response {
    let Some(s) = state.services else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "unavailable");
    };
    let mut tx = match begin(&s, org, &headers).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    let input = match body
        .map_err(|_| rejected(StatusCode::BAD_REQUEST, "invalidSyntax"))
        .and_then(|Json(v)| group_input(&v))
    {
        Ok(v) => v,
        Err(r) => return *r,
    };
    match db::create_group(
        &mut tx,
        org,
        &input,
        s.clock.now(),
        &crate::audit::ctx(&headers, None),
    )
    .await
    {
        Ok(u) => {
            if let Err(e) = tx.commit().await {
                return failure(e.into());
            }
            let mut r = reply(StatusCode::CREATED, group(&s, &u.0, &u.1));
            if let Ok(v) = HeaderValue::from_str(&location(&s, org, "Groups", u.0.id)) {
                r.headers_mut().insert(header::LOCATION, v);
            }
            r
        }
        Err(e) => failure(e),
    }
}
async fn update_group_inner(
    state: ApiState,
    org: Uuid,
    id: Uuid,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
    patch: bool,
) -> Response {
    let Some(s) = state.services else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "unavailable");
    };
    let mut tx = match begin(&s, org, &headers).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    let current = match db::get_group(&mut tx, org, id).await {
        Ok(Some(u)) => u,
        Ok(None) => return error(StatusCode::NOT_FOUND, "notFound"),
        Err(e) => return failure(e),
    };
    let mut v = match body {
        Ok(Json(v)) => v,
        Err(_) => return error(StatusCode::BAD_REQUEST, "invalidSyntax"),
    };
    if patch {
        v = match patched(group(&s, &current.0, &current.1), v, true) {
            Ok(v) => v,
            Err(r) => return *r,
        }
    }
    if v.get("id")
        .is_some_and(|v| v.as_str() != Some(id.to_string().as_str()))
    {
        return error(StatusCode::BAD_REQUEST, "mutability");
    }
    let input = match group_input(&v) {
        Ok(v) => v,
        Err(r) => return *r,
    };
    match db::replace_group(
        &mut tx,
        org,
        id,
        &input,
        s.clock.now(),
        &crate::audit::ctx(&headers, None),
    )
    .await
    {
        Ok(Some(u)) => {
            if let Err(e) = tx.commit().await {
                return failure(e.into());
            }
            reply(StatusCode::OK, group(&s, &u.0, &u.1))
        }
        Ok(None) => error(StatusCode::NOT_FOUND, "notFound"),
        Err(e) => failure(e),
    }
}
pub(crate) async fn put_group(
    State(state): State<ApiState>,
    Path((org, id)): Path<(Uuid, Uuid)>,
    h: HeaderMap,
    b: Result<Json<Value>, JsonRejection>,
) -> Response {
    update_group_inner(state, org, id, h, b, false).await
}
pub(crate) async fn patch_group(
    State(state): State<ApiState>,
    Path((org, id)): Path<(Uuid, Uuid)>,
    h: HeaderMap,
    b: Result<Json<Value>, JsonRejection>,
) -> Response {
    update_group_inner(state, org, id, h, b, true).await
}
pub(crate) async fn delete_group(
    State(state): State<ApiState>,
    Path((org, id)): Path<(Uuid, Uuid)>,
    h: HeaderMap,
) -> Response {
    let Some(s) = state.services else {
        return error(StatusCode::SERVICE_UNAVAILABLE, "unavailable");
    };
    let mut tx = match begin(&s, org, &h).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    match db::delete_group(
        &mut tx,
        org,
        id,
        s.clock.now(),
        &crate::audit::ctx(&h, None),
    )
    .await
    {
        Ok(true) => match tx.commit().await {
            Ok(()) => StatusCode::NO_CONTENT.into_response(),
            Err(e) => failure(e.into()),
        },
        Ok(false) => error(StatusCode::NOT_FOUND, "notFound"),
        Err(e) => failure(e),
    }
}

pub(crate) async fn status(
    State(state): State<ApiState>,
    session: AuthenticatedSession,
    Path(org): Path<Uuid>,
    h: HeaderMap,
) -> Response {
    let Some(s) = state.services else {
        return crate::responses::unavailable(&h, "AUTH_UNAVAILABLE");
    };
    let mut tx =
        match crate::sso_settings::owner_tx(&s, &session, org, Action::ScimManage, &h).await {
            Ok(t) => t,
            Err(r) => return *r,
        };
    let configured: bool =
        match sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM scim_tokens WHERE org_id=$1)")
            .bind(org)
            .fetch_one(&mut *tx)
            .await
        {
            Ok(v) => v,
            Err(_) => return crate::responses::internal(&h),
        };
    let groups = match db::list_groups(&mut tx, org, None, 1, 100).await {
        Ok((_, groups)) => groups
            .into_iter()
            .map(|(g, _)| json!({"id":g.id,"display_name":g.display_name}))
            .collect::<Vec<_>>(),
        Err(_) => return crate::responses::internal(&h),
    };
    Json(json!({"token_configured":configured,"groups":groups})).into_response()
}
pub(crate) async fn rotate_token(
    State(state): State<ApiState>,
    session: AuthenticatedSession,
    Path(org): Path<Uuid>,
    h: HeaderMap,
) -> Response {
    let Some(s) = state.services else {
        return crate::responses::unavailable(&h, "AUTH_UNAVAILABLE");
    };
    let mut tx =
        match crate::sso_settings::owner_tx(&s, &session, org, Action::ScimManage, &h).await {
            Ok(t) => t,
            Err(r) => return *r,
        };
    let token = match craftcloud_auth::OpaqueToken::generate() {
        Ok(t) => format!("cc_scim_{}", t.expose()),
        Err(_) => return crate::responses::internal(&h),
    };
    if db::rotate_token(&mut tx, org, &db::token_hash(&token))
        .await
        .is_err()
        || craftcloud_db::audit::record_in(
            &mut tx,
            &crate::audit::ctx(&h, Some(session.user_id)),
            &craftcloud_db::audit::NewEvent::new("scim.token_rotated").org(org),
        )
        .await
        .is_err()
        || tx.commit().await.is_err()
    {
        return crate::responses::internal(&h);
    }
    Json(json!({"token":token})).into_response()
}
pub(crate) async fn revoke_token(
    State(state): State<ApiState>,
    session: AuthenticatedSession,
    Path(org): Path<Uuid>,
    h: HeaderMap,
) -> Response {
    let Some(s) = state.services else {
        return crate::responses::unavailable(&h, "AUTH_UNAVAILABLE");
    };
    let mut tx =
        match crate::sso_settings::owner_tx(&s, &session, org, Action::ScimManage, &h).await {
            Ok(t) => t,
            Err(r) => return *r,
        };
    if db::revoke_token(&mut tx, org).await.is_err()
        || craftcloud_db::audit::record_in(
            &mut tx,
            &crate::audit::ctx(&h, Some(session.user_id)),
            &craftcloud_db::audit::NewEvent::new("scim.token_revoked").org(org),
        )
        .await
        .is_err()
        || tx.commit().await.is_err()
    {
        return crate::responses::internal(&h);
    }
    StatusCode::NO_CONTENT.into_response()
}
fn mapping_value(m: &db::Mapping) -> Value {
    json!({"group_id":m.group_id,"workspace_id":m.workspace_id,"role":m.role})
}
pub(crate) async fn mappings(
    State(state): State<ApiState>,
    session: AuthenticatedSession,
    Path(org): Path<Uuid>,
    h: HeaderMap,
) -> Response {
    let Some(s) = state.services else {
        return crate::responses::unavailable(&h, "AUTH_UNAVAILABLE");
    };
    let mut tx =
        match crate::sso_settings::owner_tx(&s, &session, org, Action::ScimManage, &h).await {
            Ok(t) => t,
            Err(r) => return *r,
        };
    match db::list_mappings(&mut tx, org).await {
        Ok(items) => Json(json!({"items":items.iter().map(mapping_value).collect::<Vec<_>>()}))
            .into_response(),
        Err(_) => crate::responses::internal(&h),
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MappingRequest {
    items: Vec<MappingInput>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MappingInput {
    group_id: Uuid,
    workspace_id: Uuid,
    role: String,
}
pub(crate) async fn set_mappings(
    State(state): State<ApiState>,
    session: AuthenticatedSession,
    Path(org): Path<Uuid>,
    h: HeaderMap,
    body: Result<Json<MappingRequest>, JsonRejection>,
) -> Response {
    let Some(s) = state.services else {
        return crate::responses::unavailable(&h, "AUTH_UNAVAILABLE");
    };
    let mut tx =
        match crate::sso_settings::owner_tx(&s, &session, org, Action::ScimManage, &h).await {
            Ok(t) => t,
            Err(r) => return *r,
        };
    let v = match body {
        Ok(Json(v)) => v,
        Err(e) => return crate::responses::rejection_response(&e, &h),
    };
    if v.items.len() > 100
        || v.items
            .iter()
            .any(|m| !["viewer", "editor"].contains(&m.role.as_str()))
    {
        return crate::responses::validation_failed(&["items"], &h);
    }
    let items = v
        .items
        .into_iter()
        .map(|m| db::Mapping {
            group_id: m.group_id,
            workspace_id: m.workspace_id,
            role: m.role,
        })
        .collect::<Vec<_>>();
    if let Err(e) = db::replace_mappings(
        &mut tx,
        org,
        &items,
        s.clock.now(),
        &crate::audit::ctx(&h, Some(session.user_id)),
    )
    .await
    {
        return match e {
            DbError::CheckViolation { .. }
            | DbError::ForeignKeyViolation { .. }
            | DbError::UniqueViolation { .. } => {
                crate::responses::validation_failed(&["items"], &h)
            }
            _ => crate::responses::internal(&h),
        };
    }
    if tx.commit().await.is_err() {
        return crate::responses::internal(&h);
    }
    Json(json!({"items":items.iter().map(mapping_value).collect::<Vec<_>>()})).into_response()
}
pub(crate) async fn events(
    State(state): State<ApiState>,
    session: AuthenticatedSession,
    Path(org): Path<Uuid>,
    h: HeaderMap,
) -> Response {
    let Some(s) = state.services else {
        return crate::responses::unavailable(&h, "AUTH_UNAVAILABLE");
    };
    let mut tx =
        match crate::sso_settings::owner_tx(&s, &session, org, Action::ScimManage, &h).await {
            Ok(t) => t,
            Err(r) => return *r,
        };
    let rows:Vec<(Uuid,String,time::OffsetDateTime)>=match sqlx::query_as("SELECT id,action,created_at FROM audit_events WHERE org_id=$1 AND action LIKE 'scim.%' ORDER BY created_at DESC,id DESC LIMIT 100").bind(org).fetch_all(&mut *tx).await{Ok(v)=>v,Err(_)=>return crate::responses::internal(&h)};
    Json(json!({"items":rows.into_iter().map(|(id,action,at)|json!({"id":id,"action":action,"created_at":crate::orgs::rfc3339(at)})).collect::<Vec<_>>()})).into_response()
}
fn rejected(status: StatusCode, kind: &str) -> Box<Response> {
    Box::new(error(status, kind))
}
fn set_value(value: &mut Value, key: &str, replacement: Value) -> Result<(), Box<Response>> {
    value
        .as_object_mut()
        .ok_or_else(|| rejected(StatusCode::BAD_REQUEST, "invalidValue"))?
        .insert(key.to_owned(), replacement);
    Ok(())
}

/// Preserves RFC 7644 errors even when rejection originates in shared middleware.
pub(crate) async fn normalize_errors(
    axum::extract::State(services): axum::extract::State<Option<std::sync::Arc<Services>>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let headers = request.headers().clone();
    let operation = audit_operation(&method, &path);
    let response = next.run(request).await;
    let status = response.status();
    if let (Some((org, action)), Some(services)) = (operation, services)
        && (status.is_client_error() || status.is_server_error())
    {
        // A failed operation must not wait indefinitely for its best-effort audit.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            if let Ok(tx) = begin(&services, org, &headers).await {
                drop(tx);
                let result = if status.is_server_error() {
                    craftcloud_db::audit::AuditResult::Failed
                } else {
                    craftcloud_db::audit::AuditResult::Denied
                };
                let event = craftcloud_db::audit::NewEvent::new(action)
                    .org(org)
                    .result(result);
                let _ = craftcloud_db::audit::record_aggregated(
                    &services.pool,
                    &crate::audit::ctx(&headers, None),
                    &event,
                    craftcloud_db::audit::AggScope::Actor,
                )
                .await;
            }
        })
        .await;
    }
    if !path.starts_with("/scim/v2/")
        || response
            .headers()
            .get(header::CONTENT_TYPE)
            .is_some_and(|v| v.as_bytes().starts_with(b"application/scim+json"))
        || (!status.is_client_error() && !status.is_server_error())
    {
        return response;
    }
    let kind = if status == StatusCode::UNAUTHORIZED {
        "invalidToken"
    } else if status == StatusCode::NOT_FOUND {
        "notFound"
    } else {
        "invalidValue"
    };
    let (parts, _) = response.into_parts();
    let mut converted = error(status, kind);
    for (name, value) in &parts.headers {
        if name != header::CONTENT_TYPE && name != header::CONTENT_LENGTH {
            converted.headers_mut().insert(name.clone(), value.clone());
        }
    }
    converted
}

fn audit_operation(method: &axum::http::Method, path: &str) -> Option<(Uuid, &'static str)> {
    let mut parts = path.split('/');
    if parts.next() != Some("")
        || parts.next() != Some("scim")
        || parts.next() != Some("v2")
        || parts.next() != Some("organizations")
    {
        return None;
    }
    let org = Uuid::parse_str(parts.next()?).ok()?;
    let resource = parts.next()?;
    if !matches!(resource, "Users" | "Groups") {
        return None;
    }
    match parts.next() {
        None => {}
        Some(id) if Uuid::parse_str(id).is_ok() && parts.next().is_none() => {}
        _ => return None,
    }
    let operation = match method.as_str() {
        "GET" | "HEAD" => "read",
        "POST" => "create",
        "PUT" => "replace",
        "PATCH" => "patch",
        "DELETE" => "delete",
        _ => return None,
    };
    Some((
        org,
        if resource == "Users" {
            match operation {
                "read" => "scim.users_read_failed",
                "create" => "scim.users_create_failed",
                "replace" => "scim.users_replace_failed",
                "patch" => "scim.users_patch_failed",
                _ => "scim.users_delete_failed",
            }
        } else {
            match operation {
                "read" => "scim.groups_read_failed",
                "create" => "scim.groups_create_failed",
                "replace" => "scim.groups_replace_failed",
                "patch" => "scim.groups_patch_failed",
                _ => "scim.groups_delete_failed",
            }
        },
    ))
}

async fn finish_read(
    mut tx: Transaction<'static, Postgres>,
    response: Response,
    org: Uuid,
    headers: &HeaderMap,
    action: &'static str,
) -> Response {
    if response.status().is_success()
        && (craftcloud_db::audit::record_in(
            &mut tx,
            &crate::audit::ctx(headers, None),
            &craftcloud_db::audit::NewEvent::new(action).org(org),
        )
        .await
        .is_err()
            || tx.commit().await.is_err())
    {
        return crate::responses::internal(headers);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn patch_is_atomic_and_never_accepts_role_changes() {
        let original = json!({"schemas":[USER_SCHEMA],"userName":"user@example.test","displayName":"User","active":true});
        let patch = json!({"schemas":[PATCH_SCHEMA],"Operations":[{"op":"replace","path":"active","value":false},{"op":"replace","path":"roles","value":["owner"]}]});
        assert!(patched(original.clone(), patch, false).is_err());
        assert_eq!(original["active"], true);
    }
    #[test]
    fn group_remove_targets_only_exact_member() {
        let id = Uuid::new_v4();
        let keep = Uuid::new_v4();
        let original = json!({"schemas":[GROUP_SCHEMA],"displayName":"Team","members":[{"value":id},{"value":keep}]});
        let patch = json!({"schemas":[PATCH_SCHEMA],"Operations":[{"op":"remove","path":format!("members[value eq \"{id}\"]")}]});
        let result = patched(original, patch, true).expect("valid patch");
        assert_eq!(result["members"], json!([{"value":keep}]));
    }
}
