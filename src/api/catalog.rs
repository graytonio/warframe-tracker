use crate::AppState;
use crate::api::{ApiError, ApiResult, CurrentUser};
use crate::models::{ITEM_COLS, ItemSummary, item_from_row};
use crate::tree::{ProgressNode, load_tree};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::get,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{AssertSqlSafe, PgPool, Row};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/items", get(search))
        .route("/items/{id}", get(detail))
        .route("/meta", get(meta))
}

#[derive(Deserialize)]
struct SearchParams {
    q: Option<String>,
    category: Option<String>,
    limit: Option<i64>,
}

async fn search(
    State(st): State<AppState>,
    Query(p): Query<SearchParams>,
) -> ApiResult<Json<Vec<ItemSummary>>> {
    let sql = format!(
        "SELECT {ITEM_COLS} FROM items i
         WHERE i.removed_at IS NULL
           AND ($1 = '' OR i.wiki_name ILIKE '%' || $1 || '%')
           AND ($2 = '' OR i.category = $2)
         ORDER BY (lower(i.wiki_name) = lower($1)) DESC, length(i.wiki_name), i.wiki_name
         LIMIT $3"
    );
    let rows = sqlx::query(AssertSqlSafe(sql))
        .bind(p.q.unwrap_or_default())
        .bind(p.category.unwrap_or_default())
        .bind(p.limit.unwrap_or(25).clamp(1, 100))
        .fetch_all(&st.pool)
        .await?;
    Ok(Json(rows.iter().map(item_from_row).collect()))
}

#[derive(Serialize)]
struct Detail {
    item: ItemSummary,
    tree: ProgressNode,
    sources: Vec<Source>,
}

async fn detail(
    State(st): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<i64>,
) -> ApiResult<Json<Detail>> {
    let tree = load_tree(&st.pool, user, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(Detail {
        item: tree.item.clone(),
        sources: item_sources(&st.pool, &[id])
            .await?
            .into_iter()
            .map(|(_, s)| s)
            .collect(),
        tree,
    }))
}

/// Where an item drops; `kind` is a `drop_sources.source_kind` or `"relic"`.
#[derive(Serialize, Clone, Debug)]
pub struct Source {
    pub kind: String,
    pub name: String,
    pub rotation: Option<String>,
    pub chance: Option<f64>,
    pub quantity: Option<i32>,
    /// Relic name when this source drops a relic that drops the item.
    pub via: Option<String>,
    /// Set for `kind = "relic"`.
    pub vaulted: Option<bool>,
    pub rarity: Option<String>,
    /// Conclave (PvP) reward table.
    pub pvp: bool,
    /// Structured place (`drop_sources.planet`); for a relic route, the place of the
    /// source that drops the relic.
    pub planet: Option<String>,
}

/// Direct drops, relics that drop each item, and where those relics drop (`via`),
/// best chance first. Returns `(item_id, source)` pairs for all `ids` in one query.
pub async fn item_sources(pool: &PgPool, ids: &[i64]) -> sqlx::Result<Vec<(i64, Source)>> {
    let rows = sqlx::query(
        r#"
        SELECT d.item_id AS need, d.source_kind AS kind, d.source_name AS name, d.rotation,
               d.chance::float8 AS chance, d.quantity,
               NULL::text AS via, NULL::boolean AS vaulted, NULL::text AS rarity, d.is_pvp AS pvp,
               d.planet
        FROM drop_sources d WHERE d.item_id = ANY($1)
        UNION ALL
        SELECT rd.part_item_id, 'relic', r.wiki_name, NULL, NULL, NULL, NULL, r.is_vaulted, rd.rarity, false,
               NULL
        FROM relic_drops rd JOIN items r ON r.id = rd.relic_item_id
        WHERE rd.part_item_id = ANY($1) AND r.removed_at IS NULL
        UNION ALL
        SELECT rd.part_item_id, d.source_kind, d.source_name, d.rotation, d.chance::float8,
               d.quantity, r.wiki_name, NULL, NULL, d.is_pvp, d.planet
        FROM relic_drops rd JOIN items r ON r.id = rd.relic_item_id
        JOIN drop_sources d ON d.item_id = rd.relic_item_id
        WHERE rd.part_item_id = ANY($1) AND r.removed_at IS NULL
        ORDER BY chance DESC NULLS LAST, name, rotation"#,
    )
    .bind(ids)
    .fetch_all(pool)
    .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let s = Source {
                kind: r.get("kind"),
                name: r.get("name"),
                rotation: r.get("rotation"),
                chance: r.get("chance"),
                quantity: r.get("quantity"),
                via: r.get("via"),
                vaulted: r.get("vaulted"),
                rarity: r.get("rarity"),
                pvp: r.get("pvp"),
                planet: r.get("planet"),
            };
            (r.get("need"), s)
        })
        .collect())
}

#[derive(Serialize)]
struct Meta {
    last_import_at: Option<DateTime<Utc>>,
    item_count: i64,
    sources: Vec<WikiSource>,
}

#[derive(Serialize)]
struct WikiSource {
    title: String,
    rev_id: i64,
    fetched_at: DateTime<Utc>,
}

async fn meta(State(st): State<AppState>) -> ApiResult<Json<Meta>> {
    let (last_import_at, item_count): (Option<DateTime<Utc>>, i64) = sqlx::query_as(
        "SELECT (SELECT max(finished_at) FROM import_runs),
                (SELECT count(*) FROM items WHERE removed_at IS NULL)",
    )
    .fetch_one(&st.pool)
    .await?;
    let sources: Vec<(String, i64, DateTime<Utc>)> =
        sqlx::query_as("SELECT title, rev_id, fetched_at FROM wiki_sources ORDER BY title")
            .fetch_all(&st.pool)
            .await?;
    Ok(Json(Meta {
        last_import_at,
        item_count,
        sources: sources
            .into_iter()
            .map(|(title, rev_id, fetched_at)| WikiSource {
                title,
                rev_id,
                fetched_at,
            })
            .collect(),
    }))
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

    async fn call(pool: &sqlx::PgPool, uri: &str) -> (StatusCode, Value) {
        let req = Request::builder().uri(uri).body(Body::empty()).unwrap();
        let res = app(
            AppState::new(pool.clone(), crate::config::Config::for_tests()),
            "web/dist",
        )
        .oneshot(req)
        .await
        .unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    /// Saryn Prime ← Chassis ← Rubedo ×1200. Chassis drops from relic Axi S1 (vaulted),
    /// which drops from mission "Earth / Mantle" rot C; Rubedo also from planet Earth.
    async fn seed(pool: &sqlx::PgPool) {
        sqlx::raw_sql(
            r#"
          INSERT INTO items (id, wiki_name, category, is_prime, is_vaulted, removed_at) VALUES
            (1,'Saryn Prime','warframe',true,null,null),(2,'Saryn Prime Chassis','component',true,true,null),
            (3,'Rubedo','resource',false,null,null),(4,'Axi S1','relic',false,true,null),
            (5,'Saryn Old','warframe',false,null,now());
          INSERT INTO recipes VALUES (1,2,1),(2,3,1200);
          INSERT INTO relic_drops VALUES (4,2,'Rare');
          INSERT INTO drop_sources (source_kind,source_name,rotation,item_id,chance,quantity,planet) VALUES
            ('planet','Earth',null,3,null,null,'Earth'),('mission','Earth / Mantle','C',4,12.5,1,'Earth');
        "#,
        )
        .execute(pool)
        .await
        .unwrap();
    }

    fn names(v: &Value) -> Vec<&str> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|i| i["name"].as_str().unwrap())
            .collect()
    }

    #[tokio::test]
    async fn search_by_name_and_category_skips_removed() {
        let pool = test_pool().await;
        seed(&pool).await;
        let (s, v) = call(&pool, "/api/items?q=saryn").await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(names(&v), ["Saryn Prime", "Saryn Prime Chassis"]);
        assert_eq!(v[0]["is_prime"], true);
        assert_eq!(
            names(&call(&pool, "/api/items?category=resource").await.1),
            ["Rubedo"]
        );
        assert_eq!(
            call(&pool, "/api/items?limit=1")
                .await
                .1
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            call(&pool, "/api/items?limit=0")
                .await
                .1
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn detail_has_tree_and_sources() {
        let pool = test_pool().await;
        seed(&pool).await;
        let (s, v) = call(&pool, "/api/items/1").await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["item"]["name"], "Saryn Prime");
        assert_eq!(
            v["tree"]["children"][0]["item"]["name"],
            "Saryn Prime Chassis"
        );
        assert_eq!(v["sources"], json!([]));

        let v = call(&pool, "/api/items/2").await.1;
        let relic = v["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["kind"] == "relic")
            .unwrap();
        assert_eq!(
            relic,
            &json!({"kind": "relic", "name": "Axi S1", "rotation": null, "chance": null,
                    "quantity": null, "via": null, "vaulted": true, "rarity": "Rare", "pvp": false,
                    "planet": null})
        );
        let via = v["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["kind"] == "mission")
            .unwrap();
        assert_eq!(
            via,
            &json!({"kind": "mission", "name": "Earth / Mantle", "rotation": "C", "chance": 12.5,
                    "quantity": 1, "via": "Axi S1", "vaulted": null, "rarity": null, "pvp": false,
                    "planet": "Earth"})
        );

        let v = call(&pool, "/api/items/3").await.1;
        assert_eq!(v["sources"][0]["kind"], "planet");
        assert_eq!(v["sources"][0]["name"], "Earth");
    }

    #[tokio::test]
    async fn unknown_item_is_404() {
        let pool = test_pool().await;
        let (s, v) = call(&pool, "/api/items/999").await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        assert_eq!(v["error"], "not found");
    }

    #[tokio::test]
    async fn meta_on_empty_db() {
        let pool = test_pool().await;
        let (s, v) = call(&pool, "/api/meta").await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(
            v,
            json!({"last_import_at": null, "item_count": 0, "sources": []})
        );
    }

    #[tokio::test]
    async fn meta_after_import() {
        let pool = test_pool().await;
        seed(&pool).await;
        sqlx::raw_sql(
            "INSERT INTO import_runs (summary) VALUES ('{}');
             INSERT INTO wiki_sources (title, rev_id, source) VALUES ('Module:Void/data', 42, 'return {}');",
        )
        .execute(&pool)
        .await
        .unwrap();
        let v = call(&pool, "/api/meta").await.1;
        assert!(v["last_import_at"].is_string());
        assert_eq!(v["item_count"], 4);
        assert_eq!(v["sources"][0]["title"], "Module:Void/data");
        assert_eq!(v["sources"][0]["rev_id"], 42);
        assert!(v["sources"][0]["fetched_at"].is_string());
        assert!(v["sources"][0].get("source").is_none());
    }
}
