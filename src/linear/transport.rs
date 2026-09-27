//! The rules of one GraphQL request to Linear: the endpoint, the bounds, the
//! decoding of an answer, and the viewer check.
//!
//! Reads go through the credential manager's verified-read lease: every read
//! selects `viewer { id app isMe }`, and the response is accepted only when
//! the viewer is an app actor, whose ID the manager binds to the credential.
//! Writes take a lease only after that binding exists.

use std::time::Duration;

use serde_json::Value;
use zeroize::Zeroizing;

use super::{ApiError, VerifiedReadOutcome};

pub const GRAPHQL_ENDPOINT: &str = "https://api.linear.app/graphql";
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
pub(crate) const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_ERROR_MESSAGE_CHARS: usize = 200;

/// Exactly one `application/json` content type, parameters allowed.
pub(crate) fn json_content_type<'a>(mut values: impl Iterator<Item = &'a str>) -> bool {
    let (Some(value), None) = (values.next(), values.next()) else {
        return false;
    };
    value
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .eq_ignore_ascii_case("application/json")
}

/// A GraphQL response's `data`, or the first error it reports. HTTP 429 and
/// a GraphQL error whose `extensions.code` is `RATELIMITED` (Linear answers
/// those with HTTP 400) are `RateLimited`, never a definitive refusal.
pub fn decode(status: u16, json_content: bool, body: &[u8]) -> Result<Value, ApiError> {
    if status == 429 {
        return Err(ApiError::RateLimited);
    }
    if !json_content {
        return Err(if status == 200 {
            ApiError::ContentType
        } else {
            ApiError::HttpStatus(status)
        });
    }
    let reply: Value = serde_json::from_slice(body).map_err(|_| {
        if status == 200 {
            ApiError::ReadFieldsInvalid
        } else {
            ApiError::HttpStatus(status)
        }
    })?;
    let errors = reply["errors"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    if errors
        .iter()
        .any(|error| error["extensions"]["code"] == "RATELIMITED")
    {
        return Err(ApiError::RateLimited);
    }
    if let Some(error) = errors.first() {
        let message = error["extensions"]["userPresentableMessage"]
            .as_str()
            .or_else(|| error["message"].as_str())
            .unwrap_or("unknown error");
        return Err(ApiError::Graphql(
            message
                .chars()
                .filter(|c| !c.is_control())
                .take(MAX_ERROR_MESSAGE_CHARS)
                .collect(),
        ));
    }
    if status != 200 {
        return Err(ApiError::HttpStatus(status));
    }
    match reply.get("data") {
        Some(data) if data.is_object() => Ok(data.clone()),
        _ => Err(ApiError::ReadFieldsInvalid),
    }
}

/// Accepts a read's data only when its viewer is this plugin's app actor.
pub(crate) fn verified(data: Value) -> Result<VerifiedReadOutcome<Value>, ApiError> {
    let viewer = &data["viewer"];
    if viewer["app"].as_bool() != Some(true) || viewer["isMe"].as_bool() != Some(true) {
        return Err(ApiError::ActorIdentityMismatch);
    }
    let id = viewer["id"]
        .as_str()
        .filter(|id| !id.trim().is_empty() && id.len() <= 4096)
        .ok_or(ApiError::ActorIdentityMismatch)?;
    Ok(VerifiedReadOutcome::with_value(
        Zeroizing::new(id.to_owned()),
        data.clone(),
    ))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn decoding_prefers_graphql_errors_and_requires_json() {
        assert_eq!(decode(200, true, br#"{"data":{"x":1}}"#).unwrap()["x"], 1);
        assert_eq!(
            decode(400, true, br#"{"errors":[{"message":"Entity not found","extensions":{"code":"INVALID_INPUT"}}]}"#),
            Err(ApiError::Graphql("Entity not found".into()))
        );
        assert_eq!(
            decode(200, true, br#"{"data":null,"errors":[{"message":"x","extensions":{"userPresentableMessage":"Readable"}}]}"#),
            Err(ApiError::Graphql("Readable".into()))
        );
        assert_eq!(
            decode(502, false, b"<html>"),
            Err(ApiError::HttpStatus(502))
        );
        assert_eq!(decode(200, false, b"{}"), Err(ApiError::ContentType));
        assert_eq!(decode(200, true, b"{}"), Err(ApiError::ReadFieldsInvalid));
        assert!(json_content_type(
            ["application/json; charset=utf-8"].into_iter()
        ));
        assert!(!json_content_type(
            ["application/json", "text/html"].into_iter()
        ));
        assert!(!json_content_type(std::iter::empty()));
    }

    #[test]
    fn rate_limits_decode_apart_from_refusals() {
        assert_eq!(
            decode(
                400,
                true,
                br#"{"errors":[{"message":"Rate limit exceeded","extensions":{"code":"RATELIMITED"}}]}"#
            ),
            Err(ApiError::RateLimited)
        );
        assert_eq!(
            decode(429, false, b"Too Many Requests"),
            Err(ApiError::RateLimited)
        );
        assert_eq!(decode(429, true, b"{}"), Err(ApiError::RateLimited));
        assert_eq!(
            decode(
                400,
                true,
                br#"{"errors":[{"message":"Entity not found","extensions":{"code":"INVALID_INPUT"}}]}"#
            ),
            Err(ApiError::Graphql("Entity not found".into()))
        );
        assert_eq!(
            decode(502, false, b"<html>"),
            Err(ApiError::HttpStatus(502))
        );
        assert_eq!(decode(200, true, br#"{"data":{"x":1}}"#).unwrap()["x"], 1);
        assert_eq!(
            ApiError::RateLimited.to_string(),
            "Linear rate-limited the request"
        );
    }

    #[test]
    fn only_an_app_viewer_is_verified() {
        let data = json!({"viewer":{"id":"app-1","app":true,"isMe":true},"x":1});
        let (viewer, value) = verified(data).unwrap().into_parts();
        assert_eq!(viewer.as_str(), "app-1");
        assert_eq!(value["x"], 1);
        assert_eq!(
            verified(json!({"viewer":{"id":"u","app":false,"isMe":true}})).unwrap_err(),
            ApiError::ActorIdentityMismatch
        );
        assert_eq!(
            verified(json!({})).unwrap_err(),
            ApiError::ActorIdentityMismatch
        );
    }
}
