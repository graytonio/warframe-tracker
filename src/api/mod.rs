pub mod catalog;
pub mod user;

use crate::AppState;
use axum::{
    Json, Router,
    extract::FromRequestParts,
    http::{StatusCode, request::Parts},
    response::{IntoResponse, Response},
};

pub fn router() -> Router<AppState> {
    Router::new()
        .merge(catalog::routes())
        .merge(user::routes())
        .merge(crate::plan::routes())
}

/// The only place a request learns who its user is. Replace the body with a session lookup to add auth.
pub struct CurrentUser(pub i64);

impl<S: Send + Sync> FromRequestParts<S> for CurrentUser {
    type Rejection = ApiError;
    async fn from_request_parts(_: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(CurrentUser(1))
    }
}

#[derive(Debug)]
pub enum ApiError {
    NotFound,
    BadRequest(String),
    Internal(String),
}

pub type ApiResult<T> = Result<T, ApiError>;

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        // FK violation on a write means the referenced item doesn't exist.
        if let sqlx::Error::Database(d) = &e
            && d.code().as_deref() == Some("23503")
        {
            return ApiError::NotFound;
        }
        ApiError::Internal(e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (code, msg) = match self {
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            ApiError::Internal(m) => {
                eprintln!("internal error: {m}");
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error".to_string(),
                )
            }
        };
        (code, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}
