use crate::AppState;
use crate::api::catalog::{Source, item_sources};
use crate::api::{ApiResult, CurrentUser};
use crate::models::ItemSummary;
use crate::tree::{ProgressNode, load_tree};
use axum::{
    Json, Router,
    extract::{Query, State},
    routing::get,
};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::collections::{HashMap, HashSet};

pub fn routes() -> Router<AppState> {
    Router::new().route("/plan", get(get_plan))
}

#[derive(Deserialize, Default, Clone, Copy)]
pub struct PlanOptions {
    /// Count Conclave (PvP) reward tables as sources. Off by default.
    #[serde(default)]
    pub include_pvp: bool,
}

async fn get_plan(
    State(s): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(opts): Query<PlanOptions>,
) -> ApiResult<Json<Plan>> {
    Ok(Json(build_plan(&s.pool, user, opts).await?))
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
    pub pvp: bool,
    pub covers: Vec<Cover>,
}

#[derive(Serialize, Debug)]
pub struct Plan {
    pub by_need: Vec<Need>,
    pub by_source: Vec<SourceGroup>,
    pub unsourced: Vec<Need>,
}

pub async fn build_plan(pool: &PgPool, user_id: i64, opts: PlanOptions) -> sqlx::Result<Plan> {
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

    let mut trees = Vec::new();
    for root in roots {
        if let Some(t) = load_tree(pool, user_id, root).await? {
            trees.push(t);
        }
    }
    let mut needs = collect(&trees, &sourced);

    let ids: Vec<i64> = needs.keys().copied().collect();
    for (id, s) in item_sources(pool, &ids).await? {
        if s.pvp && !opts.include_pvp {
            continue;
        }
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

/// Everything still missing across all wishlist trees.
///
/// Demand is totalled per item over every tree before inventory is subtracted, and
/// only the shortfall is passed down to ingredients. Per-node checks would let one
/// Forma in inventory cover two wishlist items, or expand a recipe 4x when 3 of 4 are owned.
fn collect(trees: &[ProgressNode], sourced: &HashSet<i64>) -> HashMap<i64, Need> {
    // Merge the trees into one recipe graph.
    let mut info: HashMap<i64, &ProgressNode> = HashMap::new();
    let mut edges: HashMap<i64, Vec<(i64, i64)>> = HashMap::new();
    fn walk<'a>(
        n: &'a ProgressNode,
        info: &mut HashMap<i64, &'a ProgressNode>,
        edges: &mut HashMap<i64, Vec<(i64, i64)>>,
    ) {
        if info.insert(n.item.id, n).is_some() {
            return;
        }
        // `needed` is multiplied down the tree, so the recipe count is the ratio.
        let kids = n
            .children
            .iter()
            .map(|c| (c.item.id, c.needed / n.needed.max(1)))
            .collect();
        edges.insert(n.item.id, kids);
        for c in &n.children {
            walk(c, info, edges);
        }
    }
    for t in trees {
        walk(t, &mut info, &mut edges);
    }

    // Parents before children, so an item's demand is complete before it's expanded.
    fn visit(
        id: i64,
        edges: &HashMap<i64, Vec<(i64, i64)>>,
        seen: &mut HashSet<i64>,
        out: &mut Vec<i64>,
    ) {
        if !seen.insert(id) {
            return;
        }
        for (c, _) in &edges[&id] {
            visit(*c, edges, seen, out);
        }
        out.push(id);
    }
    let (mut order, mut seen) = (Vec::new(), HashSet::new());
    for t in trees {
        visit(t.item.id, &edges, &mut seen, &mut order);
    }
    order.reverse();

    let mut demand: HashMap<i64, i64> = HashMap::new();
    let mut for_: HashMap<i64, Vec<ItemRef>> = HashMap::new();
    for t in trees {
        *demand.entry(t.item.id).or_default() += 1;
        for_.entry(t.item.id).or_default().push(ItemRef {
            id: t.item.id,
            name: t.item.name.clone(),
        });
    }

    let mut needs = HashMap::new();
    for id in order {
        let d = demand.get(&id).copied().unwrap_or(0);
        let node = info[&id];
        if d == 0 || node.obtained {
            continue;
        }
        let roots = for_.get(&id).cloned().unwrap_or_default();
        let kids = &edges[&id];
        if (kids.is_empty() || sourced.contains(&id)) && node.have < d {
            needs.insert(
                id,
                Need {
                    item: node.item.clone(),
                    needed: d,
                    have: node.have,
                    for_: roots.clone(),
                    sources: vec![],
                },
            );
        }
        let short = (d - node.have).max(0);
        if short == 0 {
            continue;
        }
        for (c, n) in kids {
            *demand.entry(*c).or_default() += short.saturating_mul(*n);
            let f = for_.entry(*c).or_default();
            for r in &roots {
                if f.iter().all(|x| x.id != r.id) {
                    f.push(r.clone());
                }
            }
        }
    }
    needs
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
                    pvp: s.pvp,
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
        let p = build_plan(&pool, 1, Default::default()).await.unwrap();
        assert!(p.by_need.is_empty() && p.by_source.is_empty() && p.unsourced.is_empty());
    }

    #[tokio::test]
    async fn totals_resources_and_ranks_multi_cover_first() {
        let pool = test_pool().await;
        seed(&pool).await;
        let p = build_plan(&pool, 1, Default::default()).await.unwrap();
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
        let p = build_plan(&pool, 1, Default::default()).await.unwrap();
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
        let p = build_plan(&pool, 1, Default::default()).await.unwrap();
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
        let p = build_plan(&pool, 1, Default::default()).await.unwrap();
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
        let p = build_plan(&pool, 1, Default::default()).await.unwrap();
        let last = p.by_source.last().unwrap();
        assert!(last.vaulted && last.name == "Axi S1");
    }

    /// Forma (100) is a resource with its own recipe: 1 Neural Sensors (101).
    /// Two wishlist items (102, 103) each need Forma; Forma drops from a mission.
    async fn seed_forma(pool: &sqlx::PgPool, per_item: i32, owned: i64) {
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "INSERT INTO items (id, wiki_name, category) VALUES
               (100,'Forma','resource'),(101,'Neural Sensors','resource'),
               (102,'Item A','weapon'),(103,'Item B','weapon');
             INSERT INTO recipes VALUES (100,101,1),(102,100,{per_item}),(103,100,{per_item});
             INSERT INTO drop_sources (source_kind,source_name,item_id,chance) VALUES ('mission','Void',100,5);
             INSERT INTO wishlist (user_id,item_id) VALUES (1,102),(1,103);
             INSERT INTO inventory (user_id,item_id,count) VALUES (1,100,{owned})"
        )))
        .execute(pool)
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn inventory_is_shared_across_wishlist_items() {
        let pool = test_pool().await;
        seed_forma(&pool, 1, 1).await;
        let p = build_plan(&pool, 1, Default::default()).await.unwrap();
        let forma = p.by_need.iter().find(|n| n.item.name == "Forma").unwrap();
        assert_eq!((forma.needed, forma.have), (2, 1));
        assert_eq!(forma.for_.len(), 2);
        // Only the one missing Forma gets crafted.
        let ns = p
            .by_need
            .iter()
            .chain(&p.unsourced)
            .find(|n| n.item.name == "Neural Sensors")
            .unwrap();
        assert_eq!(ns.needed, 1);
    }

    #[tokio::test]
    async fn partial_inventory_expands_only_the_shortfall() {
        let pool = test_pool().await;
        seed_forma(&pool, 2, 3).await; // need 4, own 3
        let p = build_plan(&pool, 1, Default::default()).await.unwrap();
        let ns = p
            .by_need
            .iter()
            .chain(&p.unsourced)
            .find(|n| n.item.name == "Neural Sensors")
            .unwrap();
        assert_eq!(ns.needed, 1);
    }

    #[tokio::test]
    async fn pvp_sources_hidden_unless_included() {
        let pool = test_pool().await;
        seed(&pool).await;
        // A Conclave table covering both resources would otherwise tie Earth for first.
        sqlx::raw_sql(
            "INSERT INTO drop_sources (source_kind,source_name,rotation,item_id,chance,is_pvp) VALUES
               ('mission','Saturn / Lunaro Arena (Conclave)','B',4,12,true),
               ('mission','Saturn / Lunaro Arena (Conclave)','B',5,12,true)",
        )
        .execute(&pool)
        .await
        .unwrap();
        let conclave = |p: &super::Plan| p.by_source.iter().any(|g| g.name.contains("Conclave"));
        let off = build_plan(&pool, 1, Default::default()).await.unwrap();
        assert!(!conclave(&off));
        let all_sources = off.by_need.iter().flat_map(|n| &n.sources);
        assert!(all_sources.into_iter().all(|s| !s.pvp));
        let on = super::PlanOptions { include_pvp: true };
        assert!(conclave(&build_plan(&pool, 1, on).await.unwrap()));
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
