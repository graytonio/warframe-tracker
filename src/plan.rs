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
    /// The wiki's prose `Location:` line (resources only). Display only.
    pub location_hint: Option<String>,
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

/// A planet or open world, with the needs it covers.
#[derive(Serialize, Debug)]
pub struct LocationGroup {
    pub place: String,
    /// The place has mission nodes, so `nodes` can name some.
    pub known: bool,
    pub covers: Vec<LocationCover>,
    /// Up to 5 example nodes.
    pub nodes: Vec<String>,
    /// Every source at this place, one row per site and material.
    pub sites: Vec<Site>,
}

/// One source of one need at a place: "Mantle (Defense) drops Neurodes, rotation C, 25%".
#[derive(Serialize, Debug)]
pub struct Site {
    /// Node ("Mantle (Defense)"), activity, "Any mission on Earth", "X (enemy)" or container.
    pub site: String,
    /// `mission`, `planet`, `enemy` or `container`.
    pub kind: String,
    pub item_id: i64,
    pub name: String,
    pub rotation: Option<String>,
    pub chance: Option<f64>,
    pub quantity: Option<i32>,
    /// Relic name when the site drops a relic that drops the material.
    pub via: Option<String>,
}

#[derive(Serialize, Debug)]
pub struct LocationCover {
    pub item_id: i64,
    pub name: String,
    /// e.g. `["region resource", "3 missions", "12 enemies"]`
    pub via: Vec<String>,
}

#[derive(Serialize, Debug)]
pub struct Plan {
    pub by_need: Vec<Need>,
    pub by_source: Vec<SourceGroup>,
    pub by_location: Vec<LocationGroup>,
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
        if let Some(t) = load_tree(pool, Some(user_id), root).await? {
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
    let hints: Vec<(i64, Option<String>)> =
        sqlx::query_as("SELECT id, location_hint FROM items WHERE id = ANY($1)")
            .bind(&ids)
            .fetch_all(pool)
            .await?;
    for (id, h) in hints {
        needs.get_mut(&id).unwrap().location_hint = h;
    }
    let mut needs: Vec<Need> = needs.into_values().collect();
    needs.sort_by(|a, b| a.item.name.cmp(&b.item.name));
    let (by_need, unsourced): (Vec<_>, Vec<_>) =
        needs.into_iter().partition(|n| !n.sources.is_empty());

    let enemies: Vec<&str> = by_need
        .iter()
        .flat_map(|n| &n.sources)
        .filter(|s| s.kind == "enemy")
        .map(|s| s.name.as_str())
        .collect();
    let mut enemy_places: HashMap<String, Vec<String>> = HashMap::new();
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT DISTINCT enemy_name, planet FROM enemy_locations WHERE enemy_name = ANY($1) ORDER BY 1, 2",
    )
    .bind(&enemies)
    .fetch_all(pool)
    .await?;
    for (e, p) in rows {
        enemy_places.entry(e).or_default().push(p);
    }
    // Places with mission nodes.
    let known: HashSet<String> = sqlx::query_scalar(
        "SELECT DISTINCT split_part(source_name, ' / ', 1) FROM drop_sources
         WHERE source_kind = 'mission' AND source_name LIKE '% / %'",
    )
    .fetch_all(pool)
    .await?
    .into_iter()
    .collect();

    Ok(Plan {
        by_source: group(&by_need),
        by_location: by_location(&by_need, &enemy_places, &known),
        by_need,
        unsourced,
    })
}

/// Roll each need's sources up to the places they're found: `planet` when set, else every
/// planet of an enemy source. Needs with no place are left out.
fn by_location(
    needs: &[Need],
    enemy_places: &HashMap<String, Vec<String>>,
    known: &HashSet<String>,
) -> Vec<LocationGroup> {
    const KINDS: [&str; 4] = ["planet", "mission", "enemy", "container"];
    /// (need index, distinct source names per kind in KINDS order)
    type Tally<'a> = (usize, [HashSet<&'a str>; 4]);
    let mut places: HashMap<&str, Vec<Tally>> = HashMap::new();
    let mut nodes: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut sites: HashMap<&str, Vec<(usize, Site)>> = HashMap::new();
    for (i, n) in needs.iter().enumerate() {
        for s in &n.sources {
            let Some(k) = KINDS.iter().position(|k| *k == s.kind) else {
                continue;
            };
            let at: Vec<&str> = match (&s.planet, s.kind.as_str()) {
                (Some(p), _) => vec![p.as_str()],
                (None, "enemy") => enemy_places
                    .get(&s.name)
                    .into_iter()
                    .flatten()
                    .map(String::as_str)
                    .collect(),
                _ => vec![],
            };
            for p in at {
                let covers = places.entry(p).or_default();
                if covers.last().is_none_or(|(j, _)| *j != i) {
                    covers.push((i, Default::default()));
                }
                covers.last_mut().unwrap().1[k].insert(&s.name);
                let site = match s.kind.as_str() {
                    // "Earth / Mantle (Defense)" → "Mantle (Defense)"; "Cambion Drift: Bounty…" → "Bounty…"
                    "mission" => match s.name.split_once(" / ") {
                        Some((_, r)) => r.to_string(),
                        None => s
                            .name
                            .strip_prefix(&format!("{p}: "))
                            .unwrap_or(&s.name)
                            .to_string(),
                    },
                    "planet" => format!("Any mission on {p}"),
                    "enemy" => format!("{} (enemy)", s.name),
                    _ => s.name.clone(),
                };
                sites.entry(p).or_default().push((
                    k,
                    Site {
                        site,
                        kind: s.kind.clone(),
                        item_id: n.item.id,
                        name: n.item.name.clone(),
                        rotation: s.rotation.clone(),
                        chance: s.chance,
                        quantity: s.quantity,
                        via: s.via.clone(),
                    },
                ));
                // ponytail: nodes come from mission sources only; tileset-inferred examples deferred
                let node = match s.name.split_once(" / ") {
                    Some((_, rest)) if s.kind == "mission" => rest.split(" (").next(),
                    _ => None,
                };
                if let Some(node) = node {
                    let ns = nodes.entry(p).or_default();
                    if ns.len() < 5 && !ns.contains(&node) {
                        ns.push(node);
                    }
                }
            }
        }
    }
    let mut out: Vec<(usize, LocationGroup)> = places
        .into_iter()
        .map(|(place, covers)| {
            let total = covers
                .iter()
                .map(|(_, c)| c.iter().map(HashSet::len).sum::<usize>())
                .sum();
            let known = known.contains(place);
            let group = LocationGroup {
                place: place.to_string(),
                known,
                covers: covers
                    .into_iter()
                    .map(|(i, by_kind)| LocationCover {
                        item_id: needs[i].item.id,
                        name: needs[i].item.name.clone(),
                        via: via(by_kind.map(|s| s.len())),
                    })
                    .collect(),
                nodes: match known {
                    true => nodes.remove(place).unwrap_or_default(),
                    false => vec![],
                }
                .into_iter()
                .map(String::from)
                .collect(),
                sites: {
                    // Missions first (KINDS has planet first), then region resource, enemies, containers.
                    let rank = |k: usize| [1, 0, 2, 3][k];
                    let mut rows = sites.remove(place).unwrap_or_default();
                    rows.sort_by(|(ka, a), (kb, b)| {
                        rank(*ka)
                            .cmp(&rank(*kb))
                            .then(a.site.cmp(&b.site))
                            .then(
                                b.chance
                                    .partial_cmp(&a.chance)
                                    .unwrap_or(std::cmp::Ordering::Equal),
                            )
                            .then(a.name.cmp(&b.name))
                    });
                    rows.into_iter().map(|(_, r)| r).collect()
                },
            };
            (total, group)
        })
        .collect();
    out.sort_by(|(ta, a), (tb, b)| {
        b.covers
            .len()
            .cmp(&a.covers.len())
            .then(tb.cmp(ta))
            .then(a.place.cmp(&b.place))
    });
    out.into_iter().map(|(_, g)| g).collect()
}

/// `[planet, mission, enemy, container]` source counts → `["region resource", "3 missions", …]`.
fn via([planet, mission, enemy, container]: [usize; 4]) -> Vec<String> {
    let mut v = vec![];
    if planet > 0 {
        v.push("region resource".to_string());
    }
    for (n, one, many) in [
        (mission, "mission", "missions"),
        (enemy, "enemy", "enemies"),
        (container, "container", "containers"),
    ] {
        if n > 0 {
            v.push(format!("{n} {}", if n == 1 { one } else { many }));
        }
    }
    v
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
                    location_hint: None,
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

    /// Wishlist item 99 needs every listed resource once; `sql` adds sources.
    async fn seed_places(pool: &sqlx::PgPool, resources: &[(i64, &str)], sql: &str) {
        let items: Vec<String> = resources
            .iter()
            .map(|(id, n)| format!("({id},'{n}','resource')"))
            .collect();
        let recipes: Vec<String> = resources
            .iter()
            .map(|(id, _)| format!("(99,{id},1)"))
            .collect();
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "INSERT INTO items (id, wiki_name, category) VALUES (99,'Widget','weapon'),{};
             INSERT INTO recipes VALUES {};
             INSERT INTO wishlist (user_id,item_id) VALUES (1,99);
             {sql}",
            items.join(","),
            recipes.join(",")
        )))
        .execute(pool)
        .await
        .unwrap();
    }

    /// `(place, known, [(need, via)], nodes)` for compact assertions.
    type Loc = (String, bool, Vec<(String, Vec<String>)>, Vec<String>);
    fn locs(p: &super::Plan) -> Vec<Loc> {
        p.by_location
            .iter()
            .map(|g| {
                (
                    g.place.clone(),
                    g.known,
                    g.covers
                        .iter()
                        .map(|c| (c.name.clone(), c.via.clone()))
                        .collect(),
                    g.nodes.clone(),
                )
            })
            .collect()
    }
    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[tokio::test]
    async fn signals_merge_per_place() {
        let pool = test_pool().await;
        seed_places(
            &pool,
            &[(20, "Rubedo"), (21, "Neurodes")],
            "INSERT INTO drop_sources (source_kind,source_name,rotation,item_id,chance,planet) VALUES
               ('planet','Earth',null,20,null,'Earth'),
               ('mission','Earth / Mantle (Defense)','A',21,10,'Earth'),
               ('mission','Earth / Mantle (Defense)','B',21,10,'Earth'),
               ('enemy','Earth Grunt',null,21,5,null);
             INSERT INTO enemy_locations VALUES ('Earth Grunt','Earth','Grineer Forest');",
        )
        .await;
        let p = build_plan(&pool, 1, Default::default()).await.unwrap();
        assert_eq!(
            locs(&p),
            vec![(
                "Earth".into(),
                true,
                vec![
                    ("Neurodes".into(), s(&["1 mission", "1 enemy"])),
                    ("Rubedo".into(), s(&["region resource"]))
                ],
                s(&["Mantle"])
            )]
        );
    }

    #[tokio::test]
    async fn enemy_only_need_rolls_up_to_enemy_planets() {
        let pool = test_pool().await;
        seed_places(
            &pool,
            &[(30, "Isos")],
            "INSERT INTO drop_sources (source_kind,source_name,item_id,chance) VALUES
               ('enemy','Taro Crewship',30,5);
             INSERT INTO enemy_locations VALUES ('Taro Crewship','Venus Proxima','Free Space'),
               ('Taro Crewship','Venus Proxima','Other'),('Taro Crewship','Neptune Proxima','Free Space');",
        )
        .await;
        let p = build_plan(&pool, 1, Default::default()).await.unwrap();
        let isos = vec![("Isos".to_string(), s(&["1 enemy"]))];
        assert_eq!(
            locs(&p),
            vec![
                ("Neptune Proxima".into(), false, isos.clone(), vec![]),
                ("Venus Proxima".into(), false, isos, vec![])
            ]
        );
    }

    #[tokio::test]
    async fn relic_route_places_at_mission_planet() {
        let pool = test_pool().await;
        seed(&pool).await;
        sqlx::raw_sql(
            "UPDATE drop_sources SET planet = 'Earth' WHERE source_name = 'Earth/Mantle'",
        )
        .execute(&pool)
        .await
        .unwrap();
        let p = build_plan(&pool, 1, Default::default()).await.unwrap();
        let earth = p.by_location.iter().find(|g| g.place == "Earth").unwrap();
        assert_eq!(earth.covers.len(), 1);
        assert_eq!(earth.covers[0].name, "Saryn Prime Neuroptics");
        assert_eq!(earth.covers[0].via, s(&["1 mission"]));
    }

    /// `(site, kind, material, rotation, chance, via)` for compact assertions.
    type Row = (
        String,
        String,
        String,
        Option<String>,
        Option<f64>,
        Option<String>,
    );
    fn sites(g: &super::LocationGroup) -> Vec<Row> {
        g.sites
            .iter()
            .map(|x| {
                (
                    x.site.clone(),
                    x.kind.clone(),
                    x.name.clone(),
                    x.rotation.clone(),
                    x.chance,
                    x.via.clone(),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn sites_list_every_source_at_the_place() {
        let pool = test_pool().await;
        seed_places(
            &pool,
            &[(20, "Rubedo"), (21, "Neurodes")],
            "INSERT INTO drop_sources (source_kind,source_name,rotation,item_id,chance,planet) VALUES
               ('planet','Earth',null,20,null,'Earth'),
               ('mission','Earth / Mantle (Defense)','A',21,10,'Earth'),
               ('mission','Earth / Mantle (Defense)','C',21,25,'Earth'),
               ('mission','Earth / Cervantes (Spy) — caches','A',20,5,'Earth'),
               ('enemy','Earth Grunt',null,21,5,null);
             INSERT INTO enemy_locations VALUES ('Earth Grunt','Earth','Grineer Forest');",
        )
        .await;
        let p = build_plan(&pool, 1, Default::default()).await.unwrap();
        let o = |x: &str| Some(x.to_string());
        assert_eq!(
            sites(&p.by_location[0]),
            vec![
                // missions first, by site, then best chance
                (
                    o("Cervantes (Spy) — caches").unwrap(),
                    "mission".into(),
                    "Rubedo".into(),
                    o("A"),
                    Some(5.0),
                    None
                ),
                (
                    o("Mantle (Defense)").unwrap(),
                    "mission".into(),
                    "Neurodes".into(),
                    o("C"),
                    Some(25.0),
                    None
                ),
                (
                    o("Mantle (Defense)").unwrap(),
                    "mission".into(),
                    "Neurodes".into(),
                    o("A"),
                    Some(10.0),
                    None
                ),
                (
                    "Any mission on Earth".into(),
                    "planet".into(),
                    "Rubedo".into(),
                    None,
                    None,
                    None
                ),
                (
                    "Earth Grunt (enemy)".into(),
                    "enemy".into(),
                    "Neurodes".into(),
                    None,
                    Some(5.0),
                    None
                ),
            ]
        );
    }

    #[tokio::test]
    async fn sites_show_relic_routes() {
        let pool = test_pool().await;
        seed(&pool).await;
        sqlx::raw_sql(
            "UPDATE drop_sources SET planet = 'Earth' WHERE source_name = 'Earth/Mantle'",
        )
        .execute(&pool)
        .await
        .unwrap();
        let p = build_plan(&pool, 1, Default::default()).await.unwrap();
        let earth = p.by_location.iter().find(|g| g.place == "Earth").unwrap();
        assert_eq!(
            sites(earth),
            vec![(
                "Earth/Mantle".into(),
                "mission".into(),
                "Saryn Prime Neuroptics".into(),
                Some("C".into()),
                Some(20.0),
                Some("Axi S1".into())
            )]
        );
    }

    #[tokio::test]
    async fn pvp_and_unplaced_needs_excluded_from_by_location() {
        let pool = test_pool().await;
        seed_places(
            &pool,
            &[(40, "Ferrite"), (41, "Oxium")],
            "INSERT INTO drop_sources (source_kind,source_name,rotation,item_id,chance,is_pvp,planet) VALUES
               ('mission','Saturn / Lunaro Arena (Conclave)','B',40,12,true,'Saturn'),
               ('enemy','Oxium Osprey',null,41,5,false,null);
             UPDATE items SET location_hint = 'Jupiter, Pluto' WHERE id = 41;",
        )
        .await;
        let off = build_plan(&pool, 1, Default::default()).await.unwrap();
        assert!(off.by_location.is_empty(), "{:?}", locs(&off));
        let oxium = off.by_need.iter().find(|n| n.item.name == "Oxium").unwrap();
        assert_eq!(oxium.location_hint.as_deref(), Some("Jupiter, Pluto"));
        let on = build_plan(&pool, 1, super::PlanOptions { include_pvp: true })
            .await
            .unwrap();
        assert_eq!(
            locs(&on),
            vec![(
                "Saturn".into(),
                true,
                vec![("Ferrite".into(), s(&["1 mission"]))],
                s(&["Lunaro Arena"])
            )]
        );
    }

    #[tokio::test]
    async fn location_sort_and_nodes() {
        let pool = test_pool().await;
        // Alpha: six Mars nodes. Beta: one Mars node, two Venus enemies, one Perita enemy.
        // An unrelated Venus mission makes Venus known without giving it nodes.
        seed_places(
            &pool,
            &[(50, "Alpha"), (51, "Beta")],
            "INSERT INTO items (id, wiki_name, category) VALUES (53,'Other','resource');
             INSERT INTO drop_sources (source_kind,source_name,rotation,item_id,chance,planet) VALUES
               ('mission','Mars / N1 (Spy)','A',50,10,'Mars'),('mission','Mars / N2 (Spy)','A',50,10,'Mars'),
               ('mission','Mars / N3 (Spy)','A',50,10,'Mars'),('mission','Mars / N4 (Spy)','A',50,10,'Mars'),
               ('mission','Mars / N5 (Spy)','A',50,10,'Mars'),('mission','Mars / N6 (Spy)','A',50,10,'Mars'),
               ('mission','Mars / N1 (Spy)','A',51,10,'Mars'),
               ('enemy','G1',null,51,5,null),('enemy','G2',null,51,5,null),
               ('mission','Venus / Aphrodite (Exterminate)','A',53,10,'Venus');
             INSERT INTO enemy_locations VALUES ('G1','Venus',''),('G2','Venus',''),('G1','Perita','');",
        )
        .await;
        let p = build_plan(&pool, 1, Default::default()).await.unwrap();
        assert_eq!(
            locs(&p),
            vec![
                (
                    "Mars".into(),
                    true,
                    vec![
                        ("Alpha".into(), s(&["6 missions"])),
                        ("Beta".into(), s(&["1 mission"]))
                    ],
                    s(&["N1", "N2", "N3", "N4", "N5"])
                ),
                (
                    "Venus".into(),
                    true,
                    vec![("Beta".into(), s(&["2 enemies"]))],
                    vec![]
                ),
                (
                    "Perita".into(),
                    false,
                    vec![("Beta".into(), s(&["1 enemy"]))],
                    vec![]
                ),
            ]
        );
    }

    #[tokio::test]
    async fn plan_route_on_empty_db_is_200_and_empty() {
        use crate::api::test_support::{Auth, call, login};
        let pool = test_pool().await;
        let c = login(&pool, 1).await;
        let (s, v) = call(&pool, Auth::Session(&c), "GET", "/api/plan", None).await;
        assert_eq!(s, 200);
        assert_eq!(
            v,
            serde_json::json!({"by_need": [], "by_source": [], "by_location": [], "unsourced": []})
        );
    }
}
