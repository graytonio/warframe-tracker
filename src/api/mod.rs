pub mod catalog;
pub mod user;

use crate::AppState;
use axum::{
    Json, Router,
    extract::FromRequestParts,
    http::{StatusCode, header, request::Parts},
    middleware,
    response::{IntoResponse, Response},
};

pub fn router() -> Router<AppState> {
    Router::new()
        .merge(catalog::routes())
        .merge(user::routes())
        .merge(crate::plan::routes())
        // Unknown /api paths must not fall through to the SPA's index.html.
        .fallback(|| async { ApiError::NotFound })
        .layer(middleware::map_response(json_errors))
}

/// Extractor rejections (bad path/query/body) are plain text; rewrap them as `{"error": ...}`.
async fn json_errors(res: Response) -> Response {
    let is_json = res
        .headers()
        .get(header::CONTENT_TYPE)
        .is_some_and(|v| v.as_bytes().starts_with(b"application/json"));
    if !res.status().is_client_error() || is_json {
        return res;
    }
    let status = res.status();
    let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
        .await
        .unwrap_or_default();
    let msg = String::from_utf8_lossy(&body).into_owned();
    let mut out = Json(serde_json::json!({ "error": msg })).into_response();
    *out.status_mut() = status;
    out
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

#[cfg(test)]
mod tests {
    use crate::{AppState, app, db::test_pool};
    use axum::{body::Body as B, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn every_api_error_is_json() {
        let pool = test_pool().await;
        for (method, uri, body) in [
            ("GET", "/api/items/abc", ""),
            ("GET", "/api/items?limit=abc", ""),
            ("POST", "/api/wishlist", "{}"),
            ("POST", "/api/wishlist", "not json"),
            ("GET", "/api/nope", ""),
        ] {
            let req = Request::builder()
                .method(method)
                .uri(uri)
                .header("content-type", "application/json")
                .body(B::from(body))
                .unwrap();
            let res = app(AppState { pool: pool.clone() }, "web/dist")
                .oneshot(req)
                .await
                .unwrap();
            let status = res.status();
            assert!(status.is_client_error(), "{method} {uri}: {status}");
            let bytes = res.into_body().collect().await.unwrap().to_bytes();
            let v: serde_json::Value = serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| panic!("{method} {uri} not JSON: {bytes:?}"));
            assert!(v["error"].is_string(), "{method} {uri}");
        }
    }
}
