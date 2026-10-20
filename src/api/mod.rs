pub mod catalog;
pub mod sync;
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
        .merge(sync::routes())
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

/// The only place a request learns who its user is: a bearer token's user, or user 1 when
/// there is no `Authorization` header (the web UI). A bad token is a 401, never user 1.
pub struct CurrentUser(pub i64);

/// Like `CurrentUser`, but a token is required.
pub struct TokenUser(pub i64);

/// `None` without a header; otherwise the token's user or `Unauthorized`.
async fn bearer_user(parts: &Parts, st: &AppState) -> ApiResult<Option<i64>> {
    let Some(h) = parts.headers.get(header::AUTHORIZATION) else {
        return Ok(None);
    };
    let token = h
        .to_str()
        .ok()
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(ApiError::Unauthorized)?;
    match crate::tokens::authenticate(&st.pool, token).await? {
        Some(user) => Ok(Some(user)),
        None => Err(ApiError::Unauthorized),
    }
}

impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> ApiResult<Self> {
        Ok(CurrentUser(bearer_user(parts, st).await?.unwrap_or(1)))
    }
}

impl FromRequestParts<AppState> for TokenUser {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> ApiResult<Self> {
        let user = bearer_user(parts, st).await?;
        Ok(TokenUser(user.ok_or(ApiError::Unauthorized)?))
    }
}

#[derive(Debug)]
pub enum ApiError {
    NotFound,
    Unauthorized,
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
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, "invalid token".to_string()),
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
            let res = app(
                AppState::new(pool.clone(), crate::config::Config::for_tests()),
                "web/dist",
            )
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

    #[tokio::test]
    async fn bearer_token_selects_user_or_401() {
        let pool = test_pool().await;
        let token = crate::tokens::create(&pool, 1, "pc").await.unwrap();
        for (auth, want) in [
            (None, 200),
            (Some("Bearer nope".to_string()), 401),
            (Some(format!("Bearer {token}")), 200),
        ] {
            let mut req = Request::builder().uri("/api/wishlist");
            if let Some(a) = &auth {
                req = req.header("authorization", a);
            }
            let res = app(
                AppState::new(pool.clone(), crate::config::Config::for_tests()),
                "web/dist",
            )
            .oneshot(req.body(B::empty()).unwrap())
            .await
            .unwrap();
            assert_eq!(res.status().as_u16(), want, "{auth:?}");
            let bytes = res.into_body().collect().await.unwrap().to_bytes();
            let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            if want == 401 {
                assert_eq!(v["error"], "invalid token");
            }
        }
    }
}
