use crate::AppState;
use crate::api::catalog::{Source, item_sources};
use crate::api::{ApiResult, CurrentUser};
use crate::models::ItemSummary;
use crate::tree::{ProgressNode, load_tree};
use axum::{Json, Router, extract::State, routing::get};
use serde::Serialize;
use sqlx::PgPool;
use std::collections::{HashMap, HashSet};

pub fn routes() -> Router<AppState> {
    Router::new().route("/plan", get(get_plan))
}

async fn get_plan(
    State(s): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> ApiResult<Json<Plan>> {
    Ok(Json(build_plan(&s.pool, user).await?))
}

#[derive(Serialize, Clone, Debug)]
pub struct ItemRef {
    pub id: i64,
    pub name: String,
}

#[derive(Serialize, Debug)]
pub struct Need {
    pub item: ItemSummary,
    pub needed: i64,
    pub have: i64,
    #[serde(rename = "for")]
    pub for_: Vec<ItemRef>,
    sources: Vec<Source>,
}

#[derive(Serialize, Debug)]
pub struct Cover {
    pub item_id: i64,
    pub name: String,
    pub chance: Option<f64>,
    pub via: Option<String>,
}

#[derive(Serialize, Debug)]
pub struct SourceGroup {
    pub kind: String,
    pub name: String,
    pub rotation: Option<String>,
    pub best_chance: Option<f64>,
    pub vaulted: bool,
    pub covers: Vec<Cover>,
}

#[derive(Serialize, Debug)]
pub struct Plan {
    pub by_need: Vec<Need>,
    pub by_source: Vec<SourceGroup>,
    pub unsourced: Vec<Need>,
}

pub async fn build_plan(pool: &PgPool, user_id: i64) -> sqlx::Result<Plan> {
    let roots: Vec<i64> = sqlx::query_scalar(
        "SELECT item_id FROM wishlist WHERE user_id = $1 AND completed_at IS NULL ORDER BY added_at",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    // ponytail: loads every sourced id; restrict to tree ids if drop tables get huge.
    let sourced: HashSet<i64> = sqlx::query_scalar(
        "SELECT part_item_id FROM relic_drops UNION SELECT item_id FROM drop_sources",
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect();

    let mut needs: HashMap<i64, Need> = HashMap::new();
    for root in roots {
        if let Some(t) = load_tree(pool, user_id, root).await? {
            let r = ItemRef {
                id: t.item.id,
                name: t.item.name.clone(),
            };
            collect(&t, &r, &sourced, &mut needs);
        }
    }
    needs.retain(|_, n| n.have < n.needed);

    let ids: Vec<i64> = needs.keys().copied().collect();
    for (id, s) in item_sources(pool, &ids).await? {
        needs.get_mut(&id).unwrap().sources.push(s);
    }
    let mut needs: Vec<Need> = needs.into_values().collect();
    needs.sort_by(|a, b| a.item.name.cmp(&b.item.name));
    let (by_need, unsourced): (Vec<_>, Vec<_>) =
        needs.into_iter().partition(|n| !n.sources.is_empty());

    Ok(Plan {
        by_source: group(&by_need),
        by_need,
        unsourced,
    })
}

/// Walk one wishlist tree, adding every missing thing to `needs` (rules in Task 5, step 2).
fn collect(
    node: &ProgressNode,
    root: &ItemRef,
    sourced: &HashSet<i64>,
    needs: &mut HashMap<i64, Need>,
) {
    let leaf = node.children.is_empty();
    if node.obtained || (!leaf && node.have >= node.needed) {
        return;
    }
    if leaf || sourced.contains(&node.item.id) {
        let n = needs.entry(node.item.id).or_insert_with(|| Need {
            item: node.item.clone(),
            needed: 0,
            have: node.have,
            for_: vec![],
            sources: vec![],
        });
        n.needed += node.needed;
        if n.for_.iter().all(|f| f.id != root.id) {
            n.for_.push(root.clone());
        }
    }
    for c in &node.children {
        collect(c, root, sourced, needs);
    }
}

fn group(needs: &[Need]) -> Vec<SourceGroup> {
    let mut groups: HashMap<(String, String, Option<String>), SourceGroup> = HashMap::new();
    for n in needs {
        for s in &n.sources {
            let g = groups
                .entry((s.kind.clone(), s.name.clone(), s.rotation.clone()))
                .or_insert_with(|| SourceGroup {
                    kind: s.kind.clone(),
                    name: s.name.clone(),
                    rotation: s.rotation.clone(),
                    best_chance: None,
                    vaulted: s.kind == "relic" && s.vaulted == Some(true),
                    covers: vec![],
                });
            if s.chance > g.best_chance {
                g.best_chance = s.chance;
            }
            if g.covers.iter().all(|c| c.item_id != n.item.id) {
                g.covers.push(Cover {
                    item_id: n.item.id,
                    name: n.item.name.clone(),
                    chance: s.chance,
                    via: s.via.clone(),
                });
            }
        }
    }
    let mut out: Vec<SourceGroup> = groups.into_values().collect();
    out.sort_by(|a, b| {
        a.vaulted
            .cmp(&b.vaulted)
            .then(b.covers.len().cmp(&a.covers.len()))
            // Option orders None first, so reversing puts nulls last.
            .then(
                b.best_chance
                    .partial_cmp(&a.best_chance)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(a.name.cmp(&b.name))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::build_plan;
    use crate::db::test_pool;

    // SP ← Chassis(1) ← Rubedo(1200); SP ← Neuroptics(1) ← Rubedo(750), Neurodes(2)
    // Neuroptics drops from relic Axi S1 (unvaulted), which drops from mission "Earth/Mantle" rot C.
    // Rubedo: planet Earth. Neurodes: planet Earth and mission "Lua/Tycho" rot A 10%.
    async fn seed(pool: &sqlx::PgPool) {
        sqlx::raw_sql(
            r#"
          INSERT INTO items (id, wiki_name, category, is_vaulted) VALUES
            (1,'Saryn Prime','warframe',null),(2,'Saryn Prime Chassis','component',null),
            (3,'Saryn Prime Neuroptics','component',false),(4,'Rubedo','resource',null),
            (5,'Neurodes','resource',null),(6,'Axi S1','relic',false),(7,'Loki','warframe',null),(8,'Mystery Part','component',null);
          INSERT INTO recipes VALUES (1,2,1),(1,3,1),(2,4,1200),(3,4,750),(3,5,2),(7,8,1);
          INSERT INTO relic_drops VALUES (6,3,'Common');
          INSERT INTO drop_sources (source_kind,source_name,rotation,item_id,chance) VALUES
            ('planet','Earth',null,4,null),('planet','Earth',null,5,null),
            ('mission','Lua/Tycho','A',5,10),('mission','Earth/Mantle','C',6,20);
          INSERT INTO wishlist (user_id,item_id) VALUES (1,1);
        "#,
        )
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn empty_wishlist_is_empty_plan() {
        let pool = test_pool().await;
        let p = build_plan(&pool, 1).await.unwrap();
        assert!(p.by_need.is_empty() && p.by_source.is_empty() && p.unsourced.is_empty());
    }

    #[tokio::test]
    async fn totals_resources_and_ranks_multi_cover_first() {
        let pool = test_pool().await;
        seed(&pool).await;
        let p = build_plan(&pool, 1).await.unwrap();
        let need = |n: &str| {
            p.by_need
                .iter()
                .find(|x| x.item.name == n)
                .unwrap_or_else(|| panic!("{n}"))
        };
        assert_eq!(need("Rubedo").needed, 1950);
        assert_eq!(need("Neurodes").needed, 2);
        assert!(
            need("Saryn Prime Neuroptics")
                .sources
                .iter()
                .any(|s| s.kind == "mission" && s.via.as_deref() == Some("Axi S1"))
        );
        assert_eq!(p.by_source[0].name, "Earth"); // covers Rubedo + Neurodes
        assert_eq!(p.by_source[0].covers.len(), 2);
    }

    #[tokio::test]
    async fn checked_component_removes_its_resources() {
        let pool = test_pool().await;
        seed(&pool).await;
        sqlx::raw_sql("INSERT INTO component_progress VALUES (1,2,true)")
            .execute(&pool)
            .await
            .unwrap();
        let p = build_plan(&pool, 1).await.unwrap();
        assert_eq!(
            p.by_need
                .iter()
                .find(|x| x.item.name == "Rubedo")
                .unwrap()
                .needed,
            750
        );
        assert!(
            p.by_need
                .iter()
                .all(|x| x.item.name != "Saryn Prime Chassis")
        );
    }

    #[tokio::test]
    async fn inventory_subtracts_and_satisfied_needs_drop() {
        let pool = test_pool().await;
        seed(&pool).await;
        sqlx::raw_sql("INSERT INTO inventory (user_id,item_id,count) VALUES (1,4,2000),(1,5,1)")
            .execute(&pool)
            .await
            .unwrap();
        let p = build_plan(&pool, 1).await.unwrap();
        assert!(p.by_need.iter().all(|x| x.item.name != "Rubedo"));
        let n = p
            .by_need
            .iter()
            .find(|x| x.item.name == "Neurodes")
            .unwrap();
        assert_eq!((n.needed, n.have), (2, 1));
    }

    #[tokio::test]
    async fn no_source_goes_to_unsourced() {
        let pool = test_pool().await;
        seed(&pool).await;
        sqlx::raw_sql("INSERT INTO wishlist (user_id,item_id) VALUES (1,7)")
            .execute(&pool)
            .await
            .unwrap();
        let p = build_plan(&pool, 1).await.unwrap();
        assert!(p.unsourced.iter().any(|x| x.item.name == "Mystery Part"));
    }

    #[tokio::test]
    async fn vaulted_relic_groups_sort_last() {
        let pool = test_pool().await;
        seed(&pool).await;
        sqlx::raw_sql("UPDATE items SET is_vaulted = true WHERE id = 6")
            .execute(&pool)
            .await
            .unwrap();
        let p = build_plan(&pool, 1).await.unwrap();
        let last = p.by_source.last().unwrap();
        assert!(last.vaulted && last.name == "Axi S1");
    }

    #[tokio::test]
    async fn plan_route_on_empty_db_is_200_and_empty() {
        use crate::{AppState, app};
        use axum::{body::Body, http::Request};
        use http_body_util::BodyExt;
        use tower::ServiceExt;
        let pool = test_pool().await;
        let req = Request::get("/api/plan").body(Body::empty()).unwrap();
        let res = app(AppState { pool }, "web/dist")
            .oneshot(req)
            .await
            .unwrap();
        assert_eq!(res.status(), 200);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(
            v,
            serde_json::json!({"by_need": [], "by_source": [], "unsourced": []})
        );
    }
}
