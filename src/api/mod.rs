pub mod catalog;
pub mod sync;
pub mod user;

use crate::AppState;
use axum::{
    Json, Router,
    extract::FromRequestParts,
    http::{Method, StatusCode, header, request::Parts},
    middleware,
    response::{IntoResponse, Response},
};
use axum_extra::extract::cookie::CookieJar;

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

pub const SESSION_COOKIE: &str = "wft_session";

/// The only place a request learns who its user is: a bearer token's user, else a
/// `wft_session` cookie's user, else 401. A bad bearer token is a 401, never a fallback.
/// Session-authenticated writes must come from our own `Origin` (CSRF).
pub struct CurrentUser(pub i64);

/// Like `CurrentUser`, but no credentials gives `None`. Only for public GET routes.
pub struct OptionalUser(pub Option<i64>);

/// `(user, via_session)`: bearer first, then the session cookie.
async fn resolve(parts: &Parts, st: &AppState) -> ApiResult<Option<(i64, bool)>> {
    if let Some(h) = parts.headers.get(header::AUTHORIZATION) {
        let token = h
            .to_str()
            .ok()
            .and_then(|v| v.strip_prefix("Bearer "))
            .ok_or_else(invalid_token)?;
        return match crate::tokens::authenticate(&st.pool, token).await? {
            Some(user) => Ok(Some((user, false))),
            None => Err(invalid_token()),
        };
    }
    let jar = CookieJar::from_headers(&parts.headers);
    let Some(c) = jar.get(SESSION_COOKIE) else {
        return Ok(None);
    };
    Ok(crate::sessions::lookup(&st.pool, c.value())
        .await?
        .map(|u| (u, true)))
}

fn invalid_token() -> ApiError {
    ApiError::Unauthorized("invalid token".into())
}

impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> ApiResult<Self> {
        let (user, via_session) = resolve(parts, st)
            .await?
            .ok_or_else(|| ApiError::Unauthorized("sign in required".into()))?;
        let read = parts.method == Method::GET || parts.method == Method::HEAD;
        let same_origin = parts
            .headers
            .get(header::ORIGIN)
            .is_some_and(|o| o.as_bytes() == st.config.origin().as_bytes());
        if via_session && !read && !same_origin {
            return Err(ApiError::Forbidden("cross-site request".into()));
        }
        Ok(CurrentUser(user))
    }
}

impl FromRequestParts<AppState> for OptionalUser {
    type Rejection = ApiError;
    async fn from_request_parts(parts: &mut Parts, st: &AppState) -> ApiResult<Self> {
        Ok(OptionalUser(resolve(parts, st).await?.map(|(u, _)| u)))
    }
}

#[derive(Debug)]
pub enum ApiError {
    NotFound,
    Unauthorized(String),
    Forbidden(String),
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
            ApiError::Unauthorized(m) => (StatusCode::UNAUTHORIZED, m),
            ApiError::Forbidden(m) => (StatusCode::FORBIDDEN, m),
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
pub mod test_support {
    use crate::{AppState, app, config::Config};
    use axum::{
        body::Body,
        http::{HeaderMap, Request, StatusCode, request::Builder},
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    pub enum Auth<'a> {
        None,
        Session(&'a str),
        Bearer(&'a str),
    }

    /// Session cookie for `user_id` (creating the user row if needed).
    pub async fn login(pool: &sqlx::PgPool, user_id: i64) -> String {
        sqlx::query("INSERT INTO users (id, name) VALUES ($1, 'test') ON CONFLICT DO NOTHING")
            .bind(user_id)
            .execute(pool)
            .await
            .unwrap();
        crate::sessions::create(pool, user_id).await.unwrap()
    }

    /// A request with `auth`, our own `Origin`, and a JSON content type when `json`.
    pub fn request(auth: Auth<'_>, method: &str, uri: &str, json: bool) -> Builder {
        let mut req = Request::builder()
            .method(method)
            .uri(uri)
            .header("origin", "http://localhost:3000");
        match auth {
            Auth::None => {}
            Auth::Session(c) => req = req.header("cookie", format!("wft_session={c}")),
            Auth::Bearer(t) => req = req.header("authorization", format!("Bearer {t}")),
        }
        if json {
            req = req.header("content-type", "application/json");
        }
        req
    }

    pub async fn send(
        pool: &sqlx::PgPool,
        req: Request<Body>,
    ) -> (StatusCode, HeaderMap, serde_json::Value) {
        let res = app(AppState::new(pool.clone(), Config::for_tests()), "web/dist")
            .oneshot(req)
            .await
            .unwrap();
        let (status, headers) = (res.status(), res.headers().clone());
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let v = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, headers, v)
    }

    pub async fn call(
        pool: &sqlx::PgPool,
        auth: Auth<'_>,
        method: &str,
        uri: &str,
        body: Option<serde_json::Value>,
    ) -> (StatusCode, serde_json::Value) {
        let req = request(auth, method, uri, body.is_some());
        let b = body.map_or(Body::empty(), |b| Body::from(b.to_string()));
        let (s, _, v) = send(pool, req.body(b).unwrap()).await;
        (s, v)
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{Auth, call, login, request, send};
    use crate::db::test_pool;
    use axum::{body::Body as B, http::StatusCode};
    use serde_json::json;

    #[tokio::test]
    async fn every_api_error_is_json() {
        let pool = test_pool().await;
        let c = login(&pool, 1).await;
        for (method, uri, body) in [
            ("GET", "/api/items/abc", ""),
            ("GET", "/api/items?limit=abc", ""),
            ("POST", "/api/wishlist", "{}"),
            ("POST", "/api/wishlist", "not json"),
            ("GET", "/api/nope", ""),
        ] {
            let req = request(Auth::Session(&c), method, uri, true)
                .body(B::from(body))
                .unwrap();
            let (status, _, v) = send(&pool, req).await;
            assert!(status.is_client_error(), "{method} {uri}: {status}");
            assert_ne!(status, StatusCode::UNAUTHORIZED, "{method} {uri}");
            assert!(v["error"].is_string(), "{method} {uri}");
        }
    }

    #[tokio::test]
    async fn bearer_token_selects_user_or_401() {
        let pool = test_pool().await;
        let token = crate::tokens::create(&pool, 1, "pc").await.unwrap();
        for (auth, want, msg) in [
            (Auth::None, 401, "sign in required"),
            (Auth::Bearer("nope"), 401, "invalid token"),
            (Auth::Bearer(&token), 200, ""),
        ] {
            let (s, v) = call(&pool, auth, "GET", "/api/wishlist", None).await;
            assert_eq!(s.as_u16(), want, "{msg}");
            if want == 401 {
                assert_eq!(v["error"], msg);
            }
        }
        // A bad bearer is a 401 even next to a valid session.
        let c = login(&pool, 1).await;
        let req = request(Auth::Session(&c), "GET", "/api/wishlist", false)
            .header("authorization", "Bearer nope")
            .body(B::empty())
            .unwrap();
        assert_eq!(send(&pool, req).await.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn unauthenticated_requests_are_401() {
        let pool = test_pool().await;
        for (m, u, b) in [
            ("GET", "/api/wishlist", None),
            ("POST", "/api/wishlist", Some(json!({"item_id": 1}))),
            ("PATCH", "/api/wishlist/1", Some(json!({"completed": true}))),
            ("DELETE", "/api/wishlist/1", None),
            ("PUT", "/api/progress/1", Some(json!({"obtained": true}))),
            ("GET", "/api/inventory", None),
            ("PUT", "/api/inventory", Some(json!([]))),
            ("PATCH", "/api/inventory/1", Some(json!({"count": 1}))),
            ("GET", "/api/plan", None),
            ("POST", "/api/sync/inventory", Some(json!({"items": {}}))),
        ] {
            let (s, v) = call(&pool, Auth::None, m, u, b.clone()).await;
            assert_eq!(s, StatusCode::UNAUTHORIZED, "{m} {u}");
            assert_eq!(v, json!({"error": "sign in required"}), "{m} {u}");
            // An unknown session cookie is the same as none.
            let (s, _) = call(&pool, Auth::Session("garbage"), m, u, b).await;
            assert_eq!(s, StatusCode::UNAUTHORIZED, "{m} {u}");
        }
    }

    async fn seed_item(pool: &sqlx::PgPool) {
        sqlx::raw_sql(
            "INSERT INTO items (id, wiki_name, category) VALUES (1,'Saryn Prime','warframe'),(2,'Rubedo','resource');
             INSERT INTO recipes VALUES (1,2,10);
             INSERT INTO inventory (user_id,item_id,count) VALUES (1,2,50);
             INSERT INTO component_progress VALUES (1,2,true);",
        )
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn catalog_is_public() {
        let pool = test_pool().await;
        seed_item(&pool).await;
        let (s, v) = call(&pool, Auth::None, "GET", "/api/items?q=saryn", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v[0]["name"], "Saryn Prime");
        let (s, v) = call(&pool, Auth::None, "GET", "/api/items/1", None).await;
        assert_eq!(s, StatusCode::OK);
        let rubedo = &v["tree"]["children"][0];
        assert_eq!(rubedo["have"], 0);
        assert_eq!(rubedo["obtained"], false);
        // Signed in as user 1, the same route shows user 1's progress.
        let c = login(&pool, 1).await;
        let v = call(&pool, Auth::Session(&c), "GET", "/api/items/1", None)
            .await
            .1;
        assert_eq!(v["tree"]["children"][0]["have"], 50);
        // A stale cookie doesn't break the public catalog.
        let (s, _) = call(&pool, Auth::Session("stale"), "GET", "/api/items/1", None).await;
        assert_eq!(s, StatusCode::OK);
        let (s, _) = call(&pool, Auth::None, "GET", "/api/meta", None).await;
        assert_eq!(s, StatusCode::OK);
    }

    #[tokio::test]
    async fn session_writes_need_same_origin() {
        let pool = test_pool().await;
        seed_item(&pool).await;
        let c = login(&pool, 1).await;
        let token = crate::tokens::create(&pool, 1, "pc").await.unwrap();
        let add = |auth, origin: Option<&str>| {
            let mut r = request(auth, "POST", "/api/wishlist", true);
            r.headers_mut().unwrap().remove("origin");
            if let Some(o) = origin {
                r = r.header("origin", o);
            }
            r.body(B::from(json!({"item_id": 1}).to_string())).unwrap()
        };
        for origin in [Some("https://evil"), None] {
            let (s, _, v) = send(&pool, add(Auth::Session(&c), origin)).await;
            assert_eq!(s, StatusCode::FORBIDDEN, "{origin:?}");
            assert_eq!(v, json!({"error": "cross-site request"}));
        }
        let req = add(Auth::Bearer(&token), Some("https://evil"));
        assert_eq!(send(&pool, req).await.0, StatusCode::NO_CONTENT);
        let req = add(Auth::Session(&c), Some("http://localhost:3000"));
        assert_eq!(send(&pool, req).await.0, StatusCode::NO_CONTENT);
        // Reads don't need our Origin.
        let mut req = request(Auth::Session(&c), "GET", "/api/wishlist", false);
        req.headers_mut().unwrap().remove("origin");
        let req = req
            .header("origin", "https://evil")
            .body(B::empty())
            .unwrap();
        assert_eq!(send(&pool, req).await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn expired_session_is_401() {
        let pool = test_pool().await;
        let c = login(&pool, 1).await;
        let (s, _) = call(&pool, Auth::Session(&c), "GET", "/api/wishlist", None).await;
        assert_eq!(s, StatusCode::OK);
        sqlx::query("UPDATE sessions SET expires_at = now() - interval '1 second'")
            .execute(&pool)
            .await
            .unwrap();
        let (s, v) = call(&pool, Auth::Session(&c), "GET", "/api/wishlist", None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert_eq!(v["error"], "sign in required");
    }
}
