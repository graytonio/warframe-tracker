//! Sign in with Discord (OAuth2 code flow), logout, and localhost-only dev login.
//! Discord's access token is used once to read `/users/@me` and then dropped:
//! it is never stored or logged.

use crate::AppState;
use crate::api::{ApiError, ApiResult, CurrentUser, SESSION_COOKIE};
use crate::config::Config;
use axum::{
    Router,
    extract::{Query, State},
    http::{StatusCode, header::SET_COOKIE},
    response::{AppendHeaders, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use axum_extra::extract::cookie::CookieJar;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use sqlx::PgPool;
use std::time::Duration;

const OAUTH_COOKIE: &str = "wft_oauth";

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/discord/login", get(login))
        .route("/discord/callback", get(callback))
        .route("/logout", post(logout))
        .route("/dev-login", get(dev_login))
}

/// A same-site relative path, else `/`. Blocks `//host`, `/\host` and schemes.
pub fn safe_return_to(s: Option<&str>) -> String {
    match s {
        Some(p)
            if p.starts_with('/')
                && !p.starts_with("//")
                && !p.contains('\\')
                && p.chars().all(|c| c.is_ascii_graphic()) =>
        {
            p.to_string()
        }
        _ => "/".into(),
    }
}

fn cookie(c: &Config, name: &str, value: &str, path: &str, max_age: i64) -> String {
    let secure = if c.secure_cookies() { "; Secure" } else { "" };
    format!("{name}={value}; HttpOnly; SameSite=Lax; Path={path}; Max-Age={max_age}{secure}")
}

fn session_cookie(c: &Config, value: &str) -> String {
    cookie(
        c,
        SESSION_COOKIE,
        value,
        "/",
        crate::sessions::TTL_DAYS * 86400,
    )
}

pub(crate) fn clear_session(c: &Config) -> String {
    cookie(c, SESSION_COOKIE, "", "/", 0)
}

fn clear_oauth(c: &Config) -> String {
    cookie(c, OAUTH_COOKIE, "", "/auth", 0)
}

fn redirect_uri(c: &Config) -> String {
    format!("{}/auth/discord/callback", c.public_url)
}

#[derive(Deserialize)]
struct LoginParams {
    return_to: Option<String>,
}

async fn login(State(st): State<AppState>, Query(p): Query<LoginParams>) -> ApiResult<Response> {
    let c = &st.config;
    let client_id = c.discord_client_id.as_deref().ok_or(ApiError::NotFound)?;
    let state = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
    let back = safe_return_to(p.return_to.as_deref());
    // `state.base64(return_to)`: both halves are cookie-safe.
    let value = format!("{state}.{}", URL_SAFE_NO_PAD.encode(&back));
    let url = reqwest::Url::parse_with_params(
        "https://discord.com/oauth2/authorize",
        [
            ("response_type", "code"),
            ("client_id", client_id),
            ("scope", "identify"),
            ("state", &state),
            ("redirect_uri", &redirect_uri(c)),
        ],
    )
    .map_err(|e| ApiError::Internal(e.to_string()))?;
    let set = cookie(c, OAUTH_COOKIE, &value, "/auth", 600);
    Ok((
        AppendHeaders([(SET_COOKIE, set)]),
        Redirect::to(url.as_str()),
    )
        .into_response())
}

#[derive(Deserialize)]
struct CallbackParams {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
}

#[derive(Deserialize)]
struct DiscordUser {
    id: String,
    username: String,
    global_name: Option<String>,
    avatar: Option<String>,
}

/// Logs only the status: Discord bodies can carry tokens.
fn discord_failed(step: &str, status: Option<StatusCode>) -> ApiError {
    let status = status.map_or("request error".to_string(), |s| s.to_string());
    eprintln!("discord {step} failed: {status}");
    ApiError::BadGateway("discord sign-in failed".into())
}

async fn callback(
    State(st): State<AppState>,
    jar: CookieJar,
    Query(p): Query<CallbackParams>,
) -> ApiResult<Response> {
    let c = &st.config;
    let saved = jar.get(OAUTH_COOKIE).map(|k| k.value().to_string());
    let (saved_state, back) = saved
        .as_deref()
        .and_then(|v| v.split_once('.'))
        .unwrap_or_default();
    if p.state
        .as_deref()
        .is_none_or(|s| s.is_empty() || s != saved_state)
    {
        return Err(ApiError::BadRequest("login state mismatch".into()));
    }
    let clear = (SET_COOKIE, clear_oauth(c));
    if p.error.is_some() {
        let r = Redirect::to("/?login=cancelled");
        return Ok((AppendHeaders([clear]), r).into_response());
    }
    let code = p.code.ok_or(ApiError::BadRequest("missing code".into()))?;
    let (Some(id), Some(secret)) = (&c.discord_client_id, &c.discord_client_secret) else {
        return Err(ApiError::NotFound);
    };
    let http = reqwest::Client::new();
    let base = &c.discord_api_base;
    let res = http
        .post(format!("{base}/oauth2/token"))
        .basic_auth(id, Some(secret))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect_uri(c).as_str()),
        ])
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| discord_failed("token exchange", e.status()))?;
    if !res.status().is_success() {
        return Err(discord_failed("token exchange", Some(res.status())));
    }
    let token: TokenResponse = res
        .json()
        .await
        .map_err(|_| discord_failed("token exchange", None))?;
    let res = http
        .get(format!("{base}/users/@me"))
        .bearer_auth(&token.access_token)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| discord_failed("users/@me", e.status()))?;
    drop(token);
    if !res.status().is_success() {
        return Err(discord_failed("users/@me", Some(res.status())));
    }
    let u: DiscordUser = res
        .json()
        .await
        .map_err(|_| discord_failed("users/@me", None))?;
    let name = u.global_name.unwrap_or(u.username);
    let owner = c.owner_discord_id.as_deref();
    let user = upsert_discord_user(&st.pool, owner, &u.id, &name, u.avatar.as_deref()).await?;
    let back = URL_SAFE_NO_PAD
        .decode(back)
        .ok()
        .and_then(|b| String::from_utf8(b).ok());
    sign_in(&st, user, &safe_return_to(back.as_deref()), Some(clear)).await
}

/// New session, cookie, redirect. Also the moment to purge everyone's expired sessions.
async fn sign_in(
    st: &AppState,
    user: i64,
    back: &str,
    extra: Option<(axum::http::HeaderName, String)>,
) -> ApiResult<Response> {
    crate::sessions::purge_expired(&st.pool).await?;
    let session = crate::sessions::create(&st.pool, user).await?;
    let mut headers = vec![(SET_COOKIE, session_cookie(&st.config, &session))];
    headers.extend(extra);
    Ok((AppendHeaders(headers), Redirect::to(back)).into_response())
}

/// The user for this Discord account, updating its profile. The owner's first login
/// claims the legacy user 1 (and all its data); nobody else ever can.
pub async fn upsert_discord_user(
    pool: &PgPool,
    owner: Option<&str>,
    discord_id: &str,
    display_name: &str,
    avatar: Option<&str>,
) -> sqlx::Result<i64> {
    let mut tx = pool.begin().await?;
    let found: Option<i64> = sqlx::query_scalar(
        "UPDATE users SET display_name = $2, avatar = $3, last_login_at = now()
         WHERE discord_id = $1 RETURNING id",
    )
    .bind(discord_id)
    .bind(display_name)
    .bind(avatar)
    .fetch_optional(&mut *tx)
    .await?;
    let claimed = match found {
        Some(id) => Some(id),
        None if owner == Some(discord_id) => {
            sqlx::query_scalar(
                "UPDATE users SET discord_id = $1, display_name = $2, avatar = $3,
                     last_login_at = now()
                 WHERE id = 1 AND discord_id IS NULL RETURNING id",
            )
            .bind(discord_id)
            .bind(display_name)
            .bind(avatar)
            .fetch_optional(&mut *tx)
            .await?
        }
        None => None,
    };
    let id = match claimed {
        Some(id) => id,
        // ON CONFLICT: a concurrent first login for the same account wins the insert.
        None => {
            sqlx::query_scalar(
                "INSERT INTO users (name, discord_id, display_name, avatar, last_login_at)
                 VALUES ($2, $1, $2, $3, now())
                 ON CONFLICT (discord_id) DO UPDATE
                 SET display_name = EXCLUDED.display_name, avatar = EXCLUDED.avatar,
                     last_login_at = now()
                 RETURNING id",
            )
            .bind(discord_id)
            .bind(display_name)
            .bind(avatar)
            .fetch_one(&mut *tx)
            .await?
        }
    };
    tx.commit().await?;
    Ok(id)
}

async fn logout(
    State(st): State<AppState>,
    CurrentUser(_): CurrentUser,
    jar: CookieJar,
) -> ApiResult<Response> {
    if let Some(k) = jar.get(SESSION_COOKIE) {
        crate::sessions::delete(&st.pool, k.value()).await?;
    }
    let clear = clear_session(&st.config);
    Ok((StatusCode::NO_CONTENT, AppendHeaders([(SET_COOKIE, clear)])).into_response())
}

#[derive(Deserialize)]
struct DevLoginParams {
    #[serde(rename = "as")]
    name: Option<String>,
    return_to: Option<String>,
}

/// Signs in as `dev:<name>` without Discord. Only with `DEV_LOGIN=1` on localhost.
async fn dev_login(
    State(st): State<AppState>,
    Query(p): Query<DevLoginParams>,
) -> ApiResult<Response> {
    if !st.config.dev_login {
        return Err(ApiError::NotFound);
    }
    let name = p.name.unwrap_or_else(|| "dev".into());
    if name.is_empty() || name.len() > 64 {
        return Err(ApiError::BadRequest("name must be 1-64 characters".into()));
    }
    let user = upsert_discord_user(&st.pool, None, &format!("dev:{name}"), &name, None).await?;
    sign_in(&st, user, &safe_return_to(p.return_to.as_deref()), None).await
}
#[cfg(test)]
mod tests {
    use super::safe_return_to;
    use crate::api::test_support::{Auth, call, login, request, send_with};
    use crate::config::Config;
    use crate::db::test_pool;
    use axum::{
        Form, Json, Router,
        body::Body,
        http::{HeaderMap, StatusCode, header},
        routing::{get, post},
    };
    use serde_json::json;
    use std::collections::HashMap;

    /// Fake Discord on a random port: one token, one user `111`.
    async fn discord() -> String {
        let app = Router::new()
            .route(
                "/oauth2/token",
                post(|h: HeaderMap, Form(f): Form<HashMap<String, String>>| async move {
                    let auth = h[header::AUTHORIZATION].to_str().unwrap();
                    assert!(auth.starts_with("Basic "));
                    assert_eq!(f["grant_type"], "authorization_code");
                    assert_eq!(f["code"], "c");
                    assert_eq!(
                        f["redirect_uri"],
                        "http://localhost:3000/auth/discord/callback"
                    );
                    Json(json!({"access_token": "discord-secret-at", "token_type": "Bearer"}))
                }),
            )
            .route(
                "/users/@me",
                get(|h: HeaderMap| async move {
                    assert_eq!(h[header::AUTHORIZATION], "Bearer discord-secret-at");
                    Json(json!({"id": "111", "username": "tenno", "global_name": "Tenno", "avatar": "abc"}))
                }),
            );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        format!("http://{addr}")
    }

    async fn config(owner: Option<&str>) -> Config {
        Config {
            discord_client_id: Some("cid".into()),
            discord_client_secret: Some("csecret".into()),
            discord_api_base: discord().await,
            owner_discord_id: owner.map(Into::into),
            ..Config::for_tests()
        }
    }

    async fn fetch(
        pool: &sqlx::PgPool,
        c: &Config,
        uri: &str,
        cookie: Option<&str>,
    ) -> (StatusCode, HeaderMap) {
        let mut r = request(Auth::None, "GET", uri, false);
        if let Some(k) = cookie {
            r = r.header("cookie", k);
        }
        let (s, h, _) = send_with(pool, c.clone(), r.body(Body::empty()).unwrap()).await;
        (s, h)
    }

    /// `(value, whole header)` of the Set-Cookie for `name`.
    fn set_cookie(h: &HeaderMap, name: &str) -> (String, String) {
        let full = h
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap().to_string())
            .find(|v| v.starts_with(&format!("{name}=")))
            .unwrap_or_else(|| panic!("no {name} cookie in {h:?}"));
        let value = full.split(';').next().unwrap()[name.len() + 1..].to_string();
        (value, full)
    }

    fn location(h: &HeaderMap) -> &str {
        h[header::LOCATION].to_str().unwrap()
    }

    /// Starts a login; returns `(state, "wft_oauth=…")`.
    async fn start(pool: &sqlx::PgPool, c: &Config, back: &str) -> (String, String) {
        let uri = format!("/auth/discord/login?return_to={back}");
        let (s, h) = fetch(pool, c, &uri, None).await;
        assert_eq!(s, StatusCode::SEE_OTHER);
        let url = reqwest::Url::parse(location(&h)).unwrap();
        let state = url.query_pairs().find(|(k, _)| k == "state").unwrap().1;
        let oauth = format!("wft_oauth={}", set_cookie(&h, "wft_oauth").0);
        (state.into_owned(), oauth)
    }

    /// Full login as Discord user 111; returns the session cookie value.
    async fn sign_in(pool: &sqlx::PgPool, c: &Config) -> String {
        let (state, oauth) = start(pool, c, "/").await;
        let uri = format!("/auth/discord/callback?code=c&state={state}");
        let (s, h) = fetch(pool, c, &uri, Some(&oauth)).await;
        assert_eq!(s, StatusCode::SEE_OTHER);
        set_cookie(&h, "wft_session").0
    }

    async fn count(pool: &sqlx::PgPool, sql: &'static str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
    }

    #[tokio::test]
    async fn login_redirects_to_discord_with_state_cookie() {
        let pool = test_pool().await;
        let c = config(None).await;
        let (s, h) = fetch(&pool, &c, "/auth/discord/login?return_to=/plan", None).await;
        assert_eq!(s, StatusCode::SEE_OTHER);
        let url = reqwest::Url::parse(location(&h)).unwrap();
        assert_eq!(
            url.as_str().split('?').next(),
            Some("https://discord.com/oauth2/authorize")
        );
        let q: HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["client_id"], "cid");
        assert_eq!(q["scope"], "identify");
        assert_eq!(
            q["redirect_uri"],
            "http://localhost:3000/auth/discord/callback"
        );
        assert!(q["state"].len() >= 43);
        let (_, full) = set_cookie(&h, "wft_oauth");
        assert!(
            full.contains("HttpOnly") && full.contains("Path=/auth"),
            "{full}"
        );
        assert!(full.contains("SameSite=Lax") && full.contains("Max-Age=600"));
        assert!(!full.contains("Secure"), "localhost cookies aren't Secure");
    }

    #[tokio::test]
    async fn cookies_are_secure_off_localhost() {
        let pool = test_pool().await;
        let c = Config {
            public_url: "https://x.example".into(),
            dev_login: false,
            ..config(None).await
        };
        let (_, h) = fetch(&pool, &c, "/auth/discord/login", None).await;
        assert!(set_cookie(&h, "wft_oauth").1.ends_with("; Secure"));
    }

    #[tokio::test]
    async fn callback_rejects_state_mismatch() {
        let pool = test_pool().await;
        let c = config(None).await;
        let (state, oauth) = start(&pool, &c, "/").await;
        let with_state = format!("/auth/discord/callback?code=c&state={state}");
        for (uri, cookie) in [
            (
                "/auth/discord/callback?code=c&state=wrong",
                Some(oauth.as_str()),
            ),
            (with_state.as_str(), None),
            ("/auth/discord/callback?code=c", Some(oauth.as_str())),
            (
                "/auth/discord/callback?code=c&state=",
                Some("wft_oauth=.Lw"),
            ),
        ] {
            let (s, h) = fetch(&pool, &c, uri, cookie).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{uri} {cookie:?}");
            assert!(h.get(header::SET_COOKIE).is_none());
        }
    }

    #[tokio::test]
    async fn callback_happy_path() {
        let pool = test_pool().await;
        let c = config(None).await;
        let (state, oauth) = start(&pool, &c, "/plan").await;
        let uri = format!("/auth/discord/callback?code=c&state={state}");
        let (s, h) = fetch(&pool, &c, &uri, Some(&oauth)).await;
        assert_eq!(s, StatusCode::SEE_OTHER);
        assert_eq!(location(&h), "/plan");
        let (session, full) = set_cookie(&h, "wft_session");
        assert!(
            full.contains("HttpOnly") && full.contains("SameSite=Lax"),
            "{full}"
        );
        assert!(
            full.contains("Path=/;") && full.contains("Max-Age=2592000"),
            "{full}"
        );
        assert!(set_cookie(&h, "wft_oauth").1.contains("Max-Age=0"));
        let row: (String, String, String) =
            sqlx::query_as("SELECT discord_id, display_name, avatar FROM users WHERE id <> 1")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row, ("111".into(), "Tenno".into(), "abc".into()));
        assert_eq!(count(&pool, "SELECT count(*) FROM sessions").await, 1);
        let (s, v) = call(&pool, Auth::Session(&session), "GET", "/api/me", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["display_name"], "Tenno");
        assert_eq!(
            v["avatar_url"],
            "https://cdn.discordapp.com/avatars/111/abc.png?size=64"
        );
        assert!(v["id"].as_i64().unwrap() > 1);
        // Neither Discord's token nor our cookie is stored anywhere.
        for t in ["sessions", "users", "api_tokens"] {
            let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT count(*) FROM {t} x
                 WHERE x::text LIKE '%discord-secret-at%' OR strpos(x::text, $1) > 0"
            )))
            .bind(&session)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(n, 0, "{t}");
        }
    }

    #[tokio::test]
    async fn callback_access_denied_goes_home() {
        let pool = test_pool().await;
        let c = config(None).await;
        let (state, oauth) = start(&pool, &c, "/plan").await;
        let uri = format!("/auth/discord/callback?error=access_denied&state={state}");
        let (s, h) = fetch(&pool, &c, &uri, Some(&oauth)).await;
        assert_eq!(s, StatusCode::SEE_OTHER);
        assert_eq!(location(&h), "/?login=cancelled");
        assert!(set_cookie(&h, "wft_oauth").1.contains("Max-Age=0"));
        assert_eq!(count(&pool, "SELECT count(*) FROM sessions").await, 0);
    }

    #[tokio::test]
    async fn same_discord_id_same_user_updates_profile() {
        let pool = test_pool().await;
        let c = config(None).await;
        sign_in(&pool, &c).await;
        sqlx::query(
            "UPDATE users SET display_name = 'Old', avatar = NULL WHERE discord_id = '111'",
        )
        .execute(&pool)
        .await
        .unwrap();
        sign_in(&pool, &c).await;
        let rows: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT display_name, avatar FROM users WHERE discord_id = '111'")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert_eq!(rows, [("Tenno".into(), Some("abc".into()))]);
    }

    #[tokio::test]
    async fn owner_claims_user_1_once() {
        let pool = test_pool().await;
        sqlx::raw_sql(
            "INSERT INTO items (id, wiki_name, category) VALUES (1,'Saryn Prime','warframe');
             INSERT INTO wishlist (user_id, item_id) VALUES (1, 1);",
        )
        .execute(&pool)
        .await
        .unwrap();
        let c = config(Some("111")).await;
        let before = count(&pool, "SELECT count(*) FROM users").await;
        for _ in 0..2 {
            let session = sign_in(&pool, &c).await;
            let (s, v) = call(&pool, Auth::Session(&session), "GET", "/api/wishlist", None).await;
            assert_eq!(s, StatusCode::OK);
            assert_eq!(v[0]["item"]["name"], "Saryn Prime");
        }
        assert_eq!(count(&pool, "SELECT count(*) FROM users").await, before);
        let id: Option<String> = sqlx::query_scalar("SELECT discord_id FROM users WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(id.as_deref(), Some("111"));
    }

    #[tokio::test]
    async fn non_owner_never_claims() {
        let pool = test_pool().await;
        let c = config(Some("999")).await;
        sign_in(&pool, &c).await;
        let id: i64 = sqlx::query_scalar("SELECT id FROM users WHERE discord_id = '111'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_ne!(id, 1);
        let one: Option<String> = sqlx::query_scalar("SELECT discord_id FROM users WHERE id = 1")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(one, None);
    }

    #[test]
    fn return_to_is_sanitised() {
        for bad in [
            "https://evil",
            "//evil",
            "/\\evil",
            "javascript:alert(1)",
            "",
            "/a\r\nSet-Cookie: x",
            "/a b",
        ] {
            assert_eq!(safe_return_to(Some(bad)), "/", "{bad:?}");
        }
        assert_eq!(safe_return_to(Some("/plan?x=1")), "/plan?x=1");
        assert_eq!(safe_return_to(None), "/");
    }

    #[tokio::test]
    async fn external_return_to_redirects_home() {
        let pool = test_pool().await;
        let c = config(None).await;
        let (state, oauth) = start(&pool, &c, "//evil").await;
        let uri = format!("/auth/discord/callback?code=c&state={state}");
        let (_, h) = fetch(&pool, &c, &uri, Some(&oauth)).await;
        assert_eq!(location(&h), "/");
    }

    #[tokio::test]
    async fn logout_kills_session() {
        let pool = test_pool().await;
        let c = login(&pool, 1).await;
        let r = request(Auth::Session(&c), "POST", "/auth/logout", false);
        let (s, h, _) = send_with(&pool, Config::for_tests(), r.body(Body::empty()).unwrap()).await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        assert!(set_cookie(&h, "wft_session").1.contains("Max-Age=0"));
        let (s, _) = call(&pool, Auth::Session(&c), "GET", "/api/wishlist", None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert_eq!(count(&pool, "SELECT count(*) FROM sessions").await, 0);
        // Cross-site logout is refused; signed-out logout is 401.
        let c = login(&pool, 1).await;
        let mut r = request(Auth::Session(&c), "POST", "/auth/logout", false);
        r.headers_mut()
            .unwrap()
            .insert("origin", "https://evil".parse().unwrap());
        let (s, _, _) = send_with(&pool, Config::for_tests(), r.body(Body::empty()).unwrap()).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _) = call(&pool, Auth::None, "POST", "/auth/logout", None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn dev_login_only_on_localhost() {
        let pool = test_pool().await;
        let uri = "/auth/dev-login?as=alice";
        let (s, h) = fetch(&pool, &Config::for_tests(), uri, None).await;
        assert_eq!(s, StatusCode::SEE_OTHER);
        assert_eq!(location(&h), "/");
        let session = set_cookie(&h, "wft_session").0;
        let (s, _) = call(&pool, Auth::Session(&session), "GET", "/api/wishlist", None).await;
        assert_eq!(s, StatusCode::OK);
        let sql = "SELECT count(*) FROM users WHERE discord_id = 'dev:alice'";
        assert_eq!(count(&pool, sql).await, 1);
        let c = Config {
            public_url: "https://x".into(),
            dev_login: false,
            ..Config::for_tests()
        };
        let (s, _) = fetch(&pool, &c, uri, None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
    }
}
