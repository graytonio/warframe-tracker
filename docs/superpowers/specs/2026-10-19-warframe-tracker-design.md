# Warframe Tracker — Design

Date: 2026-10-19
Status: Draft, awaiting review

## Goal

A self-hosted web service where a player keeps a wishlist of Warframe items,
checks off the components they already have, and gets a farm plan: which
relics, missions, enemies and resources will get them the rest, grouped so
that a single mission covering several needs ranks first.

The official wiki (https://wiki.warframe.com/) is the only source of game data.

## Scope

**v1 item categories:** Warframes, weapons (all slots the wiki tracks,
including Primes), mods, arcanes. Resources and components are imported as
supporting data because recipes reference them.

**v1 progress tracking:** item level (on wishlist / done) plus per-component
checkboxes. Schema and API also support full inventory counts; the UI for
entering counts is minimal (a number field on resource rows), since the
expected long-term source of counts is a future game-inventory plugin.

**Single user, no auth**, but every user-owned row is keyed by `user_id` and
every handler resolves the user through one function (see Multi-user
readiness).

**Out of scope for v1:** companions, archwing, Railjack, cosmetics, auth,
the inventory-reading plugin, price/trade data, localisation.

## Architecture

One Rust binary, one container, one Postgres database.

```
warframe-tracker serve    # axum: /api/* + static React build, port 3000
warframe-tracker import   # fetch changed wiki modules, upsert game tables, exit
```

- **Backend:** Rust 2024, axum 0.8, sqlx 0.9 (Postgres, rustls), tokio,
  mlua (Lua 5.1, `vendored` feature) for the importer, reqwest for HTTP.
  Mirrors `self-hosted-zwilling-storage-tracker` conventions (mise tasks,
  compose Postgres for dev/test, two-stage Dockerfile).
- **Frontend:** React + TypeScript built with Vite into `web/dist`, copied
  into the image at `/app/web` and served by axum (`tower-http` `ServeDir`
  with SPA fallback to `index.html`). The path comes from `WEB_DIR`, default
  `web/dist`, so `cargo run` works in dev.
- **Migrations:** `sqlx::migrate!` run at startup of both subcommands.

```
wiki.warframe.com ──(import, scheduled)──▶ Postgres game tables
                                              │
browser ◀──▶ axum /api ◀──▶ Postgres user tables + game tables
```

## Data sourcing (importer)

### Source modules

All game data lives in a small fixed set of wiki Lua data modules:

| Module | Provides |
|---|---|
| `Module:Warframes/data` | Warframes (incl. Primes) |
| `Module:Weapons/data/{primary,secondary,melee,archwing,companion,railjack,modular,misc}` | Weapons; v1 imports primary/secondary/melee, others kept for later |
| `Module:Mods/data` | Mods |
| `Module:Arcane/data` | Arcanes |
| `Module:Blueprints/data` | Recipes: result, credits, build time, parts `{Count, Name, Type}` |
| `Module:Resources/data` | Resources (name, description, image) |
| `Module:Void/data` | `RelicData` (relic → part drops with rarity) and derived `PrimeData` (incl. vaulted) |
| `Module:DropTables/data` | `Missions` (rotation A/B/C rewards with chance), `Enemies` (mod/resource drops), `Containers` |
| `Module:Missions/data` | Planet → region resources, mission nodes / types |
| `Module:Table` | Helper required by `Void/data`; fetched only to satisfy `require` |

The module list is a constant in the importer. Adding a category later means
adding module titles and a mapping function.

### Change detection

1. One batched request for all module titles:
   `GET https://wiki.warframe.com/api.php?action=query&format=json&prop=revisions&rvprop=ids|timestamp&titles=<A>|<B>|...`
2. Compare each `revid` with `wiki_sources.rev_id`.
3. For each changed module, fetch
   `GET https://wiki.warframe.com/index.php?title=<title>&action=raw`.
4. If nothing changed, exit. Steady-state cost is one small request per run.

Requests are sequential with a descriptive `User-Agent`
(`warframe-tracker/<version> (+<repo url>)`). A failed fetch aborts that run
without touching the DB; the next run retries.

Because modules reference each other (recipes reference resources, drop
tables reference items), any change re-imports **all** game tables from the
latest fetched source. Raw source for every module is cached in
`wiki_sources.source` so a re-import never needs to refetch unchanged modules.

### Evaluation

- New `mlua::Lua` per module with a sandbox: only `string`, `table`, `math`,
  base functions minus `load*`/`dofile`; no `os`, `io`, `debug`.
- A custom `require(name)` resolves only to sources in `wiki_sources`.
- `mw` is not used by any data module today; if one starts to, the import
  fails loudly, never silently.
- The module's return value is converted to `serde_json::Value` via mlua's
  `serialize` feature, then mapped into typed Rust structs (serde) per module.
  Unknown fields are ignored; missing required fields fail the import.

### Mapping to tables

Each import runs in one transaction, so readers see either the old or the
new dataset:

- `items` are **upserted** on `(category, wiki_name)`.
- `recipes`, `recipe_meta`, `relic_drops` and `drop_sources` are **deleted
  and reinserted** wholesale. Nothing user-owned references them.

The wiki joins recipes, relics and drops by name. The importer resolves a
name to an item using the reference's `Type` hint (e.g. `Resource`,
`Mod`). If a name is still ambiguous or unknown, the reference is skipped
and listed in the import log summary; it does not fail the run, since wiki
data has occasional dangling names.

User-owned rows reference game items by `item_id`. Upserts keep `item_id`
stable across imports for items that still exist. Items that vanish from
the wiki are marked `removed_at` instead of deleted, so wishlist entries
survive.

## Schema

### Game data (written only by importer)

```
wiki_sources(title PK, rev_id, fetched_at, source text)

items(
  id bigserial PK,
  wiki_name text,                 -- "Saryn Prime", "Neurodes", "Saryn Prime Chassis"
                                  -- unique (category, wiki_name)
  category text,                  -- warframe | weapon | mod | arcane | resource | component | relic
  subcategory text null,          -- weapon slot, mod polarity group, etc.
  is_prime bool,
  is_vaulted bool null,           -- primes / prime parts, from Void/data
  image text null, wiki_link text null,
  data jsonb,                     -- full wiki record for display; not queried
  removed_at timestamptz null
)

recipes(
  result_item_id FK items,        -- what this blueprint builds
  ingredient_item_id FK items,
  count int,
  PK(result_item_id, ingredient_item_id)
)
recipe_meta(result_item_id PK, credits int, build_seconds int)

relic_drops(relic_item_id FK items, part_item_id FK items, rarity text)

drop_sources(
  id bigserial PK,
  source_kind text,               -- mission | enemy | container | planet
  source_name text,               -- "Nokko: Critter Liberation Rewards", "Earth"
  rotation text null,             -- A | B | C
  item_id FK items,
  chance numeric null,            -- percent, null for planet region resources
  quantity int null,
  is_pvp bool                     -- Conclave reward table
)
```

`category` is plain `text` with a `CHECK` constraint, so new categories are a
migration away. A component (e.g. "Saryn Prime Chassis") is an item with
category `component`. Components with their own blueprint have a `recipes`
row, which gives arbitrary-depth build trees.

### User data

```
users(id bigserial PK, name text, created_at)       -- seeded: id 1 "default"

wishlist(
  user_id FK users, item_id FK items,
  added_at, completed_at null,
  PK(user_id, item_id)
)

component_progress(
  user_id FK users, item_id FK items,               -- the component
  obtained bool,
  PK(user_id, item_id)
)

inventory(
  user_id FK users, item_id FK items,
  count bigint check (count >= 0),
  updated_at, source text,                          -- 'manual' | 'plugin'
  PK(user_id, item_id)
)
```

`component_progress` is the v1 checkbox; `inventory` holds counts. The
planner treats a component as covered if `obtained = true` **or**
`inventory.count >= needed`, so a plugin writing counts later lights up the
same UI with no schema change.

## API

All routes are under `/api`, JSON in/out, user resolved by the `CurrentUser`
extractor.

```
GET    /api/items?q=&category=&limit=     search catalog (ILIKE on wiki_name)
GET    /api/items/:id                     item + build tree + direct sources

GET    /api/wishlist                      wishlist with per-item progress %
POST   /api/wishlist            {item_id}
DELETE /api/wishlist/:item_id
PATCH  /api/wishlist/:item_id   {completed: bool}

PUT    /api/progress/:item_id   {obtained: bool}       component checkbox

GET    /api/inventory
PUT    /api/inventory           [{item_id, count}]     bulk upsert, for plugin
PATCH  /api/inventory/:item_id  {count}

GET    /api/plan                          farm plan (below)
GET    /api/meta                          last import time, source revisions
```

Errors: `{error: string}` with 400 (validation), 404 (unknown item), 500.
Unknown `item_id` on writes is a 404, not a silent insert.

## Farm planner

`GET /api/plan` for the current user:

1. **Expand.** For each active wishlist item, walk `recipes` recursively
   (SQL recursive CTE) to leaf ingredients, multiplying counts. Skip any
   subtree whose component is covered (checkbox or inventory). Subtract
   inventory counts from resource totals.
2. **Needs.** Produce the list of missing things:
   `{item, needed, have, for: [wishlist items]}`.
3. **Sources.** For each need:
   - Prime part → relics from `relic_drops` (with rarity, vaulted flag), and
     where those relics drop from `drop_sources`.
   - Anything else → `drop_sources` rows directly, plus planets whose region
     resource list contains it.
   - Item with no known source (e.g. market-only or event) → listed under
     "No farmable source on wiki" with its wiki link.
4. **Group.** Pivot sources to `{source, rotation, covers: [needs]}` and sort
   by number of distinct needs covered, then best drop chance. Vaulted relics
   are shown but sorted last and flagged.

Response has two views of the same data: `by_need` and `by_source`. The
frontend shows `by_source` as the default "what to run" list.

**PvP option.** `GET /api/plan?include_pvp=true` keeps Conclave reward
tables as sources; by default they're excluded, because their small
chances on many common resources would otherwise top the ranking. The
importer marks them with `drop_sources.is_pvp`: a mission drop table whose
alias starts with `Conclave` or `PVP`. The Plan page has a checkbox for this
option, remembered per browser.

v1 ranking is deliberately simple (coverage count, then chance). Expected
runs, mission duration and Steel Path modifiers are out of scope.

## Frontend

Three screens, React Router:

- **Wishlist:** search box (debounced `/api/items`) to add items. Each
  wishlist row expands into its build tree with a checkbox per component and
  a count per resource.
- **Plan:** `by_source` list ("Run Earth / Mapping Rotation C, covers
  Neurodes and Saryn Prime Neuroptics relic") with a toggle to `by_need`.
- **Inventory:** table of resources/components with editable counts.

Data fetching uses plain `fetch` plus a small hook; no state library.
Styling is plain CSS. Wiki images hot-link from the wiki's image URLs with
attribution in the footer. v1 doesn't cache images.

## Multi-user readiness

- Every user-owned table has `user_id` in its primary key.
- One `CurrentUser` axum extractor returns `UserId(1)` today. Adding auth
  means replacing that extractor's body (session/OIDC lookup) and adding a
  users UI. No handler or query changes.
- No global or in-memory per-user state in the server.
- The importer touches only game tables, never user tables.

## Deployment (homelab-flagops-templates)

- **Image:** `ghcr.io/graytonio/warframe-tracker`, built by GitHub Actions
  on `v*.*.*` tags (version must match `Cargo.toml`), multi-arch. The
  workflow is copied from zwilling-tracker. Dockerfile has three stages:
  node (build `web/`), rust (build binary), debian-slim (runtime).
- **App:** `apps/warframe-tracker/` using bjw-s `app-template` 5.2.1,
  modelled on `apps/zwilling-tracker/`:
  - controller `main`: `warframe-tracker serve`, port 3000, private ingress
    `warframe.[{ env "env_domain" }]`, letsencrypt, homepage annotations.
  - controller `import` (CronJob, e.g. every 6h): `warframe-tracker import`.
    The same image and DB env. Importing at startup is avoided so a slow wiki
    never blocks the web pod. The first run after deploy can be triggered
    manually with `kubectl create job --from=cronjob/...`.
  - `templates/pg.yaml`: Zalando `postgresql` CR, 1 instance, PG 17, 2Gi
    (raw module sources are ~5 MB, game tables small).
  - DB env: `PGUSER`/`PGPASSWORD` from the operator secret, then
    `DATABASE_URL=...?sslmode=require`.
- No secrets beyond DB credentials; the wiki is anonymous.

## Error handling

- **Importer:** any fetch, Lua or mapping error aborts before the
  transaction commits, logs the module title and error, and exits non-zero
  (CronJob shows failure). The previous dataset stays live.
- **Server:** if game tables are empty (import never ran), `/api/meta`
  reports it and the UI shows a "data not imported yet" banner.
- **DB unavailable at boot:** retry with backoff instead of crashing hard,
  since the Zalando DB may come up after the pod.

## Testing

- **Importer unit tests** run against checked-in fixture snippets of real
  module source (a few entries each, including Saryn Prime's blueprint and
  one relic) in `tests/fixtures/`, asserting parsed structs. No network in
  tests.
- **One import integration test** loads all fixtures into a compose Postgres
  and checks row counts and the Saryn Prime recipe tree.
- **Planner tests**:
  - Saryn Prime with Chassis checked off drops Chassis' resources from the plan.
  - Inventory counts subtract.
  - A mission dropping two needs ranks above one dropping one.
- **API:** a few axum `oneshot` tests for wishlist CRUD and 404 on an unknown
  item, in the zwilling-tracker style.
- **Frontend:** no test suite in v1; `tsc --noEmit` and a build in CI.
- `mise run lint` / `mise run test`, as in zwilling-tracker.

## Future work (explicitly deferred)

- Companions, archwing, Railjack, and other categories (add module + mapping).
- Inventory-reading plugin writing to `PUT /api/inventory` with
  `source = 'plugin'` (needs an API token once auth exists).
- Auth and multiple users (replace `CurrentUser`).
- Smarter ranking (expected runs, mission time).
