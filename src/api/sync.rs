use crate::AppState;
use crate::api::{ApiError, ApiResult, TokenUser};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    routing::post,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

const MAX_PATHS: usize = 20_000;
const MAX_UNMATCHED: usize = 200;

pub fn routes() -> Router<AppState> {
    // 20k long game paths can pass axum's 2 MB default.
    Router::new()
        .route("/sync/inventory", post(sync_inventory))
        .layer(DefaultBodyLimit::max(8 * 1024 * 1024))
}

#[derive(Deserialize)]
struct SyncBody {
    items: HashMap<String, i64>,
    /// Highest owned rank per game path; absent from older clients.
    #[serde(default)]
    ranks: HashMap<String, i64>,
}

#[derive(Serialize)]
struct SyncResult {
    matched: usize,
    updated: u64,
    unmatched: Vec<String>,
}

/// "/Lotus/.../SarynPrimeChassisComponent" -> "saryn prime chassis"
fn component_name(path: &str) -> String {
    let seg = path.rsplit('/').next().unwrap_or(path);
    let seg = seg.strip_suffix("Component").unwrap_or(seg);
    let mut out = String::new();
    for (i, c) in seg.char_indices() {
        if i > 0 && c.is_uppercase() {
            out.push(' ');
        }
        out.extend(c.to_lowercase());
    }
    out
}

/// The game's inventory is the truth for what it reports: matched items become `plugin`
/// rows, `plugin` rows it no longer reports are deleted, `manual` rows it doesn't report stay.
async fn sync_inventory(
    State(st): State<AppState>,
    TokenUser(user): TokenUser,
    Json(b): Json<SyncBody>,
) -> ApiResult<Json<SyncResult>> {
    if b.items.len() > MAX_PATHS {
        return Err(ApiError::BadRequest(format!(
            "at most {MAX_PATHS} items per sync"
        )));
    }
    if b.items.values().any(|c| *c < 0) {
        return Err(ApiError::BadRequest("count must be >= 0".into()));
    }
    let paths: Vec<String> = b.items.keys().cloned().collect();
    let exact: HashMap<String, i64> = sqlx::query_as(
        "SELECT data->>'InternalName', min(id) FROM items
         WHERE removed_at IS NULL AND data->>'InternalName' = ANY($1) GROUP BY 1",
    )
    .bind(&paths)
    .fetch_all(&st.pool)
    .await?
    .into_iter()
    .collect();
    let rest: Vec<(&String, String)> = paths
        .iter()
        .filter(|p| !exact.contains_key(*p))
        .map(|p| (p, component_name(p)))
        .collect();
    let names: Vec<&str> = rest.iter().map(|(_, n)| n.as_str()).collect();
    let by_name: HashMap<String, i64> = sqlx::query_as(
        "SELECT lower(wiki_name), min(id) FROM items
         WHERE removed_at IS NULL AND category = 'component' AND lower(wiki_name) = ANY($1)
         GROUP BY 1",
    )
    .bind(&names)
    .fetch_all(&st.pool)
    .await?
    .into_iter()
    .collect();

    // Several paths can map to one item; sum them so the upsert touches each row once.
    let mut counts: BTreeMap<i64, i64> = BTreeMap::new();
    let mut add = |id: i64, path: &str| {
        let c = counts.entry(id).or_default();
        *c = c.saturating_add(b.items[path]);
    };
    for (path, id) in &exact {
        add(*id, path);
    }
    let mut unmatched = Vec::new();
    for (path, name) in &rest {
        match by_name.get(name) {
            Some(id) => add(*id, path),
            None => unmatched.push((*path).clone()),
        }
    }
    let matched = paths.len() - unmatched.len();
    unmatched.sort();
    unmatched.truncate(MAX_UNMATCHED);

    let (ids, cnts): (Vec<i64>, Vec<i64>) = counts.into_iter().unzip();
    let mut tx = st.pool.begin().await?;
    let updated = sqlx::query(
        "INSERT INTO inventory (user_id, item_id, count, source, updated_at)
         SELECT $1, i, c, 'plugin', now() FROM UNNEST($2::bigint[], $3::bigint[]) AS t(i, c)
         ON CONFLICT (user_id, item_id) DO UPDATE
         SET count = EXCLUDED.count, source = 'plugin', updated_at = now()
         WHERE inventory.count IS DISTINCT FROM EXCLUDED.count OR inventory.source <> 'plugin'",
    )
    .bind(user)
    .bind(&ids)
    .bind(&cnts)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    sqlx::query(
        "DELETE FROM inventory WHERE user_id = $1 AND source = 'plugin' AND item_id <> ALL($2)",
    )
    .bind(user)
    .bind(&ids)
    .execute(&mut *tx)
    .await?;
    // An arcane only counts once a copy is at max rank (wiki `MaxRank`, 5 if unknown).
    // Ticking is one-way: a missing rank never un-ticks a manual or earlier tick.
    let exact_ids: Vec<i64> = exact.values().copied().collect();
    let arcane_max: HashMap<i64, i64> = sqlx::query_as(
        "SELECT id, CASE WHEN data->>'MaxRank' ~ '^[0-9]+$' THEN (data->>'MaxRank')::bigint ELSE 5 END
         FROM items WHERE category = 'arcane' AND id = ANY($1)",
    )
    .bind(&exact_ids)
    .fetch_all(&mut *tx)
    .await?
    .into_iter()
    .collect();
    let maxed: Vec<i64> = exact
        .iter()
        .filter(|(path, id)| {
            arcane_max
                .get(*id)
                .is_some_and(|max| b.ranks.get(*path).is_some_and(|r| r >= max))
        })
        .map(|(_, id)| *id)
        .collect();
    sqlx::query(
        "INSERT INTO component_progress (user_id, item_id, obtained)
         SELECT $1, i, true FROM UNNEST($2::bigint[]) AS t(i)
         ON CONFLICT (user_id, item_id) DO UPDATE SET obtained = true",
    )
    .bind(user)
    .bind(&maxed)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(SyncResult {
        matched,
        updated,
        unmatched,
    }))
}

#[cfg(test)]
mod tests {
    use super::component_name;
    use crate::{AppState, app, db::test_pool};
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use http_body_util::BodyExt;
    use serde_json::{Value, json};
    use tower::ServiceExt;

    const NEURODE: &str = "/Lotus/Types/Items/MiscItems/Neurode";
    const SARYN: &str = "/Lotus/Powersuits/Saryn/SarynPrime";
    const CHASSIS: &str = "/Lotus/Types/Recipes/WarframeRecipes/SarynPrimeChassisComponent";
    const CHASSIS_BP: &str = "/Lotus/Types/Recipes/WarframeRecipes/SarynPrimeChassisBlueprint";
    const SARYN_BP: &str = "/Lotus/Types/Recipes/WarframeRecipes/SarynPrimeBlueprint";

    async fn call(
        pool: &sqlx::PgPool,
        method: &str,
        uri: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(t) = token {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        let body = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        let res = app(AppState { pool: pool.clone() }, "web/dist")
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

    /// Seeds items and returns a token for user 1.
    async fn seed(pool: &sqlx::PgPool) -> String {
        sqlx::raw_sql(
            r#"
          INSERT INTO items (id, wiki_name, category, data) VALUES
            (1,'Neurodes','resource','{"InternalName":"/Lotus/Types/Items/MiscItems/Neurode"}'),
            (2,'Saryn Prime','warframe','{"InternalName":"/Lotus/Powersuits/Saryn/SarynPrime"}'),
            (3,'Saryn Prime Chassis','component','{}'),
            (4,'Saryn Prime Chassis Blueprint','component','{}'),
            (5,'Saryn Prime Blueprint','component','{}'),
            (6,'Rubedo','resource','{}'),
            (7,'Axi S1','relic','{}');
          INSERT INTO recipes VALUES (2,3,1),(2,5,1),(3,4,1),(3,6,1200);
          INSERT INTO relic_drops VALUES (7,4,'Common'),(7,5,'Rare');
          INSERT INTO drop_sources (source_kind,source_name,item_id) VALUES
            ('planet','Earth',6),('mission','Earth/Mantle',7);
        "#,
        )
        .execute(pool)
        .await
        .unwrap();
        crate::tokens::create(pool, 1, "pc").await.unwrap()
    }

    async fn sync(pool: &sqlx::PgPool, t: &str, items: Value) -> Value {
        let (s, v) = call(
            pool,
            "POST",
            "/api/sync/inventory",
            Some(t),
            Some(json!({ "items": items })),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v
    }

    /// `(name, count, source)` rows of user 1's inventory.
    async fn inv(pool: &sqlx::PgPool) -> Vec<(String, i64, String)> {
        let (_, v) = call(pool, "GET", "/api/inventory", None, None).await;
        v.as_array()
            .unwrap()
            .iter()
            .map(|e| {
                (
                    e["item"]["name"].as_str().unwrap().to_string(),
                    e["count"].as_i64().unwrap(),
                    e["source"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    async fn patch(pool: &sqlx::PgPool, id: i64, count: i64) {
        let uri = format!("/api/inventory/{id}");
        let (s, _) = call(pool, "PATCH", &uri, None, Some(json!({ "count": count }))).await;
        assert_eq!(s, StatusCode::NO_CONTENT);
    }

    async fn plan_names(pool: &sqlx::PgPool) -> Vec<String> {
        let p = crate::plan::build_plan(pool, 1, Default::default())
            .await
            .unwrap();
        p.by_need
            .iter()
            .chain(&p.unsourced)
            .map(|n| n.item.name.clone())
            .collect()
    }

    #[tokio::test]
    async fn max_rank_arcane_is_ticked_and_stays_ticked() {
        let pool = test_pool().await;
        let t = seed(&pool).await;
        sqlx::raw_sql(
            r#"INSERT INTO items (id, wiki_name, category, data) VALUES
                 (20,'Arcane Energize','arcane','{"InternalName":"/A/Energize","MaxRank":5}'),
                 (21,'Pax Soar','arcane','{"InternalName":"/A/PaxSoar","MaxRank":3}'),
                 (22,'Arcane Grace','arcane','{"InternalName":"/A/Grace","MaxRank":5}'),
                 (23,'Serration','mod','{"InternalName":"/M/Serration"}')"#,
        )
        .execute(&pool)
        .await
        .unwrap();
        let post = |ranks: Value| {
            call(
                &pool,
                "POST",
                "/api/sync/inventory",
                Some(&t),
                Some(json!({
                    "items": {"/A/Energize": 1, "/A/PaxSoar": 1, "/A/Grace": 1, "/M/Serration": 1},
                    "ranks": ranks
                })),
            )
        };
        let ticked = || async {
            let mut v: Vec<i64> = sqlx::query_scalar(
                "SELECT item_id FROM component_progress WHERE user_id = 1 AND obtained ORDER BY 1",
            )
            .fetch_all(&pool)
            .await
            .unwrap();
            v.sort();
            v
        };
        let r = post(json!({"/A/Energize": 4, "/A/PaxSoar": 3, "/M/Serration": 10})).await;
        assert_eq!(r.0, StatusCode::OK);
        // Pax Soar maxes at 3; Energize is short; mods are never ticked.
        assert_eq!(ticked().await, vec![21]);
        post(json!({"/A/Energize": 5})).await;
        assert_eq!(ticked().await, vec![20, 21]);
        // Older clients send no ranks; nothing un-ticks.
        let (s, _) = call(
            &pool,
            "POST",
            "/api/sync/inventory",
            Some(&t),
            Some(json!({"items": {"/A/Grace": 1}})),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(ticked().await, vec![20, 21]);
    }

    #[test]
    fn component_names() {
        assert_eq!(component_name(CHASSIS), "saryn prime chassis");
        assert_eq!(
            component_name("/X/BratonPrimeBarrel"),
            "braton prime barrel"
        );
        assert_eq!(component_name(SARYN_BP), "saryn prime blueprint");
    }

    #[tokio::test]
    async fn maps_exact_and_component_paths() {
        let pool = test_pool().await;
        let t = seed(&pool).await;
        let v = sync(
            &pool,
            &t,
            json!({ NEURODE: 40, CHASSIS: 1, CHASSIS_BP: 1, SARYN_BP: 1, SARYN: 1, "/Lotus/Unknown/Thing": 3 }),
        )
        .await;
        assert_eq!(v["matched"], 5);
        assert_eq!(v["updated"], 5);
        assert_eq!(v["unmatched"], json!(["/Lotus/Unknown/Thing"]));
        let rows = inv(&pool).await;
        assert_eq!(rows.len(), 5, "{rows:?}");
        assert!(rows.iter().all(|(_, _, s)| s == "plugin"));
        assert!(rows.contains(&("Neurodes".into(), 40, "plugin".into())));
        // Same snapshot again: nothing changed.
        let v = sync(
            &pool,
            &t,
            json!({ NEURODE: 40, CHASSIS: 1, CHASSIS_BP: 1, SARYN_BP: 1, SARYN: 1 }),
        )
        .await;
        assert_eq!(v["updated"], 0);
    }

    #[tokio::test]
    async fn duplicate_paths_sum() {
        let pool = test_pool().await;
        let t = seed(&pool).await;
        let v = sync(
            &pool,
            &t,
            json!({ CHASSIS: 1, "/Lotus/Other/SarynPrimeChassis": 2 }),
        )
        .await;
        assert_eq!(v["matched"], 2);
        assert_eq!(
            inv(&pool).await,
            [("Saryn Prime Chassis".into(), 3, "plugin".into())]
        );
    }

    #[tokio::test]
    async fn snapshot_removes_absent_plugin_rows() {
        let pool = test_pool().await;
        let t = seed(&pool).await;
        sync(&pool, &t, json!({ NEURODE: 1, CHASSIS: 1 })).await;
        sync(&pool, &t, json!({ NEURODE: 2 })).await;
        assert_eq!(inv(&pool).await, [("Neurodes".into(), 2, "plugin".into())]);
    }

    #[tokio::test]
    async fn snapshot_keeps_manual_absent_rows() {
        let pool = test_pool().await;
        let t = seed(&pool).await;
        patch(&pool, 6, 500).await;
        sync(&pool, &t, json!({ NEURODE: 1 })).await;
        let rows = inv(&pool).await;
        assert!(
            rows.contains(&("Rubedo".into(), 500, "manual".into())),
            "{rows:?}"
        );
        assert!(rows.contains(&("Neurodes".into(), 1, "plugin".into())));
    }

    #[tokio::test]
    async fn manual_row_in_snapshot_becomes_plugin() {
        let pool = test_pool().await;
        let t = seed(&pool).await;
        patch(&pool, 1, 5).await;
        let v = sync(&pool, &t, json!({ NEURODE: 40 })).await;
        assert_eq!(v["updated"], 1);
        assert_eq!(inv(&pool).await, [("Neurodes".into(), 40, "plugin".into())]);
    }

    #[tokio::test]
    async fn requires_token_and_validates() {
        let pool = test_pool().await;
        let t = seed(&pool).await;
        let body = json!({ "items": { NEURODE: 1 } });
        let (s, v) = call(&pool, "POST", "/api/sync/inventory", None, Some(body)).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        assert_eq!(v["error"], "invalid token");
        let (s, v) = call(
            &pool,
            "POST",
            "/api/sync/inventory",
            Some(&t),
            Some(json!({ "items": { NEURODE: -1 } })),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["error"].is_string());
        let many: serde_json::Map<String, Value> =
            (0..20_001).map(|i| (format!("/p/{i}"), json!(1))).collect();
        let (s, _) = call(
            &pool,
            "POST",
            "/api/sync/inventory",
            Some(&t),
            Some(json!({ "items": many })),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(inv(&pool).await.is_empty());
    }

    #[tokio::test]
    async fn owned_frame_leaves_the_plan() {
        let pool = test_pool().await;
        let t = seed(&pool).await;
        sqlx::query("INSERT INTO wishlist (user_id, item_id) VALUES (1, 2)")
            .execute(&pool)
            .await
            .unwrap();
        assert!(plan_names(&pool).await.contains(&"Rubedo".to_string()));
        sync(&pool, &t, json!({ SARYN: 1 })).await;
        assert_eq!(plan_names(&pool).await, Vec::<String>::new());
    }

    /// Only the Chassis Blueprint owned: its relic need goes, the Chassis resources stay.
    #[tokio::test]
    async fn part_blueprint_keeps_part_resources() {
        let pool = test_pool().await;
        let t = seed(&pool).await;
        sqlx::query("INSERT INTO wishlist (user_id, item_id) VALUES (1, 2)")
            .execute(&pool)
            .await
            .unwrap();
        let before = plan_names(&pool).await;
        assert!(before.contains(&"Saryn Prime Chassis Blueprint".to_string()));
        sync(&pool, &t, json!({ CHASSIS_BP: 1 })).await;
        let after = plan_names(&pool).await;
        assert!(!after.contains(&"Saryn Prime Chassis Blueprint".to_string()));
        assert!(after.contains(&"Rubedo".to_string()), "{after:?}");
        assert!(after.contains(&"Saryn Prime Blueprint".to_string()));
    }
}
