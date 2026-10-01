use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

/// A failed request.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    /// A stable, machine-readable code, such as `conflict`.
    pub code: &'static str,
    pub message: String,
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        ApiError {
            status,
            code,
            message: message.into(),
        }
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        ApiError::new(StatusCode::BAD_REQUEST, "invalid", message)
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        ApiError::new(StatusCode::UNAUTHORIZED, "unauthorized", message)
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        ApiError::new(StatusCode::FORBIDDEN, "forbidden", message)
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        ApiError::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    pub fn internal(message: impl Into<String>) -> Self {
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = serde_json::json!({
            "error": { "code": self.code, "message": self.message }
        });
        (self.status, Json(body)).into_response()
    }
}

impl From<octopage::Error> for ApiError {
    fn from(error: octopage::Error) -> Self {
        use octopage::Error as E;
        use octopage_pagestore::Error as Store;
        let message = error.to_string();
        let (status, code) = match &error {
            E::Sql(_) => (StatusCode::BAD_REQUEST, "sql"),
            E::Conflict => (StatusCode::CONFLICT, "conflict"),
            E::Serialization { .. } => (StatusCode::CONFLICT, "busy"),
            E::Unavailable(_) => (StatusCode::SERVICE_UNAVAILABLE, "unavailable"),
            E::OutcomeUnknown => (StatusCode::BAD_GATEWAY, "outcome_unknown"),
            E::Full(_) => (StatusCode::INSUFFICIENT_STORAGE, "full"),
            E::SecretBlocked { .. } => (StatusCode::UNPROCESSABLE_ENTITY, "secret_blocked"),
            E::Store(Store::Locked | Store::WrongKey) => (StatusCode::LOCKED, "locked"),
            E::Store(Store::NoDatabase(_)) => (StatusCode::NOT_FOUND, "not_found"),
            E::AsOf { .. } | E::Invalid(_) | E::Merge { .. } => {
                (StatusCode::BAD_REQUEST, "invalid")
            }
            _ => (StatusCode::BAD_GATEWAY, "storage"),
        };
        ApiError::new(status, code, message)
    }
}

impl From<crate::meta::MetaError> for ApiError {
    fn from(error: crate::meta::MetaError) -> Self {
        tracing::error!(%error, "metadata store");
        ApiError::internal("the service's metadata store failed")
    }
}
