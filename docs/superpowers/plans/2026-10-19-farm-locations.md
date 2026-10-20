# Farm Locations Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The farm plan says *where to go*. Every need's sources roll up to planets and open worlds ("Where to go"). Bounty tables and node cache/bonus tables get real place names. Resources show the wiki's prose `Location:` line as a hint.

**Architecture:**
- **Importer:** maps all three node aliases, names node-less tables by their `Link` place, stores a structured `drop_sources.planet`, imports `enemy_locations` from 12 `Module:Enemies/data/<faction>` subpages, and parses `items.location_hint`.
- **Planner:** adds `by_location`, a pure roll-up over the sources it already fetches plus one `enemy_locations` query.
- **Plan page:** gets a default "Where to go" view.

**Tech Stack:** existing; no new dependencies.

**Spec:** `docs/superpowers/specs/2026-10-19-farm-locations-design.md` (approved; see its Decisions section)

## Global Constraints

- Everything in the v1 plan's Global Constraints and its "Learned during Phase 1" section still applies (`docs/superpowers/plans/2026-10-19-warframe-tracker.md`):
  - wrap SQL built with `format!` in `AssertSqlSafe`;
  - tests use `db::test_pool()`;
  - never run `docker compose down`;
  - errors are JSON.
- Places come only from structured wiki data. `location_hint` is display-only and never feeds `by_location`.
- Nodes listed for a place are at most 5. Nodes inferred from an enemy tileset are labelled "e.g." in the UI.
- `by_location` honours `include_pvp`, the same as `by_source`.
- `IMPORT_VERSION` must end up higher than every version on `main` and on `feat/inventory-sync` (inventory sync takes 3). Use 4. If a rebase collides, take the next free number.
- The new modules go in `wiki::MODULES`, which then has 24 titles, still under the 50-title revisions limit.

## Review Focus

1. **Enemies with an empty or odd planet value** (`""`, `"Any near Kuva Fortress"`). They must not crash anything or produce empty-name groups. The odd values show as non-`known` places with no nodes. Owner: Task 2 test `enemy_planets_normalised`.
2. **Same planet from several signals.** A region resource, a mission node and an enemy on "Earth" must merge into one group with three `via` strings, not three groups. Owner: Task 3 test `signals_merge_per_place`.
3. **Relic routes.** A prime-part need sourced from a relic dropped by a node mission must place at that mission's planet. A vaulted relic with no mission source has no place, which is fine. Owner: Task 3 test `relic_route_places_at_mission_planet`.
4. **Node-less table whose name already starts with its place.** `"Cambion Drift Bounty…"` must not become `"Cambion Drift: Cambion Drift Bounty…"`. Owner: Task 1 test `link_place_names`.
5. **Re-import keeps ids and user rows** after the schema additions. The v1 test `reimport_keeps_ids_and_marks_removed` must still pass; don't weaken it.

## Execution Shape

Tasks 1–4 run in order in one worktree, by one agent: Task 2 depends on Task 1's planet column, and Task 3 on both. Commit per task.

---

### Task 1: Node aliases, `Link` places, `drop_sources.planet`, skip `_IgnoreEntry`

**Files:** `schema.sql`, `src/import.rs`, `tests/fixtures/Module_Missions_data.json` / `Module_DropTables_data.json` (extend only), `tests/fixtures/README.md`

**Interfaces:**
- **Produces:** the column `drop_sources.planet text`, filled per the table in the spec's "`drop_sources.planet`" section, and these `source_name` formats:
  - `"{Planet} / {Node} ({Type})"` (unchanged)
  - `"{Planet} / {Node} ({Type}) — caches"`
  - `"{Planet} / {Node} ({Type}) — completion bonus"`
  - `"{place}: {Name}"`
  - `Name`
- **`fn link_place(link: Option<&str>) -> Option<&str>`:** the part of `Link` before `#`, or `None` if the result is generic (`Bounty`, `Storage Container`, `Mission`) or empty.
- **`fn normalise_place(p: &str) -> Option<String>`:** trims the name; an empty name gives `None`. `"Zariman Ten Zero"` becomes `"Zariman"`. Task 2 reuses this function.

- [ ] **Step 1: Schema.**
  `ALTER TABLE drop_sources ADD COLUMN IF NOT EXISTS planet text;`
  `CREATE INDEX IF NOT EXISTS drop_sources_planet ON drop_sources (planet);`
- [ ] **Step 2: Failing tests.** Extend the fixtures in a test-local copy, the way `conclave_tables_are_marked_pvp` does.

```rust
#[tokio::test]
async fn cache_and_extra_aliases_map_to_nodes() {
    let pool = test_pool().await;
    let mut m = fixtures();
    // One node using all three aliases; one table per alias with a Rubedo entry.
    m.get_mut("Module:Missions/data").unwrap()["MissionDetails"].as_array_mut().unwrap().push(json!({
        "Name": "Plato", "Planet": "Lua", "Type": "Sabotage",
        "DropTableAlias": "TestMain", "CacheDropTableAlias": "TestCache", "ExtraDropTableAlias": "TestExtra"
    }));
    let missions = m.get_mut("Module:DropTables/data").unwrap()["Missions"].as_object_mut().unwrap();
    for alias in ["TestMain", "TestCache", "TestExtra"] {
        missions.insert(alias.into(), json!({"Name": alias, "Rewards": {"A": [["Rubedo", "Resource", 10]]}}));
    }
    import_with(&pool, &m).await;
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT source_name, planet FROM drop_sources WHERE source_name LIKE 'Lua / Plato%' ORDER BY 1")
        .fetch_all(&pool).await.unwrap();
    assert_eq!(rows, vec![
        ("Lua / Plato (Sabotage)".into(), Some("Lua".into())),
        ("Lua / Plato (Sabotage) — caches".into(), Some("Lua".into())),
        ("Lua / Plato (Sabotage) — completion bonus".into(), Some("Lua".into())),
    ]);
}

#[tokio::test]
async fn link_place_names() {
    let pool = test_pool().await;
    let mut m = fixtures();
    let missions = m.get_mut("Module:DropTables/data").unwrap()["Missions"].as_object_mut().unwrap();
    missions.insert("CetusBounty1".into(), json!({"Name": "Cetus Bounty Lvl 5-15", "Link": "Plains of Eidolon#Bounties",
        "Rewards": {"A": [["Rubedo", "Resource", 10]]}}));
    missions.insert("DeimosBounty1".into(), json!({"Name": "Cambion Drift Bounty Lvl 5-15", "Link": "Cambion Drift#Bounties",
        "Rewards": {"A": [["Rubedo", "Resource", 10]]}}));
    missions.insert("Ignored".into(), json!({"Name": "Hallowed Flame", "_IgnoreEntry": true,
        "Rewards": {"A": [["Rubedo", "Resource", 10]]}}));
    import_with(&pool, &m).await;
    let names: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT source_name, planet FROM drop_sources WHERE source_name LIKE '%Bounty%' OR source_name LIKE '%Hallowed%' ORDER BY 1")
        .fetch_all(&pool).await.unwrap();
    assert_eq!(names, vec![
        ("Cambion Drift Bounty Lvl 5-15".into(), Some("Cambion Drift".into())),
        ("Plains of Eidolon: Cetus Bounty Lvl 5-15".into(), Some("Plains of Eidolon".into())),
    ]);
}

#[test]
fn place_helpers() {
    assert_eq!(super::link_place(Some("Cambion Drift#Bounties")), Some("Cambion Drift"));
    assert_eq!(super::link_place(Some("Storage Container")), None);
    assert_eq!(super::link_place(None), None);
    assert_eq!(super::normalise_place(" Zariman Ten Zero ").as_deref(), Some("Zariman"));
    assert_eq!(super::normalise_place(""), None);
}
```

  Also extend `drop_sources_and_planets_import` to assert that region-resource rows have `planet = 'Earth'` and that the Höllvania node row has `planet = 'Höllvania'`.

- [ ] **Step 3: Implement.**
  - **Node map:** becomes `HashMap<&str, Vec<(String /*source name*/, String /*planet*/)>>`. Each `MissionDetails` row pushes up to three entries:
    - `DropTableAlias` → `"{planet} / {name} ({ty})"`
    - `CacheDropTableAlias` → the same, plus `" — caches"`
    - `ExtraDropTableAlias` → the same, plus `" — completion bonus"`
  - **Mission loop:**
    - Skip `rec["_IgnoreEntry"] == true`.
    - Fallback when no node uses the table:

      ```rust
      let name = rec["Name"].as_str().unwrap_or(alias);
      let place = link_place(rec["Link"].as_str());
      let label = match place {
          Some(p) if !name.starts_with(p) => format!("{p}: {name}"),
          _ => name.to_string(),
      };
      vec![(label, place.and_then(normalise_place).unwrap_or_default())]
      ```

      An empty planet string becomes NULL at insert time (`NULLIF(p, '')`).
  - **Containers:** set `planet` to `link_place(rec["Link"]).and_then(normalise_place)`.
  - **Planet rows:** `planet` is the planet.
  - **Enemy rows:** `planet` is NULL.
  - Add `planet` to `DropRows` and to the `UNNEST` insert. `push` gains one argument; keep the existing `#[allow(clippy::too_many_arguments)]`.
- [ ] **Step 4.** `mise run test && mise run lint`. Commit: `feat(import): node cache/bonus tables, Link places, drop_sources.planet`.

---

### Task 2: `enemy_locations` and `items.location_hint`

**Files:** `schema.sql`, `src/wiki.rs` (MODULES), `src/import.rs`, `tests/fixtures/Module_Enemies_data_*.json` (new, trimmed), `tests/fixtures/README.md`

**Interfaces:**
- **Produces:**
  - Table `enemy_locations(enemy_name, planet, tileset)`.
  - Column `items.location_hint`.
  - `wiki::ENEMY_MODULES: &[&str]`, the 12 subpage titles, which `MODULES` includes.

- [ ] **Step 1: Schema.**

```sql
CREATE TABLE IF NOT EXISTS enemy_locations (
  enemy_name text NOT NULL,
  planet text NOT NULL,
  tileset text NOT NULL DEFAULT '',
  PRIMARY KEY (enemy_name, planet, tileset)
);
ALTER TABLE items ADD COLUMN IF NOT EXISTS location_hint text;
```

- [ ] **Step 2: Modules.** Add these to `MODULES`, keeping `"Module:Table"` last:
  `"Module:Enemies/data/anarchs"`, `corpus`, `grineer`, `infestation`, `narmer`, `orokin`, `scaldra`, `sentient`, `stalker`, `techrot`, `themurmur`, `unaffiliated`.

  Run `cargo run -- dump wiki-json` to refresh the dumps; all 23 must evaluate. If one fails, fix `lua.rs` with a regression test.

  Then create trimmed fixtures, keeping the module nesting, for `corpus` and `unaffiliated`:
  - `"Alad V"` (Planets plus TileSets);
  - `"002-ER"` (Planets only);
  - one Railjack enemy that drops Isos, from the dump;
  - one record with `Tilesets` (lowercase s), if any exists;
  - one record with `Planets = {"Zariman Ten Zero"}`, or a synthetic one added in a test.

  Every other enemy module fixture is `{}`. Record the `jq` commands in the fixtures README.

  `fixtures()` loads every MODULES title, so the new fixture files must exist for every title.
- [ ] **Step 3: Failing tests.**

```rust
#[tokio::test]
async fn enemy_planets_normalised() {
    let pool = test_pool().await;
    let mut m = fixtures();
    m.get_mut("Module:Enemies/data/unaffiliated").unwrap().as_object_mut().unwrap().insert(
        "Test Thrax".into(),
        json!({"General": {"Planets": ["Zariman Ten Zero", "", "Any near Kuva Fortress"], "Tilesets": ["Zariman"]}}),
    );
    import_with(&pool, &m).await;
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT planet, tileset FROM enemy_locations WHERE enemy_name = 'Test Thrax' ORDER BY 1")
        .fetch_all(&pool).await.unwrap();
    assert_eq!(rows, vec![("Any near Kuva Fortress".into(), "Zariman".into()), ("Zariman".into(), "Zariman".into())]);
    let alad: Vec<(String, String)> = sqlx::query_as(
        "SELECT planet, tileset FROM enemy_locations WHERE enemy_name = 'Alad V'").fetch_all(&pool).await.unwrap();
    assert_eq!(alad, vec![("Jupiter".into(), "Corpus Gas City".into())]);
    let er: Vec<(String, String)> = sqlx::query_as(
        "SELECT planet, tileset FROM enemy_locations WHERE enemy_name = '002-ER'").fetch_all(&pool).await.unwrap();
    assert_eq!(er, vec![("Neptune".into(), "".into())]);
}

#[tokio::test]
async fn location_hint_parsed_from_description() {
    let pool = test_pool().await;
    import(&pool).await;
    let hint: Option<String> = sqlx::query_scalar(
        "SELECT location_hint FROM items WHERE category='resource' AND wiki_name='Neurodes'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(hint.as_deref(), Some("Earth, Lua, Eris, and Deimos"));
    let none: Option<String> = sqlx::query_scalar(
        "SELECT location_hint FROM items WHERE category='warframe' AND wiki_name='Saryn Prime'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(none, None);
}
```

- [ ] **Step 4: Implement.**
  - For each `ENEMY_MODULES` title, take each `(name, rec)` of the object. `planets` is `rec["General"]["Planets"]`, and `tilesets` is `rec["General"]["TileSets"]` falling back to `["Tilesets"]`, falling back to `[""]`.
  - Insert the cross product of `planets.filter_map(normalise_place)` × the trimmed tilesets. De-duplicate in a `HashSet` before an `UNNEST` insert.
  - `DELETE FROM enemy_locations` at the start of `map_modules`, next to the other game-table deletes.
  - **`location_hint`:** for resources only, find a line starting with `Location:` in `Description` (split on `\n` and trim `\r`), and keep the remainder, trimmed. Pass it through `upsert_item`; add a parameter, or set it with a follow-up `UPDATE` keyed by id, whichever keeps `upsert_item` readable.
  - Bump `IMPORT_VERSION` to 4.
- [ ] **Step 5: Real data.** Run `mise run import`, then report:
  - the `enemy_locations` row count;
  - the distinct planets that match no `MissionDetails.Planet`;
  - unresolved items, which should be unchanged from before.

  Then commit: `feat(import): enemy locations and resource location hints`.

---

### Task 3: Planner `by_location`

**Files:** `src/plan.rs`, `src/api/catalog.rs` (only to add `planet` to `Source`)

**Interfaces:**
- **`Source` gains `planet: Option<String>`.** `item_sources` selects `d.planet` for direct rows. For relic-via-mission rows it selects `d.planet` of the mission that drops the relic, and for plain relic rows NULL. Update the two exact-JSON assertions in the catalog tests.
- **`Need` gains `location_hint: Option<String>`**, loaded with one query over the need ids.
- **`Plan` gains `by_location: Vec<LocationGroup>`.** The structs:

```rust
#[derive(Serialize, Debug)]
pub struct LocationGroup { pub place: String, pub known: bool, pub covers: Vec<LocationCover>, pub nodes: Vec<String> }
#[derive(Serialize, Debug)]
pub struct LocationCover { pub item_id: i64, pub name: String, pub via: Vec<String> }
```

- [ ] **Step 1: Failing tests.** Extend the `seed()` data in place, or add a `seed_places()` helper.

```rust
#[tokio::test]
async fn signals_merge_per_place() {
    // Rubedo: planet Earth (region resource); Neurodes: mission row with planet Earth,
    // plus an enemy "Earth Grunt" with enemy_locations ('Earth Grunt','Earth','Grineer Forest').
    // → one group "Earth", known=true, covers Rubedo ["region resource"] and Neurodes ["1 mission", "1 enemy"].
}
#[tokio::test]
async fn enemy_only_need_rolls_up_to_enemy_planets() {
    // Isos-like resource dropped only by enemy "Taro Crewship" with enemy_locations on
    // 'Venus Proxima' and 'Neptune Proxima' → two groups, each covering it via ["1 enemy"].
}
#[tokio::test]
async fn relic_route_places_at_mission_planet() {
    // Neuroptics via relic Axi S1 dropped by a mission row with planet 'Earth' → Earth covers Neuroptics.
}
#[tokio::test]
async fn pvp_and_unplaced_needs_excluded_from_by_location() {
    // A PvP mission row with planet 'Saturn' doesn't create a Saturn group by default; with include_pvp it does.
    // A need whose only source is an enemy with no enemy_locations is absent from by_location but
    // present in by_need with its location_hint.
}
#[tokio::test]
async fn location_sort_and_nodes() {
    // The group covering 2 needs sorts before groups covering 1.
    // nodes: mission sources' node names first (the part before " (" in "Earth / Mantle (Defense)"),
    // max 5, known=true only for planets present in MissionDetails nodes.
}
```

Write each test in full, seeding with SQL as the existing planner tests do.

- [ ] **Step 2: Implement.** The roll-up is pure Rust over the already-fetched `by_need` sources, plus these queries:

```sql
-- enemy places for the enemy sources in play
SELECT enemy_name, planet, tileset FROM enemy_locations WHERE enemy_name = ANY($1);
-- known places (planets that have mission nodes); node examples per place
SELECT DISTINCT split_part(source_name, ' / ', 1) AS planet,
       split_part(split_part(source_name, ' / ', 2), ' (', 1) AS node
FROM drop_sources WHERE source_kind = 'mission' AND source_name LIKE '% / %';
```

  The second query only reads data the importer already wrote, so `MissionDetails` doesn't need to be stored separately. Tileset-based node examples are deferred, and `nodes` comes only from mission sources. That's the cheaper, fully structured half of the spec's rule; mark it with `// ponytail: nodes come from mission sources only; tileset-inferred examples deferred` and mention it in the PR.

  `via` strings, in this order, with counts per need per place: `"region resource"`, `"N mission(s)"`, `"N enemy/enemies"`, `"N container(s)"`.

  Sort: distinct needs covered (descending), then total sources (descending), then place (ascending).
- [ ] **Step 3.** `mise run test && mise run lint`. Commit: `feat(plan): where-to-go roll-up by place`.

---

### Task 4: Plan page "Where to go"

**Files:** `web/src/api.ts`, `web/src/Plan.tsx`, `web/src/index.css`

- [ ] **Step 1: Types.** Add `planet: string | null` to `Source`, `location_hint: string | null` to `Need`, and `by_location: LocationGroup[]` (with the `LocationGroup` and `LocationCover` types) to `Plan`.
- [ ] **Step 2: Views.**
  - The view state becomes `'location' | 'source' | 'need'`, defaulting to `'location'` and remembered in localStorage (key `plan.view`, accessed in try/catch like `plan.includePvp`).
  - The toggle buttons read: "Where to go", "What to run", "What I need".
- [ ] **Step 3: "Where to go" list.**
  - Each group: `<h3>` with the place; a "known" badge is optional.
  - `covers` render as chips: `"{name} — {via.join(', ')}"`.
  - When `nodes` is non-empty, show a muted `"e.g. " + nodes.join(', ')`.
  - Empty state: `"No structured locations for your needs yet — see What I need."`
- [ ] **Step 4: "What I need".** When `location_hint` is set, render `<p className="muted">Wiki says: {location_hint}</p>`.
- [ ] **Step 5.** `cd web && npm run typecheck && npm run build`. Commit: `feat(web): where-to-go plan view`.

---

### Task 5: Verify and PR (the same agent)

- [ ] **Step 1: Real data via the API.**
  - Another agent may be using port 3000. Add one line to `src/main.rs` to read the listen port from `PORT` (default `3000`): `let port = std::env::var("PORT").unwrap_or_else(|_| "3000".into());`. Then run `PORT=3100 cargo run -- serve` in the background. Stop it when done.
  - POST Saryn Prime and **Isos** (a resource; the wishlist accepts any item) to `/api/wishlist`.
  - `/api/plan`:
    - `by_location` must include Proxima groups for Isos (e.g. "Venus Proxima"), and `by_need` must show Isos's `location_hint`.
    - Pick one bounty-only resource from the import summary and confirm that a "Plains of Eidolon", "Orb Vallis" or "Cambion Drift" group appears.
  - Remove the wishlist rows you added.
  - Re-measure the spec's coverage claim with SQL: resources that have `drop_sources` rows but get no place from `planet` or `enemy_locations`. Report the number alongside the spec's 6.
- [ ] **Step 2.** `mise run lint && mise run test`, plus the web build.
- [ ] **Step 3.** Push branch `feat/farm-locations` and open a PR with base `spec/farm-locations` (stacked on #4). The body covers:
  - what changed per task;
  - the real-data numbers;
  - the tileset deferral;
  - the `IMPORT_VERSION` note: 4 here, while inventory sync uses 3. Whichever merges second keeps the higher number.

  End the body with the Claude Code attribution line. Don't merge, tag or deploy.
