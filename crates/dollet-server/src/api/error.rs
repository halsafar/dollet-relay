//! One error type for every handler.
//!
//! Domain errors already carry enough to pick a status code, so handlers
//! return them with `?` and never build a response by hand. Internal failures
//! are logged here and replaced with a flat message: a SQL error text in a 500
//! body tells an attacker the schema.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use dollet_core::Error;
use serde_json::{Value, json};

pub enum ApiError {
    Domain(Error),
    /// A status the domain has no opinion about — so far only 429, which is a
    /// property of how often a caller asked rather than of what they asked for.
    Status(StatusCode, String),
    /// A body with more than a sentence in it.
    ///
    /// `dollet_core::Error` carries a `String`, so structure is carried as a
    /// value rather than serialised into the error `String` and parsed back
    /// out.
    Structured(StatusCode, Value),
}

impl<E: Into<Error>> From<E> for ApiError {
    fn from(error: E) -> Self {
        Self::Domain(error.into())
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn too_many_requests(detail: impl Into<String>) -> Self {
        Self::Status(StatusCode::TOO_MANY_REQUESTS, detail.into())
    }

    /// A refusal that carries machine-readable detail beside its sentence.
    ///
    /// `detail` stays exactly where every client already looks, so nothing has
    /// to change to keep working; the extra keys are additive.
    pub fn with(status: StatusCode, detail: impl Into<String>, extra: Value) -> Self {
        let mut body = json!({ "detail": detail.into() });
        if let (Some(body), Some(extra)) = (body.as_object_mut(), extra.as_object()) {
            for (key, value) in extra {
                body.insert(key.clone(), value.clone());
            }
        }
        Self::Structured(status, body)
    }

    /// Field-level validation messages, flat and one per field.
    ///
    /// ```json
    /// {"detail": "...", "fields": {"username": "already taken"}}
    /// ```
    ///
    /// One message rather than DRF's arrays: a form shows one line under a
    /// field, and a list whose tail is never rendered is a list that invites
    /// writing detail nobody reads.
    pub fn invalid_fields<K, V>(
        detail: impl Into<String>,
        fields: impl IntoIterator<Item = (K, V)>,
    ) -> Self
    where
        K: Into<String>,
        V: Into<String>,
    {
        let fields: serde_json::Map<String, Value> = fields
            .into_iter()
            .map(|(key, message)| (key.into(), Value::String(message.into())))
            .collect();
        Self::with(
            StatusCode::BAD_REQUEST,
            detail,
            json!({ "fields": Value::Object(fields) }),
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, detail) = match self {
            Self::Structured(status, body) => return (status, Json(body)).into_response(),
            Self::Status(status, detail) => (status, detail),
            Self::Domain(Error::NotFound) => (StatusCode::NOT_FOUND, "not found".to_owned()),
            Self::Domain(Error::Invalid(message)) => (StatusCode::BAD_REQUEST, message),
            Self::Domain(Error::Unauthorized) => (
                StatusCode::UNAUTHORIZED,
                "authentication required".to_owned(),
            ),
            Self::Domain(Error::Forbidden) => (
                StatusCode::FORBIDDEN,
                "not permitted for this account".to_owned(),
            ),
            Self::Domain(Error::Conflict(message)) => (StatusCode::CONFLICT, message),
            Self::Domain(Error::Upstream(message)) => (StatusCode::BAD_GATEWAY, message),
            Self::Domain(other) => {
                tracing::error!(error = %other, "request failed");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal server error".to_owned(),
                )
            }
        };

        // A detail that is itself a JSON object is spliced in rather than
        // nested as a string. A refusal that has to carry structure — the
        // blast-radius counts behind a 409, say — would otherwise arrive as a
        // quoted blob the caller has to parse a second time before it can show
        // a number to the user.
        let body = match serde_json::from_str::<serde_json::Value>(&detail) {
            Ok(serde_json::Value::Object(fields)) if fields.contains_key("detail") => {
                serde_json::Value::Object(fields)
            }
            _ => json!({ "detail": detail }),
        };

        (status, Json(body)).into_response()
    }
}
