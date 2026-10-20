use crate::lua;
use crate::wiki::{MODULES, WikiClient};
use anyhow::Context;
use serde::Serialize;
use serde_json::{Map, Value, json};
use sqlx::{PgConnection, PgPool};
use std::collections::{HashMap, HashSet};

/// Bump whenever `map_modules` output changes, so the next run re-maps even if the wiki didn't change.
const IMPORT_VERSION: i64 = 2;

#[derive(Serialize, Default, Debug)]
pub struct ImportSummary {
    pub version: i64,
    pub items: usize,
    pub recipes: usize,
    pub relic_drops: usize,
    pub drop_sources: usize,
    pub unresolved: Vec<String>,
}

pub async fn run_import(pool: &PgPool) -> anyhow::Result<()> {
    run_import_with(pool, &WikiClient::new("https://wiki.warframe.com")).await?;
    Ok(())
}

/// Fetch changed modules and re-import everything. `None` when the wiki is unchanged
/// since the last successful run. Source writes and the import share one transaction.
pub async fn run_import_with(
    pool: &PgPool,
    client: &WikiClient,
) -> anyhow::Result<Option<ImportSummary>> {
    let revs = client.revisions(MODULES).await?;
    let mut tx = pool.begin().await?;
    let stored: HashMap<String, i64> = sqlx::query_as("SELECT title, rev_id FROM wiki_sources")
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .collect();
    let mut changed = 0;
    for &t in MODULES {
        let rev = *revs
            .get(t)
            .with_context(|| format!("{t} not found on the wiki"))?;
        if stored.get(t) == Some(&rev) {
            continue;
        }
        let source = client
            .raw(t)
            .await
            .with_context(|| format!("fetching {t}"))?;
        sqlx::query(
            "INSERT INTO wiki_sources (title, rev_id, fetched_at, source) VALUES ($1, $2, now(), $3)
             ON CONFLICT (title) DO UPDATE SET rev_id = EXCLUDED.rev_id, fetched_at = now(), source = EXCLUDED.source",
        )
        .bind(t)
        .bind(rev)
        .bind(source)
        .execute(&mut *tx)
        .await?;
        changed += 1;
    }
    let ran: bool = sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM import_runs WHERE (summary->>'version')::bigint = $1)",
    )
    .bind(IMPORT_VERSION)
    .fetch_one(&mut *tx)
    .await?;
    if changed == 0 && ran {
        println!("wiki unchanged");
        return Ok(None);
    }

    let sources: HashMap<String, String> = sqlx::query_as("SELECT title, source FROM wiki_sources")
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .collect();
    let mut modules = HashMap::new();
    for &t in MODULES.iter().filter(|t| **t != "Module:Table") {
        modules.insert(t.to_string(), lua::eval_module(t, &sources)?);
    }
    let summary = map_modules(&mut tx, &modules).await?;
    sqlx::query("INSERT INTO import_runs (summary) VALUES ($1)")
        .bind(serde_json::to_value(&summary)?)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    let unique: Vec<&String> = summary
        .unresolved
        .iter()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    println!(
        "imported {} modules changed: {} items, {} recipes, {} relic drops, {} drop sources, {} unresolved ({} distinct)",
        changed,
        summary.items,
        summary.recipes,
        summary.relic_drops,
        summary.drop_sources,
        summary.unresolved.len(),
        unique.len()
    );
    for u in unique.iter().take(20) {
        println!("  unresolved: {u}");
    }
    Ok(Some(summary))
}

type Ids = HashMap<(String, String), i64>;

/// Categories a blueprint result may have, in lookup order.
const RESULT_CATS: &[&str] = &[
    "warframe",
    "weapon",
    "resource",
    "mod",
    "arcane",
    "component",
];
/// Drop table entry types the importer tracks; others (cosmetics, sigils, credits…) are skipped.
const DROP_TYPES: &[&str] = &["Mod", "Arcane", "Resource", "Relic", "Blueprint", "Item"];
/// Enemy/container drop lists; each has a `<singular>Chance` multiplier.
const DROP_LISTS: &[&str] = &["Mods", "Resources", "Blueprints", "Items", "Relics"];

/// Map evaluated module JSON into the game tables, inside the caller's transaction.
pub async fn map_modules(
    tx: &mut PgConnection,
    modules: &HashMap<String, Value>,
) -> anyhow::Result<ImportSummary> {
    let module = |t: &str| {
        modules
            .get(t)
            .with_context(|| format!("module {t} missing"))
    };
    sqlx::raw_sql(
        "DELETE FROM recipes; DELETE FROM recipe_meta; DELETE FROM relic_drops; DELETE FROM drop_sources;",
    )
    .execute(&mut *tx)
    .await?;
    let mut s = ImportSummary {
        version: IMPORT_VERSION,
        ..Default::default()
    };
    let mut ids = Ids::new();

    // 1. Items from the per-category modules.
    let mut tables = vec![
        (
            "warframe",
            &module("Module:Warframes/data")?["Warframes"],
            None,
        ),
        ("mod", &module("Module:Mods/data")?["Mods"], None),
        ("arcane", &module("Module:Arcane/data")?["Arcanes"], None),
        (
            "resource",
            &module("Module:Resources/data")?["Resources"],
            None,
        ),
    ];
    for slot in ["primary", "secondary", "melee"] {
        tables.push((
            "weapon",
            module(&format!("Module:Weapons/data/{slot}"))?,
            Some(slot),
        ));
    }
    for (cat, table, slot) in tables {
        for (name, rec) in obj(table, cat)? {
            let sub = slot.or_else(|| {
                if cat == "mod" {
                    rec["Type"].as_str()
                } else {
                    None
                }
            });
            let link = rec["Link"].as_str().unwrap_or(name);
            let id = upsert_item(tx, cat, name, sub, None, link, rec).await?;
            ids.insert((cat.into(), name.clone()), id);
        }
    }
    let void = module("Module:Void/data")?;
    let relics = obj(&void["RelicData"], "RelicData")?;
    for (name, rec) in relics {
        let vaulted = Some(!rec["Vaulted"].is_null());
        let id = upsert_item(tx, "relic", name, rec["Tier"].as_str(), vaulted, name, rec).await?;
        ids.insert(("relic".into(), name.clone()), id);
    }

    // 3 (before recipes, so recipe parts can match relic components). Relic drops.
    let mut drops: HashMap<(i64, i64), Option<String>> = HashMap::new();
    for (relic, rec) in relics {
        for d in rec["Drops"].as_array().into_iter().flatten() {
            let (Some(item), Some(part)) = (d["Item"].as_str(), d["Part"].as_str()) else {
                continue;
            };
            let part_id = match part {
                // Requiem relics, Kuva, Riven Sliver…: the item itself, not a part.
                "" => resolve(
                    &ids,
                    item,
                    &["resource", "mod", "arcane", "weapon", "warframe"],
                )
                .map(|(_, id)| id),
                "Blueprint" => {
                    Some(component(tx, &mut ids, &format!("{item} Blueprint"), item).await?)
                }
                p => {
                    let p = p.strip_suffix(" Blueprint").unwrap_or(p);
                    Some(component(tx, &mut ids, &format!("{item} {p}"), item).await?)
                }
            };
            match part_id {
                Some(pid) => {
                    drops.insert(
                        (ids[&("relic".to_string(), relic.clone())], pid),
                        d["Rarity"].as_str().map(String::from),
                    );
                }
                None => s.unresolved.push(format!("{relic}: {item}")),
            }
        }
    }
    let (r, p, rarity): (Vec<i64>, Vec<i64>, Vec<Option<String>>) =
        drops.into_iter().map(|((r, p), y)| (r, p, y)).collect();
    s.relic_drops = r.len();
    sqlx::query(
        "INSERT INTO relic_drops (relic_item_id, part_item_id, rarity)
         SELECT * FROM UNNEST($1::bigint[], $2::bigint[], $3::text[])",
    )
    .bind(r)
    .bind(p)
    .bind(rarity)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE items i SET is_vaulted = v.vaulted
         FROM (SELECT rd.part_item_id, bool_and(r.is_vaulted) AS vaulted
               FROM relic_drops rd JOIN items r ON r.id = rd.relic_item_id GROUP BY 1) v
         WHERE i.id = v.part_item_id AND i.category = 'component'",
    )
    .execute(&mut *tx)
    .await?;
    let relic_parts: HashSet<String> = ids
        .keys()
        .filter(|(c, _)| c == "component")
        .map(|(_, n)| n.clone())
        .collect();

    // 2. Recipes. A blueprint is processed once its result is a known item, so
    // component blueprints run after the blueprint that introduced the component.
    let bps: HashMap<&str, &Value> = obj(module("Module:Blueprints/data")?, "Blueprints")?
        .values()
        .filter_map(Value::as_object)
        .flatten()
        .map(|(k, v)| (k.as_str(), v))
        .collect();
    let generic: HashSet<&str> = obj(
        &module("Module:Resources/data")?["GenericComponents"],
        "GenericComponents",
    )?
    .keys()
    .map(String::as_str)
    .collect();
    let mut recipes: HashMap<(i64, i64), i64> = HashMap::new();
    let mut meta: HashMap<i64, (Option<i64>, Option<i64>)> = HashMap::new();
    let mut done: HashSet<&str> = HashSet::new();
    loop {
        let ready: Vec<(&str, &Value)> = bps
            .iter()
            .map(|(k, bp)| (*k, *bp))
            .filter(|(k, bp)| {
                !done.contains(k)
                    && bp["Result"]
                        .as_str()
                        .is_some_and(|r| resolve(&ids, r, RESULT_CATS).is_some())
            })
            .collect();
        if ready.is_empty() {
            break;
        }
        for (key, bp) in ready {
            done.insert(key);
            let r = bp["Result"].as_str().unwrap_or(key);
            let (rcat, rid) = resolve(&ids, r, RESULT_CATS).unwrap();
            for part in bp["Parts"].as_array().into_iter().flatten() {
                let (Some(name), Some(count)) = (part["Name"].as_str(), part["Count"].as_i64())
                else {
                    continue;
                };
                let ty = part["Type"].as_str().unwrap_or("");
                let stripped = name.strip_prefix("Prime ").unwrap_or(name);
                let comp = format!("{r} {stripped}");
                let pid = if ty != "Resource"
                    && (bps.contains_key(comp.as_str())
                        || relic_parts.contains(&comp)
                        || generic.contains(stripped))
                {
                    Some(component(tx, &mut ids, &comp, r).await?)
                } else if let Some((_, id)) = resolve(&ids, name, part_cats(ty)) {
                    Some(id)
                } else if ty != "Resource" && bps.contains_key(name) {
                    // e.g. Equinox ← "Equinox Day Aspect", Chroma ← "Volt Neuroptics"
                    Some(component(tx, &mut ids, name, r).await?)
                } else {
                    None
                };
                match pid {
                    Some(pid) if pid != rid && count > 0 => {
                        *recipes.entry((rid, pid)).or_default() += count
                    }
                    Some(_) => {}
                    None => s.unresolved.push(format!("{r}: {name}")),
                }
            }
            if rcat != "component" {
                let bp_id = component(tx, &mut ids, &format!("{r} Blueprint"), r).await?;
                recipes.insert((rid, bp_id), 1);
            }
            meta.insert(rid, (bp["Credits"].as_i64(), bp["Time"].as_i64()));
        }
    }
    s.recipes = recipes.len();
    let (res, ing, cnt): (Vec<i64>, Vec<i64>, Vec<i32>) = recipes
        .into_iter()
        .map(|((r, i), c)| (r, i, c as i32))
        .collect();
    sqlx::query(
        "INSERT INTO recipes (result_item_id, ingredient_item_id, count)
         SELECT * FROM UNNEST($1::bigint[], $2::bigint[], $3::int[])",
    )
    .bind(res)
    .bind(ing)
    .bind(cnt)
    .execute(&mut *tx)
    .await?;
    let (mid, cr, tm): (Vec<i64>, Vec<Option<i32>>, Vec<Option<i32>>) = meta
        .into_iter()
        .map(|(id, (c, t))| (id, c.map(|c| c as i32), t.map(|t| t as i32)))
        .collect();
    sqlx::query(
        "INSERT INTO recipe_meta (result_item_id, credits, build_seconds)
         SELECT * FROM UNNEST($1::bigint[], $2::int[], $3::int[])",
    )
    .bind(mid)
    .bind(cr)
    .bind(tm)
    .execute(&mut *tx)
    .await?;

    // 4. Drop sources.
    let mut rows = DropRows::default();
    let missions_mod = module("Module:Missions/data")?;
    // alias → (source name, planet) for every node using it as main, cache or bonus table.
    let mut nodes: HashMap<&str, Vec<(String, String)>> = HashMap::new();
    for m in missions_mod["MissionDetails"]
        .as_array()
        .into_iter()
        .flatten()
    {
        let (Some(name), Some(planet), Some(ty)) =
            (m["Name"].as_str(), m["Planet"].as_str(), m["Type"].as_str())
        else {
            continue;
        };
        for (key, suffix) in [
            ("DropTableAlias", ""),
            ("CacheDropTableAlias", " — caches"),
            ("ExtraDropTableAlias", " — completion bonus"),
        ] {
            if let Some(alias) = m[key].as_str() {
                nodes.entry(alias).or_default().push((
                    format!("{planet} / {name} ({ty}){suffix}"),
                    planet.to_string(),
                ));
            }
        }
    }
    let dt = module("Module:DropTables/data")?;
    for (alias, rec) in obj(&dt["Missions"], "Missions")? {
        if rec["_IgnoreEntry"].as_bool() == Some(true) {
            continue;
        }
        let fallback = {
            let name = rec["Name"].as_str().unwrap_or(alias);
            let place = link_place(rec["Link"].as_str());
            let label = match place {
                Some(p) if !name.starts_with(p) => format!("{p}: {name}"),
                _ => name.to_string(),
            };
            vec![(label, place.and_then(normalise_place).unwrap_or_default())]
        };
        let names = nodes.get(alias.as_str()).unwrap_or(&fallback);
        // Conclave drop tables: `ConclaveLunaro`, `PVPWeeklyChallengeReward`, ...
        let pvp = alias.starts_with("Conclave") || alias.starts_with("PVP");
        for (rot, entries) in rec["Rewards"].as_object().into_iter().flatten() {
            for e in entries.as_array().into_iter().flatten() {
                let Some((name, ty, chance, qty)) = entry(e) else {
                    continue;
                };
                let Some(id) = drop_item(&ids, name, ty, &names[0].0, &mut s) else {
                    continue;
                };
                for (n, planet) in names {
                    rows.push("mission", n, Some(rot), id, chance, qty, pvp, planet);
                }
            }
        }
    }
    for (kind, key) in [("enemy", "Enemies"), ("container", "Containers")] {
        for (name, rec) in obj(&dt[key], key)? {
            if rec["_IgnoreEntry"].as_bool() == Some(true) {
                continue;
            }
            let planet = match kind {
                "container" => link_place(rec["Link"].as_str())
                    .and_then(normalise_place)
                    .unwrap_or_default(),
                _ => String::new(),
            };
            for list in DROP_LISTS {
                let mult = rec[format!("{}Chance", list.trim_end_matches('s'))]
                    .as_f64()
                    .unwrap_or(100.0)
                    / 100.0;
                for e in rec[*list].as_array().into_iter().flatten() {
                    let Some((item, ty, chance, qty)) = entry(e) else {
                        continue;
                    };
                    let Some(id) = drop_item(&ids, item, ty, name, &mut s) else {
                        continue;
                    };
                    rows.push(
                        kind,
                        name,
                        None,
                        id,
                        chance.map(|c| c * mult),
                        qty,
                        false,
                        &planet,
                    );
                }
            }
        }
    }

    // 5. Planet region resources.
    for (planet, names) in obj(&missions_mod["RegionResources"], "RegionResources")? {
        for n in names
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            match resolve(&ids, n, &["resource"]) {
                Some((_, id)) => rows.push("planet", planet, None, id, None, None, false, planet),
                None => s.unresolved.push(format!("{planet}: {n}")),
            }
        }
    }
    s.drop_sources = rows.kind.len();
    sqlx::query(
        "INSERT INTO drop_sources (source_kind, source_name, rotation, item_id, chance, quantity, is_pvp, planet)
         SELECT k, n, r, i, round(c::numeric, 4), q, p, NULLIF(pl, '')
         FROM UNNEST($1::text[], $2::text[], $3::text[], $4::bigint[], $5::float8[], $6::int[], $7::bool[], $8::text[])
           AS t(k, n, r, i, c, q, p, pl)",
    )
    .bind(rows.kind)
    .bind(rows.name)
    .bind(rows.rotation)
    .bind(rows.item)
    .bind(rows.chance)
    .bind(rows.quantity)
    .bind(rows.pvp)
    .bind(rows.planet)
    .execute(&mut *tx)
    .await?;

    // Anything the importer didn't touch this run has vanished from the wiki.
    let seen: Vec<i64> = ids.values().copied().collect();
    s.items = seen.len();
    sqlx::query("UPDATE items SET removed_at = now() WHERE removed_at IS NULL AND id <> ALL($1)")
        .bind(seen)
        .execute(&mut *tx)
        .await?;
    Ok(s)
}

#[derive(Default)]
struct DropRows {
    kind: Vec<String>,
    name: Vec<String>,
    rotation: Vec<Option<String>>,
    item: Vec<i64>,
    chance: Vec<Option<f64>>,
    quantity: Vec<Option<i32>>,
    pvp: Vec<bool>,
    /// "" for none; NULL at insert.
    planet: Vec<String>,
}

impl DropRows {
    // One argument per column; a struct here would just restate DropRows.
    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        kind: &str,
        name: &str,
        rot: Option<&str>,
        item: i64,
        chance: Option<f64>,
        qty: Option<i32>,
        pvp: bool,
        planet: &str,
    ) {
        self.kind.push(kind.into());
        self.name.push(name.into());
        self.rotation.push(rot.map(String::from));
        self.item.push(item);
        self.chance.push(chance);
        self.quantity.push(qty);
        self.pvp.push(pvp);
        self.planet.push(planet.into());
    }
}

/// The place part of a drop table's `Link` ("Cambion Drift#Bounties" → "Cambion Drift");
/// `None` for generic links.
fn link_place(link: Option<&str>) -> Option<&str> {
    let p = link?.split('#').next()?.trim();
    (!p.is_empty() && !["Bounty", "Storage Container", "Mission"].contains(&p)).then_some(p)
}

/// Trimmed place name with wiki aliases folded; `None` when empty.
fn normalise_place(p: &str) -> Option<String> {
    match p.trim() {
        "" => None,
        "Zariman Ten Zero" => Some("Zariman".into()),
        p => Some(p.into()),
    }
}

fn obj<'a>(v: &'a Value, what: &str) -> anyhow::Result<&'a Map<String, Value>> {
    v.as_object()
        .with_context(|| format!("{what} is not an object"))
}

/// `[name, type, chance, quantity?]`
fn entry(e: &Value) -> Option<(&str, &str, Option<f64>, Option<i32>)> {
    Some((
        e[0].as_str()?,
        e[1].as_str()?,
        e[2].as_f64(),
        e[3].as_i64().map(|q| q as i32),
    ))
}

fn resolve<'a>(ids: &Ids, name: &str, cats: &[&'a str]) -> Option<(&'a str, i64)> {
    cats.iter().find_map(|c| {
        ids.get(&(c.to_string(), name.to_string()))
            .map(|id| (*c, *id))
    })
}

fn part_cats(ty: &str) -> &'static [&'static str] {
    match ty {
        "Resource" => &["resource"],
        _ => &["weapon", "warframe", "component", "resource"],
    }
}

/// Resolve a drop-table entry by its type hint. `None` for skipped types and unresolved
/// names; the latter are recorded in the summary.
fn drop_item(ids: &Ids, name: &str, ty: &str, source: &str, s: &mut ImportSummary) -> Option<i64> {
    if !DROP_TYPES.contains(&ty) || name == "Region Resource" || name == "Credits Cache" {
        return None;
    }
    let found = match ty {
        "Mod" => resolve(ids, name, &["mod"]),
        "Arcane" => resolve(ids, name, &["arcane"]),
        "Resource" => resolve(ids, name, &["resource"]),
        "Relic" => {
            // "Axi A1 Relic", "Meso D8 (Radiant)"
            let n = name.split(" (").next().unwrap_or(name);
            resolve(ids, n.strip_suffix(" Relic").unwrap_or(n), &["relic"])
        }
        // "Aeolak Blueprint" is a component; "Ash Chassis Blueprint" drops the "Ash Chassis" component.
        _ => resolve(ids, name, &["component", "weapon", "warframe", "resource"])
            .or_else(|| resolve(ids, name.strip_suffix(" Blueprint")?, &["component"])),
    };
    if found.is_none() {
        s.unresolved.push(format!("{source}: {name} ({ty})"));
    }
    found.map(|(_, id)| id)
}

/// Get or create a component item. `link` is the wiki page of the item it builds.
async fn component(
    tx: &mut PgConnection,
    ids: &mut Ids,
    name: &str,
    link: &str,
) -> sqlx::Result<i64> {
    let key = ("component".to_string(), name.to_string());
    if let Some(id) = ids.get(&key) {
        return Ok(*id);
    }
    let id = upsert_item(tx, "component", name, None, None, link, &json!({})).await?;
    ids.insert(key, id);
    Ok(id)
}

async fn upsert_item(
    tx: &mut PgConnection,
    cat: &str,
    name: &str,
    sub: Option<&str>,
    vaulted: Option<bool>,
    link: &str,
    data: &Value,
) -> sqlx::Result<i64> {
    let is_prime = name.ends_with(" Prime") || name.contains(" Prime ");
    sqlx::query_scalar(
        "INSERT INTO items (wiki_name, category, subcategory, is_prime, is_vaulted, image, wiki_link, data, removed_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, NULL)
         ON CONFLICT (category, wiki_name) DO UPDATE SET subcategory = EXCLUDED.subcategory,
           is_prime = EXCLUDED.is_prime, is_vaulted = EXCLUDED.is_vaulted, image = EXCLUDED.image,
           wiki_link = EXCLUDED.wiki_link, data = EXCLUDED.data, removed_at = NULL
         RETURNING id",
    )
    .bind(name)
    .bind(cat)
    .bind(sub)
    .bind(is_prime)
    .bind(vaulted)
    .bind(data["Image"].as_str())
    .bind(link)
    .bind(data)
    .fetch_one(&mut *tx)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_pool;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering::SeqCst};

    fn fixtures() -> HashMap<String, Value> {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        MODULES
            .iter()
            .filter(|t| **t != "Module:Table")
            .map(|t| {
                let f = format!("{dir}/{}.json", t.replace([':', '/'], "_"));
                (
                    t.to_string(),
                    serde_json::from_slice(&std::fs::read(&f).unwrap()).unwrap(),
                )
            })
            .collect()
    }

    async fn import_with(pool: &PgPool, m: &HashMap<String, Value>) -> ImportSummary {
        let mut tx = pool.begin().await.unwrap();
        let s = map_modules(&mut tx, m).await.unwrap();
        tx.commit().await.unwrap();
        s
    }

    async fn import(pool: &PgPool) -> ImportSummary {
        import_with(pool, &fixtures()).await
    }

    async fn id(pool: &PgPool, cat: &str, name: &str) -> i64 {
        sqlx::query_scalar("SELECT id FROM items WHERE category=$1 AND wiki_name=$2")
            .bind(cat)
            .bind(name)
            .fetch_one(pool)
            .await
            .unwrap_or_else(|_| panic!("missing {cat}/{name}"))
    }

    #[tokio::test]
    async fn cache_and_extra_aliases_map_to_nodes() {
        let pool = test_pool().await;
        let mut m = fixtures();
        // One node using all three aliases; one table per alias with a Rubedo entry.
        m.get_mut("Module:Missions/data").unwrap()["MissionDetails"]
            .as_array_mut()
            .unwrap()
            .push(json!({
                "Name": "Plato", "Planet": "Lua", "Type": "Sabotage",
                "DropTableAlias": "TestMain", "CacheDropTableAlias": "TestCache", "ExtraDropTableAlias": "TestExtra"
            }));
        let missions = m.get_mut("Module:DropTables/data").unwrap()["Missions"]
            .as_object_mut()
            .unwrap();
        for alias in ["TestMain", "TestCache", "TestExtra"] {
            missions.insert(
                alias.into(),
                json!({"Name": alias, "Rewards": {"A": [["Rubedo", "Resource", 10]]}}),
            );
        }
        import_with(&pool, &m).await;
        let rows: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT source_name, planet FROM drop_sources WHERE source_name LIKE 'Lua / Plato%' ORDER BY 1",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            vec![
                ("Lua / Plato (Sabotage)".into(), Some("Lua".into())),
                ("Lua / Plato (Sabotage) — caches".into(), Some("Lua".into())),
                (
                    "Lua / Plato (Sabotage) — completion bonus".into(),
                    Some("Lua".into())
                ),
            ]
        );
    }

    #[tokio::test]
    async fn link_place_names() {
        let pool = test_pool().await;
        let mut m = fixtures();
        let missions = m.get_mut("Module:DropTables/data").unwrap()["Missions"]
            .as_object_mut()
            .unwrap();
        missions.insert(
            "CetusBounty1".into(),
            json!({"Name": "Cetus Bounty Lvl 5-15", "Link": "Plains of Eidolon#Bounties",
            "Rewards": {"A": [["Rubedo", "Resource", 10]]}}),
        );
        missions.insert(
            "DeimosBounty1".into(),
            json!({"Name": "Cambion Drift Bounty Lvl 5-15", "Link": "Cambion Drift#Bounties",
            "Rewards": {"A": [["Rubedo", "Resource", 10]]}}),
        );
        missions.insert(
            "Ignored".into(),
            json!({"Name": "Hallowed Flame", "_IgnoreEntry": true,
            "Rewards": {"A": [["Rubedo", "Resource", 10]]}}),
        );
        import_with(&pool, &m).await;
        let names: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT source_name, planet FROM drop_sources WHERE source_name LIKE '%Bounty%' OR source_name LIKE '%Hallowed%' ORDER BY 1",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            names,
            vec![
                (
                    "Cambion Drift Bounty Lvl 5-15".into(),
                    Some("Cambion Drift".into())
                ),
                (
                    "Plains of Eidolon: Cetus Bounty Lvl 5-15".into(),
                    Some("Plains of Eidolon".into())
                ),
            ]
        );
    }

    #[test]
    fn place_helpers() {
        assert_eq!(
            super::link_place(Some("Cambion Drift#Bounties")),
            Some("Cambion Drift")
        );
        assert_eq!(super::link_place(Some("Storage Container")), None);
        assert_eq!(super::link_place(None), None);
        assert_eq!(
            super::normalise_place(" Zariman Ten Zero ").as_deref(),
            Some("Zariman")
        );
        assert_eq!(super::normalise_place(""), None);
    }

    async fn ingredients(pool: &PgPool, result: i64) -> Vec<String> {
        sqlx::query_scalar(
            "SELECT i.wiki_name FROM recipes r JOIN items i ON i.id=r.ingredient_item_id
             WHERE r.result_item_id=$1 ORDER BY 1",
        )
        .bind(result)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn saryn_prime_recipe_resolves_components_and_blueprint() {
        let pool = test_pool().await;
        import(&pool).await;
        let sp = id(&pool, "warframe", "Saryn Prime").await;
        assert_eq!(
            ingredients(&pool, sp).await,
            [
                "Orokin Cell",
                "Saryn Prime Blueprint",
                "Saryn Prime Chassis",
                "Saryn Prime Neuroptics",
                "Saryn Prime Systems"
            ]
        );
        let chassis = id(&pool, "component", "Saryn Prime Chassis").await;
        let rubedo: i32 = sqlx::query_scalar(
            "SELECT count FROM recipes WHERE result_item_id=$1 AND ingredient_item_id=$2",
        )
        .bind(chassis)
        .bind(id(&pool, "resource", "Rubedo").await)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(rubedo, 1200);
        // A component's own blueprint is not a separate ingredient.
        assert!(
            !ingredients(&pool, chassis)
                .await
                .iter()
                .any(|n| n.ends_with("Blueprint"))
        );
    }

    #[tokio::test]
    async fn relic_parts_link_to_components() {
        let pool = test_pool().await;
        import(&pool).await;
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM relic_drops rd JOIN items i ON i.id=rd.part_item_id
             WHERE i.wiki_name='Saryn Prime Neuroptics'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(n >= 1);
        // Braton Prime's PrimePart "Barrel" is the same component the relics drop.
        let bp = id(&pool, "weapon", "Braton Prime").await;
        assert!(
            ingredients(&pool, bp)
                .await
                .contains(&"Braton Prime Barrel".to_string())
        );
        let (prime, vaulted): (bool, Option<bool>) = sqlx::query_as(
            "SELECT is_prime, is_vaulted FROM items WHERE wiki_name='Braton Prime Barrel'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(prime);
        assert!(vaulted.is_some());
    }

    #[tokio::test]
    async fn reimport_keeps_ids_and_marks_removed() {
        let pool = test_pool().await;
        import(&pool).await;
        let sp = id(&pool, "warframe", "Saryn Prime").await;
        sqlx::query("INSERT INTO wishlist (user_id,item_id) VALUES (1,$1)")
            .bind(sp)
            .execute(&pool)
            .await
            .unwrap();
        let mut m = fixtures();
        m.get_mut("Module:Warframes/data").unwrap()["Warframes"]
            .as_object_mut()
            .unwrap()
            .remove("Saryn");
        import_with(&pool, &m).await;
        assert_eq!(id(&pool, "warframe", "Saryn Prime").await, sp);
        let removed: bool = sqlx::query_scalar(
            "SELECT removed_at IS NOT NULL FROM items WHERE wiki_name='Saryn' AND category='warframe'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert!(removed);
        let wl: i64 = sqlx::query_scalar("SELECT count(*) FROM wishlist")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(wl, 1);
    }

    #[tokio::test]
    async fn conclave_tables_are_marked_pvp() {
        let pool = test_pool().await;
        let mut m = fixtures();
        m.get_mut("Module:DropTables/data").unwrap()["Missions"]
            .as_object_mut()
            .unwrap()
            .insert(
                "ConclaveLunaro".into(),
                json!({ "Name": "Lunaro (Conclave)", "Rewards": { "B": [["Rubedo", "Resource", 12.65, 300]] } }),
            );
        import_with(&pool, &m).await;
        let rows: Vec<(String, bool)> = sqlx::query_as(
            "SELECT DISTINCT source_name, is_pvp FROM drop_sources WHERE source_kind = 'mission' ORDER BY 1",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert!(rows.contains(&("Lunaro (Conclave)".into(), true)));
        assert!(
            rows.iter()
                .filter(|(n, _)| !n.contains("Conclave"))
                .all(|(_, p)| !p)
        );
    }

    #[tokio::test]
    async fn drop_sources_and_planets_import() {
        let pool = test_pool().await;
        let s = import(&pool).await;
        assert!(s.drop_sources > 0);
        let earth: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM drop_sources d JOIN items i ON i.id=d.item_id
             WHERE d.source_kind='planet' AND d.source_name='Earth' AND i.wiki_name='Neurodes'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(earth, 1);
        let unplaced: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM drop_sources WHERE source_kind='planet' AND planet IS DISTINCT FROM source_name",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(unplaced, 0);
        let earth_planet: Option<String> = sqlx::query_scalar(
            "SELECT DISTINCT planet FROM drop_sources WHERE source_kind='planet' AND source_name='Earth'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(earth_planet.as_deref(), Some("Earth"));
        let hv: Vec<Option<String>> = sqlx::query_scalar(
            "SELECT DISTINCT planet FROM drop_sources WHERE source_name='Höllvania / Victory Plaza (Assassination)'",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(hv, vec![Some("Höllvania".to_string())]);
        // Missions are named after the node that uses the drop table.
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM drop_sources d JOIN items i ON i.id=d.item_id
             WHERE d.source_name='Höllvania / Victory Plaza (Assassination)' AND d.rotation='A'
               AND i.wiki_name='Meso D8'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(n, 1);
    }

    /// Non-prime weapon parts are generic components ("Barrel" → "Braton Barrel"); enemy
    /// blueprint drops resolve to them with scaled chance; refined relic names resolve.
    #[tokio::test]
    async fn generic_parts_blueprint_drops_and_refined_relics() {
        let pool = test_pool().await;
        let mut m = fixtures();
        m.get_mut("Module:Blueprints/data").unwrap()["Blueprints"]["Braton"] = json!({
            "Result": "Braton", "Credits": 1000, "Time": 60,
            "Parts": [{"Count": 1, "Name": "Barrel", "Type": "Item"},
                      {"Count": 5, "Name": "Rubedo", "Type": "Resource"}]
        });
        let dt = m.get_mut("Module:DropTables/data").unwrap();
        let ac = &mut dt["Enemies"]["Aerial Commander"];
        ac["BlueprintChance"] = json!(10);
        ac["Blueprints"] = json!([
            ["Braton Barrel Blueprint", "Blueprint", 50],
            ["Braton Blueprint", "Blueprint", 50]
        ]);
        dt["Missions"]["1999Assassination"]["Rewards"]["A"] = json!([
            ["Meso D8 (Radiant)", "Relic", 5],
            ["Some Glyph", "Cosmetic", 5]
        ]);
        let s = import_with(&pool, &m).await;
        assert!(
            !s.unresolved
                .iter()
                .any(|u| u.contains("Braton") || u.contains("Radiant") || u.contains("Glyph")),
            "{:?}",
            s.unresolved
        );
        let braton = id(&pool, "weapon", "Braton").await;
        assert_eq!(
            ingredients(&pool, braton).await,
            ["Braton Barrel", "Braton Blueprint", "Rubedo"]
        );
        let rows: Vec<(String, f64)> = sqlx::query_as(
            "SELECT i.wiki_name, d.chance::float8 FROM drop_sources d JOIN items i ON i.id=d.item_id
             WHERE d.source_kind='enemy' AND i.category='component' ORDER BY 1",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(
            rows,
            [
                ("Braton Barrel".into(), 5.0),
                ("Braton Blueprint".into(), 5.0)
            ]
        );
        let meso: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM drop_sources d JOIN items i ON i.id=d.item_id
             WHERE i.wiki_name='Meso D8' AND d.source_name LIKE '%Victory Plaza%'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(meso, 1);
    }

    fn json_to_lua(v: &Value) -> String {
        match v {
            Value::Null => "nil".into(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => n.to_string(),
            Value::String(s) => format!("{s:?}"),
            Value::Array(a) => format!(
                "{{{}}}",
                a.iter().map(json_to_lua).collect::<Vec<_>>().join(",")
            ),
            Value::Object(o) => format!(
                "{{{}}}",
                o.iter()
                    .map(|(k, v)| format!("[{k:?}]={}", json_to_lua(v)))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
        }
    }

    /// Wiki stub: every module is at revision 1 except Warframes/data, which is at `rev`.
    /// `/index.php` serves fixtures as Lua and counts hits.
    async fn stub(rev: Arc<AtomicI64>, hits: Arc<AtomicUsize>) -> WikiClient {
        use axum::{Router, extract::Query, routing::get};
        let sources: HashMap<String, String> = fixtures()
            .iter()
            .map(|(t, v)| (t.clone(), format!("return {}", json_to_lua(v))))
            .collect();
        let app = Router::new()
            .route(
                "/api.php",
                get(move || async move {
                    let pages: Map<String, Value> = MODULES
                        .iter()
                        .enumerate()
                        .map(|(i, t)| {
                            let r = if *t == "Module:Warframes/data" {
                                rev.load(SeqCst)
                            } else {
                                1
                            };
                            (
                                i.to_string(),
                                json!({"title": t, "revisions": [{"revid": r}]}),
                            )
                        })
                        .collect();
                    axum::Json(json!({"query": {"pages": pages}}))
                }),
            )
            .route(
                "/index.php",
                get(move |Query(q): Query<HashMap<String, String>>| async move {
                    hits.fetch_add(1, SeqCst);
                    sources
                        .get(&q["title"])
                        .cloned()
                        .unwrap_or_else(|| "return {}".into())
                }),
            );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        WikiClient::new(&format!("http://{addr}"))
    }

    #[tokio::test]
    async fn run_import_is_revision_gated() {
        let pool = test_pool().await;
        let (rev, hits) = (Arc::new(AtomicI64::new(1)), Arc::new(AtomicUsize::new(0)));
        let c = stub(rev.clone(), hits.clone()).await;

        let s = run_import_with(&pool, &c)
            .await
            .unwrap()
            .expect("first run imports");
        assert!(s.items > 0);
        assert_eq!(hits.swap(0, SeqCst), MODULES.len());

        assert!(run_import_with(&pool, &c).await.unwrap().is_none());
        assert_eq!(hits.swap(0, SeqCst), 0);

        rev.store(2, SeqCst);
        assert!(run_import_with(&pool, &c).await.unwrap().is_some());
        assert_eq!(hits.load(SeqCst), 1);
        let runs: i64 = sqlx::query_scalar("SELECT count(*) FROM import_runs")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(runs, 2);
    }
}
