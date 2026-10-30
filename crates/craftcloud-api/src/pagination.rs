//! Keyset pagination: page size handling and opaque cursors.
//!
//! Cursors carry the key of the last item of the previous page (a timestamp
//! and an id, never names or other personal data), the kind of
//! listing and its scope (the organization or the user), so a cursor cannot be
//! replayed against another listing or tenant. They are **not signed**: a
//! tampered cursor can only move the keyset position inside a listing the
//! caller is already authorized for, because every query filters by scope in
//! SQL. Signing needs a server secret and follows with the session secret.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use time::OffsetDateTime;
use uuid::Uuid;

/// Default page size.
pub(crate) const DEFAULT_LIMIT: u32 = 50;
/// Maximum page size; larger requests are clamped.
pub(crate) const MAX_LIMIT: u32 = 100;
/// Longest accepted cursor text.
const MAX_CURSOR_CHARS: usize = 512;

/// Query parameters of a listing.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PageQuery {
    pub(crate) limit: Option<String>,
    pub(crate) cursor: Option<String>,
}

/// Query parameters of the operator's account listing.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct UserListQuery {
    limit: Option<String>,
    cursor: Option<String>,
    unverified: Option<String>,
}

impl UserListQuery {
    /// Paging parameters and the `unverified` filter (`true` or `false`).
    pub(crate) fn split(self) -> Result<(PageQuery, bool), PageError> {
        let unverified = match self.unverified.as_deref() {
            None | Some("false") => false,
            Some("true") => true,
            Some(_) => return Err(PageError::Limit),
        };
        Ok((
            PageQuery {
                limit: self.limit,
                cursor: self.cursor,
            },
            unverified,
        ))
    }
}

/// Query parameters of a listing with the trash switch (`deleted=true|false`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TrashListQuery {
    limit: Option<String>,
    cursor: Option<String>,
    deleted: Option<String>,
}

impl TrashListQuery {
    /// Paging parameters and the `deleted` switch (`true` or `false`).
    pub(crate) fn split(self) -> Result<(PageQuery, bool), PageError> {
        let deleted = match self.deleted.as_deref() {
            None | Some("false") => false,
            Some("true") => true,
            Some(_) => return Err(PageError::Limit),
        };
        Ok((
            PageQuery {
                limit: self.limit,
                cursor: self.cursor,
            },
            deleted,
        ))
    }
}

/// Why paging parameters were rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PageError {
    /// `limit` is not a positive integer.
    Limit,
    /// The cursor is malformed or belongs to another listing or scope.
    Cursor,
}

impl PageQuery {
    /// The validated page size (clamped to the maximum).
    pub(crate) fn limit(&self) -> Result<u32, PageError> {
        match &self.limit {
            None => Ok(DEFAULT_LIMIT),
            Some(raw) => match raw.parse::<u32>() {
                Ok(0) | Err(_) => Err(PageError::Limit),
                Ok(n) => Ok(n.min(MAX_LIMIT)),
            },
        }
    }

    pub(crate) fn cursor(&self) -> Option<&str> {
        self.cursor.as_deref()
    }
}

fn micros(at: OffsetDateTime) -> i128 {
    at.unix_timestamp_nanos() / 1000
}

fn from_micros(value: i128) -> Option<OffsetDateTime> {
    OffsetDateTime::from_unix_timestamp_nanos(value.checked_mul(1000)?).ok()
}

fn encode(parts: &[&str]) -> String {
    URL_SAFE_NO_PAD.encode(parts.join("|"))
}

fn decode(cursor: &str, kind: &str, scope: &str, fields: usize) -> Result<Vec<String>, PageError> {
    if cursor.len() > MAX_CURSOR_CHARS {
        return Err(PageError::Cursor);
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(cursor)
        .map_err(|_| PageError::Cursor)?;
    let text = String::from_utf8(bytes).map_err(|_| PageError::Cursor)?;
    // The last field (a display name) may itself contain the separator.
    let parts: Vec<String> = text.splitn(3 + fields, '|').map(str::to_owned).collect();
    match parts.as_slice() {
        [version, k, s, ..]
            if version == "v1" && k == kind && s == scope && parts.len() == 3 + fields =>
        {
            Ok(parts.into_iter().skip(3).collect())
        }
        _ => Err(PageError::Cursor),
    }
}

/// Cursor of a `(created_at, id)` listing.
pub(crate) fn encode_time_cursor(kind: &str, scope: &str, at: OffsetDateTime, id: Uuid) -> String {
    encode(&["v1", kind, scope, &micros(at).to_string(), &id.to_string()])
}

pub(crate) fn decode_time_cursor(
    cursor: &str,
    kind: &str,
    scope: &str,
) -> Result<(OffsetDateTime, Uuid), PageError> {
    let fields = decode(cursor, kind, scope, 2)?;
    let (Some(at), Some(id)) = (fields.first(), fields.get(1)) else {
        return Err(PageError::Cursor);
    };
    let at = at
        .parse::<i128>()
        .ok()
        .and_then(from_micros)
        .ok_or(PageError::Cursor)?;
    let id = Uuid::parse_str(id).map_err(|_| PageError::Cursor)?;
    Ok((at, id))
}

/// Cursor of a listing ordered by a number (newest first).
pub(crate) fn encode_number_cursor(kind: &str, scope: &str, number: i64) -> String {
    encode(&["v1", kind, scope, &number.to_string()])
}

pub(crate) fn decode_number_cursor(
    cursor: &str,
    kind: &str,
    scope: &str,
) -> Result<i64, PageError> {
    let fields = decode(cursor, kind, scope, 1)?;
    fields
        .first()
        .and_then(|n| n.parse::<i64>().ok())
        .filter(|n| *n >= 1)
        .ok_or(PageError::Cursor)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(limit: Option<&str>) -> PageQuery {
        PageQuery {
            limit: limit.map(str::to_owned),
            cursor: None,
        }
    }

    #[test]
    fn limits_are_validated_and_clamped() {
        assert_eq!(query(None).limit(), Ok(50));
        assert_eq!(query(Some("1")).limit(), Ok(1));
        assert_eq!(query(Some("100")).limit(), Ok(100));
        assert_eq!(query(Some("100000")).limit(), Ok(100));
        for bad in ["0", "-1", "abc", "", "1.5", "99999999999999999999"] {
            assert_eq!(query(Some(bad)).limit(), Err(PageError::Limit), "{bad:?}");
        }
    }

    #[test]
    fn time_cursors_round_trip_and_are_bound_to_kind_and_scope() {
        let at = OffsetDateTime::from_unix_timestamp_nanos(1_700_000_000_123_456_000).unwrap();
        let id = Uuid::new_v4();
        let cursor = encode_time_cursor("orgs", "user-1", at, id);
        assert_eq!(decode_time_cursor(&cursor, "orgs", "user-1"), Ok((at, id)));
        assert_eq!(
            decode_time_cursor(&cursor, "admin", "user-1"),
            Err(PageError::Cursor)
        );
        assert_eq!(
            decode_time_cursor(&cursor, "orgs", "user-2"),
            Err(PageError::Cursor)
        );
    }

    #[test]
    fn cursors_contain_no_personal_data_only_a_timestamp_and_ids() {
        let id = Uuid::new_v4();
        let at = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let cursor = encode_time_cursor("members", "org-1", at, id);
        let text = String::from_utf8(URL_SAFE_NO_PAD.decode(&cursor).unwrap()).unwrap();
        assert_eq!(text.split('|').count(), 5);
        assert!(text.is_ascii());
    }

    #[test]
    fn garbage_cursors_are_rejected() {
        for bad in [
            "",
            "!!!",
            "dGVzdA",
            &"a".repeat(600),
            &URL_SAFE_NO_PAD.encode("v1|orgs|s|x|y"),
        ] {
            assert!(decode_time_cursor(bad, "orgs", "s").is_err(), "{bad:?}");
        }
    }
}
