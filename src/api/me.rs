use crate::AppState;
use crate::api::{ApiError, ApiResult, CurrentUser, SessionUser};
use crate::tokens::{self, CreateError};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{StatusCode, header::SET_COOKIE},
    response::{AppendHeaders, IntoResponse, Response},
    routing::{delete, get},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/me", get(me).delete(delete_me))
        .route("/tokens", get(list_tokens).post(create_token))
        .route("/tokens/{name}", delete(revoke_token))
}

#[derive(Serialize)]
struct Me {
    id: i64,
    display_name: String,
    avatar_url: Option<String>,
}

async fn me(State(st): State<AppState>, CurrentUser(user): CurrentUser) -> ApiResult<Json<Me>> {
    let (display_name, discord_id, avatar): (String, Option<String>, Option<String>) =
        sqlx::query_as(
            "SELECT COALESCE(display_name, name), discord_id, avatar FROM users WHERE id = $1",
        )
        .bind(user)
        .fetch_one(&st.pool)
        .await?;
    let avatar_url = discord_id
        .zip(avatar)
        .map(|(d, a)| format!("https://cdn.discordapp.com/avatars/{d}/{a}.png?size=64"));
    Ok(Json(Me {
        id: user,
        display_name,
        avatar_url,
    }))
}

/// Cascades remove the user's wishlist, progress, inventory, tokens and sessions.
async fn delete_me(
    State(st): State<AppState>,
    SessionUser(user): SessionUser,
) -> ApiResult<Response> {
    sqlx::query("DELETE FROM users WHERE id = $1")
        .bind(user)
        .execute(&st.pool)
        .await?;
    let clear = crate::auth::clear_session(&st.config);
    Ok((StatusCode::NO_CONTENT, AppendHeaders([(SET_COOKIE, clear)])).into_response())
}

#[derive(Serialize)]
struct TokenRow {
    name: String,
    created_at: DateTime<Utc>,
    last_used_at: Option<DateTime<Utc>>,
}

async fn list_tokens(
    State(st): State<AppState>,
    SessionUser(user): SessionUser,
) -> ApiResult<Json<Vec<TokenRow>>> {
    let rows: Vec<(String, DateTime<Utc>, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT name, created_at, last_used_at FROM api_tokens
         WHERE user_id = $1 AND revoked_at IS NULL ORDER BY created_at, id",
    )
    .bind(user)
    .fetch_all(&st.pool)
    .await?;
    Ok(Json(
        rows.into_iter()
            .map(|(name, created_at, last_used_at)| TokenRow {
                name,
                created_at,
                last_used_at,
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct NewToken {
    name: String,
}

#[derive(Serialize)]
struct CreatedToken {
    name: String,
    token: String,
}

async fn create_token(
    State(st): State<AppState>,
    SessionUser(user): SessionUser,
    Json(b): Json<NewToken>,
) -> ApiResult<(StatusCode, Json<CreatedToken>)> {
    let token = tokens::create(&st.pool, user, &b.name)
        .await
        .map_err(|e| match e {
            CreateError::Invalid(m) => ApiError::BadRequest(m),
            CreateError::Db(e) => e.into(),
        })?;
    Ok((
        StatusCode::CREATED,
        Json(CreatedToken {
            name: b.name,
            token,
        }),
    ))
}

async fn revoke_token(
    State(st): State<AppState>,
    SessionUser(user): SessionUser,
    Path(name): Path<String>,
) -> ApiResult<StatusCode> {
    if tokens::revoke(&st.pool, user, &name).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use crate::api::test_support::{Auth, call, login, request, send};
    use crate::db::test_pool;
    use axum::{body::Body, http::StatusCode};
    use serde_json::json;

    #[tokio::test]
    async fn me_returns_profile_or_401() {
        let pool = test_pool().await;
        let (s, _) = call(&pool, Auth::None, "GET", "/api/me", None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        let c = login(&pool, 1).await;
        let (s, v) = call(&pool, Auth::Session(&c), "GET", "/api/me", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(
            v,
            json!({"id": 1, "display_name": "default", "avatar_url": null})
        );
        sqlx::query("UPDATE users SET discord_id = '42', display_name = 'Tenno', avatar = 'h'")
            .execute(&pool)
            .await
            .unwrap();
        let v = call(&pool, Auth::Session(&c), "GET", "/api/me", None)
            .await
            .1;
        assert_eq!(v["display_name"], "Tenno");
        assert_eq!(
            v["avatar_url"],
            "https://cdn.discordapp.com/avatars/42/h.png?size=64"
        );
    }

    async fn new_token(
        pool: &sqlx::PgPool,
        c: &str,
        name: &str,
    ) -> (StatusCode, serde_json::Value) {
        let body = Some(json!({ "name": name }));
        call(pool, Auth::Session(c), "POST", "/api/tokens", body).await
    }

    #[tokio::test]
    async fn tokens_crud() {
        let pool = test_pool().await;
        let c = login(&pool, 1).await;
        let (s, v) = new_token(&pool, &c, "pc").await;
        assert_eq!(s, StatusCode::CREATED);
        assert_eq!(v["name"], "pc");
        let token = v["token"].as_str().unwrap().to_string();
        let (s, v) = call(&pool, Auth::Session(&c), "GET", "/api/tokens", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v.as_array().unwrap().len(), 1);
        assert_eq!(v[0]["name"], "pc");
        assert!(v[0]["created_at"].is_string() && v[0]["last_used_at"].is_null());
        assert!(v[0].get("token").is_none() && !v.to_string().contains(&token));
        let (s, _) = call(&pool, Auth::Bearer(&token), "GET", "/api/wishlist", None).await;
        assert_eq!(s, StatusCode::OK);
        // The token itself can't manage tokens.
        let (s, _) = call(&pool, Auth::Bearer(&token), "GET", "/api/tokens", None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _) = call(&pool, Auth::Session(&c), "DELETE", "/api/tokens/pc", None).await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        let (s, _) = call(&pool, Auth::Bearer(&token), "GET", "/api/wishlist", None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        let (s, _) = call(&pool, Auth::Session(&c), "DELETE", "/api/tokens/pc", None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let v = call(&pool, Auth::Session(&c), "GET", "/api/tokens", None)
            .await
            .1;
        assert_eq!(v, json!([]));
    }

    #[tokio::test]
    async fn token_names_are_per_user() {
        let pool = test_pool().await;
        let (a, b) = (login(&pool, 1).await, login(&pool, 2).await);
        assert_eq!(new_token(&pool, &a, "pc").await.0, StatusCode::CREATED);
        assert_eq!(new_token(&pool, &b, "pc").await.0, StatusCode::CREATED);
        let (s, v) = new_token(&pool, &a, "pc").await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], "a token named pc already exists");
        // Revoking B's pc leaves A's alone.
        call(&pool, Auth::Session(&b), "DELETE", "/api/tokens/pc", None).await;
        let v = call(&pool, Auth::Session(&a), "GET", "/api/tokens", None)
            .await
            .1;
        assert_eq!(v[0]["name"], "pc");
    }

    #[tokio::test]
    async fn token_name_validation() {
        let pool = test_pool().await;
        let c = login(&pool, 1).await;
        for name in [String::new(), "x".repeat(65)] {
            let (s, v) = new_token(&pool, &c, &name).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{name}");
            assert!(v["error"].is_string());
        }
        assert_eq!(
            new_token(&pool, &c, &"x".repeat(64)).await.0,
            StatusCode::CREATED
        );
    }

    #[tokio::test]
    async fn token_limit_10() {
        let pool = test_pool().await;
        let c = login(&pool, 1).await;
        for i in 0..10 {
            assert_eq!(
                new_token(&pool, &c, &format!("t{i}")).await.0,
                StatusCode::CREATED
            );
        }
        let (s, v) = new_token(&pool, &c, "t10").await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], "at most 10 tokens; revoke one first");
        call(&pool, Auth::Session(&c), "DELETE", "/api/tokens/t0", None).await;
        assert_eq!(new_token(&pool, &c, "t10").await.0, StatusCode::CREATED);
    }

    #[tokio::test]
    async fn wishlist_limit_300() {
        let pool = test_pool().await;
        sqlx::raw_sql(
            "INSERT INTO items (id, wiki_name, category)
               SELECT g, 'Item ' || g, 'resource' FROM generate_series(1, 301) g;
             INSERT INTO wishlist (user_id, item_id) SELECT 1, g FROM generate_series(1, 300) g;",
        )
        .execute(&pool)
        .await
        .unwrap();
        let c = login(&pool, 1).await;
        let add = |id: i64| {
            let c = c.clone();
            let pool = pool.clone();
            async move {
                let b = Some(json!({ "item_id": id }));
                call(&pool, Auth::Session(&c), "POST", "/api/wishlist", b).await
            }
        };
        let (s, v) = add(301).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], "at most 300 wishlist items");
        // Re-adding an item already on the list stays idempotent.
        assert_eq!(add(300).await.0, StatusCode::NO_CONTENT);
    }

    const USER_TABLES: [&str; 5] = [
        "wishlist",
        "inventory",
        "component_progress",
        "api_tokens",
        "sessions",
    ];

    async fn counts(pool: &sqlx::PgPool, user: i64) -> Vec<i64> {
        let mut out = vec![];
        for t in USER_TABLES {
            let sql = format!("SELECT count(*) FROM {t} WHERE user_id = $1");
            out.push(
                sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
                    .bind(user)
                    .fetch_one(pool)
                    .await
                    .unwrap(),
            );
        }
        out
    }

    async fn game_counts(pool: &sqlx::PgPool) -> Vec<i64> {
        let mut out = vec![];
        for t in ["items", "recipes", "drop_sources"] {
            let sql = format!("SELECT count(*) FROM {t}");
            out.push(
                sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
                    .fetch_one(pool)
                    .await
                    .unwrap(),
            );
        }
        out
    }

    #[tokio::test]
    async fn delete_account_only_removes_own_rows() {
        let pool = test_pool().await;
        sqlx::raw_sql(
            "INSERT INTO items (id, wiki_name, category) VALUES (1,'Saryn Prime','warframe'),(2,'Rubedo','resource');
             INSERT INTO recipes VALUES (1,2,10);
             INSERT INTO drop_sources (source_kind, source_name, item_id) VALUES ('planet','Earth',2);",
        )
        .execute(&pool)
        .await
        .unwrap();
        let (a, b) = (login(&pool, 2).await, login(&pool, 3).await);
        for (c, u) in [(&a, 2), (&b, 3)] {
            sqlx::query("INSERT INTO wishlist (user_id, item_id) VALUES ($1, 1)")
                .bind(u)
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query("INSERT INTO inventory (user_id, item_id, count) VALUES ($1, 2, 5)")
                .bind(u)
                .execute(&pool)
                .await
                .unwrap();
            sqlx::query("INSERT INTO component_progress VALUES ($1, 2, true)")
                .bind(u)
                .execute(&pool)
                .await
                .unwrap();
            assert_eq!(new_token(&pool, c, "pc").await.0, StatusCode::CREATED);
        }
        let (a_before, game_before) = (counts(&pool, 2).await, game_counts(&pool).await);
        assert_eq!(a_before, [1, 1, 1, 1, 1]);
        assert_eq!(counts(&pool, 3).await, [1, 1, 1, 1, 1]);
        let req = request(Auth::Session(&b), "DELETE", "/api/me", false)
            .body(Body::empty())
            .unwrap();
        let (s, h, _) = send(&pool, req).await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        let set = h["set-cookie"].to_str().unwrap();
        assert!(set.starts_with("wft_session=;") && set.contains("Max-Age=0"));
        assert_eq!(counts(&pool, 3).await, [0, 0, 0, 0, 0]);
        let gone: i64 = sqlx::query_scalar("SELECT count(*) FROM users WHERE id = 3")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(gone, 0);
        assert_eq!(counts(&pool, 2).await, a_before);
        assert_eq!(game_counts(&pool).await, game_before);
        let (s, _) = call(&pool, Auth::Session(&b), "GET", "/api/me", None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }
}
