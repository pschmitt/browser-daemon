//! The error type the JSON API handlers return.

use super::*;

pub(crate) type ApiError = (StatusCode, Json<serde_json::Value>);

pub(crate) fn api_error(status: StatusCode, msg: impl Into<String>) -> ApiError {
    (status, Json(serde_json::json!({ "error": msg.into() })))
}
