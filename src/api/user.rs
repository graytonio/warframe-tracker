use crate::AppState;
use crate::api::{ApiError, ApiResult, CurrentUser};
use crate::models::{ITEM_COLS, ItemSummary, item_from_row};
use crate::tree::{ProgressNode, load_tree};
use axum::{
    Json, Router,
    extract::{Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, patch, put},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{AssertSqlSafe, PgExecutor, Row};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/wishlist", get(wishlist).post(wishlist_add))
        .route(
            "/wishlist/{item_id}",
            patch(wishlist_patch).delete(wishlist_delete),
        )
        .route("/progress/{item_id}", put(progress_put))
        .route("/inventory", get(inventory).put(inventory_put))
        .route("/inventory/{item_id}", patch(inventory_patch))
}

/// Writes go through `INSERT … SELECT FROM items WHERE removed_at IS NULL`,
/// so zero rows affected means the item is unknown or removed.
fn found(rows: u64) -> ApiResult<StatusCode> {
    if rows == 0 {
        Err(ApiError::NotFound)
    } else {
        Ok(StatusCode::NO_CONTENT)
    }
}

#[derive(Serialize)]
struct WishlistEntry {
    item: ItemSummary,
    added_at: DateTime<Utc>,
    completed_at: Option<DateTime<Utc>>,
    progress_pct: f64,
    tree: ProgressNode,
}

async fn wishlist(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> ApiResult<Json<Vec<WishlistEntry>>> {
    let rows: Vec<(i64, DateTime<Utc>, Option<DateTime<Utc>>)> = sqlx::query_as(
        "SELECT item_id, added_at, completed_at FROM wishlist
         WHERE user_id = $1 ORDER BY added_at, item_id",
    )
    .bind(user)
    .fetch_all(&st.pool)
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    // ponytail: one load_tree per entry; batch it if wishlists get long.
    for (item_id, added_at, completed_at) in rows {
        if let Some(tree) = load_tree(&st.pool, user, item_id).await? {
            out.push(WishlistEntry {
                item: tree.item.clone(),
                added_at,
                completed_at,
                progress_pct: tree.progress_pct(),
                tree,
            });
        }
    }
    Ok(Json(out))
}

#[derive(Deserialize)]
struct WishlistAdd {
    item_id: i64,
}

async fn wishlist_add(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Json(b): Json<WishlistAdd>,
) -> ApiResult<StatusCode> {
    // The no-op update makes a repeat add count as one row, keeping it idempotent.
    let r = sqlx::query(
        "INSERT INTO wishlist (user_id, item_id)
         SELECT $1, id FROM items WHERE id = $2 AND removed_at IS NULL
         ON CONFLICT (user_id, item_id) DO UPDATE SET item_id = EXCLUDED.item_id",
    )
    .bind(user)
    .bind(b.item_id)
    .execute(&st.pool)
    .await?;
    found(r.rows_affected())
}

#[derive(Deserialize)]
struct WishlistPatch {
    completed: bool,
}

async fn wishlist_patch(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(item_id): Path<i64>,
    Json(b): Json<WishlistPatch>,
) -> ApiResult<StatusCode> {
    let r = sqlx::query(
        "UPDATE wishlist
         SET completed_at = CASE WHEN $3 THEN COALESCE(completed_at, now()) END
         WHERE user_id = $1 AND item_id = $2",
    )
    .bind(user)
    .bind(item_id)
    .bind(b.completed)
    .execute(&st.pool)
    .await?;
    found(r.rows_affected())
}

async fn wishlist_delete(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(item_id): Path<i64>,
) -> ApiResult<StatusCode> {
    let r = sqlx::query("DELETE FROM wishlist WHERE user_id = $1 AND item_id = $2")
        .bind(user)
        .bind(item_id)
        .execute(&st.pool)
        .await?;
    found(r.rows_affected())
}

#[derive(Deserialize)]
struct ProgressPut {
    obtained: bool,
}

async fn progress_put(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(item_id): Path<i64>,
    Json(b): Json<ProgressPut>,
) -> ApiResult<StatusCode> {
    let r = sqlx::query(
        "INSERT INTO component_progress (user_id, item_id, obtained)
         SELECT $1, id, $3 FROM items WHERE id = $2 AND removed_at IS NULL
         ON CONFLICT (user_id, item_id) DO UPDATE SET obtained = EXCLUDED.obtained",
    )
    .bind(user)
    .bind(item_id)
    .bind(b.obtained)
    .execute(&st.pool)
    .await?;
    found(r.rows_affected())
}

#[derive(Serialize)]
struct InventoryEntry {
    item: ItemSummary,
    count: i64,
    source: String,
    updated_at: DateTime<Utc>,
}

async fn inventory(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> ApiResult<Json<Vec<InventoryEntry>>> {
    let sql = format!(
        "SELECT {ITEM_COLS}, inv.count, inv.source, inv.updated_at
         FROM inventory inv JOIN items i ON i.id = inv.item_id
         WHERE inv.user_id = $1 ORDER BY i.wiki_name"
    );
    let rows = sqlx::query(AssertSqlSafe(sql))
        .bind(user)
        .fetch_all(&st.pool)
        .await?;
    Ok(Json(
        rows.iter()
            .map(|r| InventoryEntry {
                item: item_from_row(r),
                count: r.get("count"),
                source: r.get("source"),
                updated_at: r.get("updated_at"),
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
struct InventoryItem {
    item_id: i64,
    count: i64,
}

#[derive(Deserialize)]
struct InventoryPatch {
    count: i64,
}

fn inventory_source(h: &HeaderMap) -> &'static str {
    if h.get("x-inventory-source").is_some_and(|v| v == "plugin") {
        "plugin"
    } else {
        "manual"
    }
}

fn check_count(count: i64) -> ApiResult<()> {
    if count < 0 {
        return Err(ApiError::BadRequest("count must be >= 0".into()));
    }
    Ok(())
}

async fn upsert_inventory(
    db: impl PgExecutor<'_>,
    user: i64,
    item_id: i64,
    count: i64,
    source: &str,
) -> ApiResult<StatusCode> {
    let r = sqlx::query(
        "INSERT INTO inventory (user_id, item_id, count, source, updated_at)
         SELECT $1, id, $3, $4, now() FROM items WHERE id = $2 AND removed_at IS NULL
         ON CONFLICT (user_id, item_id) DO UPDATE
         SET count = EXCLUDED.count, source = EXCLUDED.source, updated_at = now()",
    )
    .bind(user)
    .bind(item_id)
    .bind(count)
    .bind(source)
    .execute(db)
    .await?;
    found(r.rows_affected())
}

async fn inventory_put(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    headers: HeaderMap,
    Json(items): Json<Vec<InventoryItem>>,
) -> ApiResult<StatusCode> {
    for it in &items {
        check_count(it.count)?;
    }
    let source = inventory_source(&headers);
    let mut tx = st.pool.begin().await?;
    for it in &items {
        upsert_inventory(&mut *tx, user, it.item_id, it.count, source).await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn inventory_patch(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(item_id): Path<i64>,
    Json(b): Json<InventoryPatch>,
) -> ApiResult<StatusCode> {
    check_count(b.count)?;
    upsert_inventory(&st.pool, user, item_id, b.count, "manual").await
}

#[cfg(test)]
mod tests {
    use crate::{AppState, app, db::test_pool};
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    async fn call(
        pool: &sqlx::PgPool,
        method: &str,
        uri: &str,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(uri);
        let body = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        let res = app(
            AppState::new(pool.clone(), crate::config::Config::for_tests()),
            "web/dist",
        )
        .oneshot(req.body(body).unwrap())
        .await
        .unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let v = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap()
        };
        (status, v)
    }

    async fn seed(pool: &sqlx::PgPool) {
        sqlx::raw_sql(
            r#"
          INSERT INTO items (id, wiki_name, category, is_prime) VALUES
            (1,'Saryn Prime','warframe',true),(2,'Saryn Prime Chassis','component',true),(3,'Rubedo','resource',false);
          INSERT INTO recipes VALUES (1,2,1),(2,3,1200);
        "#,
        )
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn wishlist_add_list_complete_delete() {
        let pool = test_pool().await;
        seed(&pool).await;
        let add = || call(&pool, "POST", "/api/wishlist", Some(json!({"item_id": 1})));
        assert_eq!(add().await.0, StatusCode::NO_CONTENT);
        assert_eq!(add().await.0, StatusCode::NO_CONTENT);
        let (s, v) = call(&pool, "GET", "/api/wishlist", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v.as_array().unwrap().len(), 1);
        assert_eq!(v[0]["item"]["name"], "Saryn Prime");
        assert!(v[0]["added_at"].is_string());
        assert!(v[0]["completed_at"].is_null());
        assert_eq!(v[0]["tree"]["children"][0]["children"][0]["needed"], 1200);
        assert_eq!(v[0]["progress_pct"], 0.0);
        let put = call(
            &pool,
            "PUT",
            "/api/progress/2",
            Some(json!({"obtained": true})),
        )
        .await;
        assert_eq!(put.0, StatusCode::NO_CONTENT);
        let v = call(&pool, "GET", "/api/wishlist", None).await.1;
        assert_eq!(v[0]["progress_pct"], 100.0);
        let patch = call(
            &pool,
            "PATCH",
            "/api/wishlist/1",
            Some(json!({"completed": true})),
        )
        .await;
        assert_eq!(patch.0, StatusCode::NO_CONTENT);
        let v = call(&pool, "GET", "/api/wishlist", None).await.1;
        assert!(v[0]["completed_at"].is_string());
        let del = call(&pool, "DELETE", "/api/wishlist/1", None).await;
        assert_eq!(del.0, StatusCode::NO_CONTENT);
        assert_eq!(call(&pool, "GET", "/api/wishlist", None).await.1, json!([]));
        assert_eq!(
            call(&pool, "DELETE", "/api/wishlist/1", None).await.0,
            StatusCode::NOT_FOUND
        );
    }

    #[tokio::test]
    async fn unknown_or_removed_item_writes_are_404() {
        let pool = test_pool().await;
        sqlx::raw_sql(
            "INSERT INTO items (id, wiki_name, category, removed_at) VALUES (5,'Gone','resource',now())",
        )
        .execute(&pool)
        .await
        .unwrap();
        for id in [999999, 5] {
            for (m, u, b) in [
                ("POST", "/api/wishlist".to_string(), json!({"item_id": id})),
                (
                    "PUT",
                    format!("/api/progress/{id}"),
                    json!({"obtained": true}),
                ),
                ("PATCH", format!("/api/inventory/{id}"), json!({"count": 3})),
                (
                    "PATCH",
                    format!("/api/wishlist/{id}"),
                    json!({"completed": true}),
                ),
                (
                    "PUT",
                    "/api/inventory".to_string(),
                    json!([{"item_id": id, "count": 1}]),
                ),
            ] {
                let (s, v) = call(&pool, m, &u, Some(b)).await;
                assert_eq!(s, StatusCode::NOT_FOUND, "{m} {u}");
                assert_eq!(v["error"], "not found");
            }
        }
    }

    #[tokio::test]
    async fn inventory_bulk_put_and_negative_rejected() {
        let pool = test_pool().await;
        seed(&pool).await;
        let req = Request::builder()
            .method("PUT")
            .uri("/api/inventory")
            .header("content-type", "application/json")
            .header("x-inventory-source", "plugin")
            .body(Body::from(r#"[{"item_id":3,"count":500}]"#))
            .unwrap();
        let res = app(
            AppState::new(pool.clone(), crate::config::Config::for_tests()),
            "web/dist",
        )
        .oneshot(req)
        .await
        .unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let (_, v) = call(&pool, "GET", "/api/inventory", None).await;
        assert_eq!(v[0]["item"]["name"], "Rubedo");
        assert_eq!(v[0]["count"], 500);
        assert_eq!(v[0]["source"], "plugin");
        assert!(v[0]["updated_at"].is_string());

        let (s, _) = call(
            &pool,
            "PATCH",
            "/api/inventory/3",
            Some(json!({"count": 7})),
        )
        .await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        let (_, v) = call(&pool, "GET", "/api/inventory", None).await;
        assert_eq!(v[0]["count"], 7);
        assert_eq!(v[0]["source"], "manual");

        let (s, v) = call(
            &pool,
            "PATCH",
            "/api/inventory/3",
            Some(json!({"count": -1})),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(v["error"], "count must be >= 0");
        let bulk = json!([{"item_id": 3, "count": 1}, {"item_id": 2, "count": -1}]);
        let (s, _) = call(&pool, "PUT", "/api/inventory", Some(bulk)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert_eq!(
            call(&pool, "GET", "/api/inventory", None).await.1[0]["count"],
            7
        );
    }

    #[tokio::test]
    async fn inventory_patch_creates_missing_row() {
        let pool = test_pool().await;
        seed(&pool).await;
        let (s, _) = call(
            &pool,
            "PATCH",
            "/api/inventory/3",
            Some(json!({"count": 0})),
        )
        .await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        let (_, v) = call(&pool, "GET", "/api/inventory", None).await;
        assert_eq!(v.as_array().unwrap().len(), 1);
        assert_eq!(v[0]["item"]["id"], 3);
        assert_eq!(v[0]["count"], 0);
        assert_eq!(v[0]["source"], "manual");
    }
}
