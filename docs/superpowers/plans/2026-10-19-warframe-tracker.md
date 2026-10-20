# Warframe Tracker v1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship a single-container Rust + React service that imports Warframe data from the official wiki's Lua data modules, tracks a wishlist with per-component progress and inventory counts, and produces a grouped farm plan.

**Architecture:** One Rust binary with three subcommands: `serve` (axum API + static React build), `import` (revision-gated wiki fetch → mlua eval → Postgres upsert), and `dump` (a dev tool that writes evaluated modules as JSON). Game tables are written only by the importer; user tables are keyed by `user_id` and resolved through one `CurrentUser` extractor. Build-tree expansion with progress lives in one shared module (`tree.rs`), used by both the wishlist API and the planner.

**Tech Stack:** Rust 1.94 (edition 2024), axum 0.8, sqlx 0.9 (runtime queries, no macros, matching zwilling-tracker), tokio, mlua (lua51 + vendored + serialize), reqwest (rustls), tower-http (fs), serde/serde_json, anyhow. Frontend: React + TypeScript + Vite, react-router. Postgres 17. mise tasks, docker compose for dev DB.

**Spec:** `docs/superpowers/specs/2026-10-19-warframe-tracker-design.md`

## Global Constraints

- Wiki is the only data source: `https://wiki.warframe.com/api.php` (revisions) and `https://wiki.warframe.com/index.php?title=<T>&action=raw` (source).
- Requests are sequential and send the header `User-Agent: warframe-tracker/<CARGO_PKG_VERSION> (+https://github.com/graytonio/warframe-tracker)`.
- v1 categories: `warframe | weapon | mod | arcane`, plus supporting `resource | component | relic`. `items.category` is `text` with a `CHECK` constraint.
- Every user-owned table has `user_id` in its PK; handlers get the user only via the `CurrentUser` extractor (returns `UserId(1)`).
- The importer never writes user tables. Items that vanish from the wiki get `removed_at`, never `DELETE`.
- A component is covered if `component_progress.obtained` **or** `inventory.count >= needed`.
- Server listens on port 3000; static dir comes from `WEB_DIR` (default `web/dist`).
- Errors are JSON `{"error": "..."}`: 400 validation, 404 unknown item, 500 otherwise.
- Postgres connections use `DATABASE_URL`; production requires `sslmode=require` (the Spilo image rejects `disable`).
- Dev DB: compose project `warframe-tracker`, host port **5433**, `postgres://postgres:postgres@localhost:5433/warframe`.
- Tests need Postgres at `DATABASE_URL`. Each test gets a fresh database via `db::test_pool()`, so parallel worktrees and parallel tests never collide. Do **not** run `docker compose down`, and don't start your own Postgres.

### Learned during Phase 1 (authoritative over any code snippet below)

- **sqlx 0.9:** a SQL string that isn't `&'static str` (anything built with `format!`) must be wrapped: `sqlx::query(sqlx::AssertSqlSafe(sql))`, `sqlx::raw_sql(sqlx::AssertSqlSafe(..))`. Only interpolate constants, never user input; bind user input with `$n`.
- **mlua 0.12:** `use mlua::serde::DeserializeOptions`; the `error-send` feature is on so `?` into `anyhow` works.
- **reqwest 0.13:** features are `rustls`, `json`, `query`.
- **Full JSON dumps of every module** are at `/Users/graytonw/repos/warframe-tracker/wiki-json/` (gitignored, read-only for workers; regenerate with `cargo run -- dump wiki-json`).
- **Real shapes:**
  - `Blueprints/data` has top-level groups `Blueprints` and `Suits`, both objects keyed by result name. Part `Type` values include `Resource`, `Item`, `PrimePart`, `Weapon`. Braton Prime's parts are `Barrel` / `Receiver` / `Stock` with `Type: "PrimePart"`.
  - `DropTables/data.Missions` is keyed by a drop-table **alias** (e.g. `"1999Defense"`) with a generic `Name` (`"Assassination"`). Every `Rewards` value is an object keyed by rotation (`A`/`B`/`C`). Entries are `[name, type, chance, quantity?]`, and relic entries are plain names (`"Meso Y2"`, type `"Relic"`).
  - `Missions/data.MissionDetails[]` rows have `Name` (node), `Planet`, `Type` and `DropTableAlias`. Use them to name a mission source `"{Planet} / {Name} ({Type})"`, with one `drop_sources` row per node sharing the alias. Fall back to the drop table's `Name` when no node uses the alias (bounties, events). `Missions/data` also has `RegionResources`.
  - `Void/data.RelicData[relic]` has `Drops[{Item, Part, Rarity}]`, `Tier`, `Vaulted` (a version string when vaulted, absent otherwise) and `Name`.
  - `Warframes/data` also has `Archwings`, `Necramechs` and `Operators` groups. v1 imports only `Warframes`.

## Review Focus

1. **Wiki name mismatches.** Recipe parts like `"Prime Neuroptics"` must resolve to the `"Saryn Prime Neuroptics"` component, not to an orphan item. Task 3 pins this with a Saryn Prime fixture test.
2. **A covered parent hides its subtree.** Checking off a component must drop its resources from the plan, even when the same resource is still needed elsewhere (it must be subtracted, not removed). Task 2 tests this in `tree.rs`; Task 5 tests it again in the planner.
3. **Re-import keeps user data.** Running import twice, or with an item removed upstream, must not change `item_id`s or delete wishlist rows. Task 3 tests this.
4. **Unknown or removed item on write.** `POST /api/wishlist {item_id: 999999}` must return 404, not a foreign-key 500. Task 4 tests this.
5. **Empty DB before the first import.** `/api/meta` and `/api/plan` must return 200 with empty data, not 500, so the UI can show its banner. Tasks 4 and 5 test this.

## Execution Shape

- **Phase 1 (sequential, lead session, branch `feat/v1`):** Tasks 1–2. These build the foundation every other task compiles against, plus real JSON dumps of all modules at `$WIKI_JSON` (an absolute path the lead gives to each worker).
- **Phase 2 (parallel worktrees branched from `feat/v1` after Task 2):** Tasks 3, 4, 5, 6, 7. Each task owns disjoint files; `src/lib.rs` and `src/api/mod.rs` already register every route module, so workers only fill in their own files.
- **Phase 3 (sequential, lead):** Task 8 merges the branches, runs a real import, does a browser smoke test, then a whole-branch review.

## File Map

```
Cargo.toml, mise.toml, compose.yaml, .gitignore        T1
schema.sql                                             T1  (full schema, idempotent)
src/main.rs        CLI dispatch serve|import|dump      T1 (import wiring T3)
src/lib.rs         AppState, app() router, static      T1
src/db.rs          connect_with_retry, migrate, test_pool   T1
src/models.rs      ItemSummary, wiki URL helpers       T1
src/tree.rs        load_tree: build tree + progress    T2
src/lua.rs         eval_module (sandboxed mlua)        T2
src/wiki.rs        WikiClient, MODULES                 T2
src/import.rs      run_import, map_modules             T3
tests/fixtures/*.json  trimmed real module JSON        T2 (written), T3 (used)
src/api/mod.rs     router(), CurrentUser, ApiError     T1
src/api/catalog.rs items search/detail, meta           T4
src/api/user.rs    wishlist, progress, inventory       T4
src/plan.rs        planner + /api/plan                 T5
web/**             React app                           T6
Dockerfile, .github/workflows/{ci,release}.yml         T7
~/repos/homelab-flagops-templates/apps/warframe-tracker/**  T7 (separate repo, branch only)
```

## API Contract (all tasks code against this exact JSON)

```ts
type ItemSummary = { id: number; name: string; category: string; subcategory: string | null;
  is_prime: boolean; is_vaulted: boolean | null; image_url: string | null; wiki_url: string | null };

type ProgressNode = { item: ItemSummary; needed: number; have: number; obtained: boolean;
  covered: boolean; children: ProgressNode[] };

type Source = { kind: "mission" | "enemy" | "container" | "planet" | "relic"; name: string;
  rotation: string | null; chance: number | null; quantity: number | null;
  via: string | null;          // relic name when this mission drops a relic that drops the need
  vaulted: boolean | null; rarity: string | null };  // set for kind=relic

GET  /api/items?q=&category=&limit=  -> ItemSummary[]              (limit default 25, max 100)
GET  /api/items/:id                  -> { item: ItemSummary; tree: ProgressNode; sources: Source[] }
GET  /api/wishlist                   -> { item: ItemSummary; added_at: string; completed_at: string | null;
                                          progress_pct: number; tree: ProgressNode }[]
POST /api/wishlist {item_id}         -> 204   (idempotent)
DELETE /api/wishlist/:item_id        -> 204
PATCH /api/wishlist/:item_id {completed: boolean} -> 204
PUT  /api/progress/:item_id {obtained: boolean}   -> 204
GET  /api/inventory                  -> { item: ItemSummary; count: number; source: string; updated_at: string }[]
PUT  /api/inventory [{item_id, count}] -> 204  (source='plugin' if header X-Inventory-Source: plugin else 'manual')
PATCH /api/inventory/:item_id {count} -> 204
GET  /api/plan -> { by_need: Need[]; by_source: SourceGroup[]; unsourced: Need[] }
  Need        = { item: ItemSummary; needed: number; have: number; for: {id: number; name: string}[]; sources: Source[] }
  SourceGroup = { kind: string; name: string; rotation: string | null; best_chance: number | null;
                  vaulted: boolean; covers: { item_id: number; name: string; chance: number | null; via: string | null }[] }
GET  /api/meta -> { last_import_at: string | null; item_count: number;
                    sources: { title: string; rev_id: number; fetched_at: string }[] }
```

Timestamps are RFC 3339 strings. `image_url` = `https://wiki.warframe.com/w/Special:FilePath/<Image>`; `wiki_url` = `https://wiki.warframe.com/w/<Link with spaces→_>`.

---

### Task 1: Scaffold, schema, DB helpers, router skeleton

**Files:**
- Create: `Cargo.toml`, `mise.toml`, `compose.yaml`, `.gitignore`, `schema.sql`, `src/main.rs`, `src/lib.rs`, `src/db.rs`, `src/models.rs`, `src/api/mod.rs`, `src/api/catalog.rs`, `src/api/user.rs`, `src/plan.rs`, `src/import.rs` (stubs for the last four)

**Interfaces:**
- Produces:
  - `pub struct AppState { pub pool: PgPool }` (Clone)
  - `pub fn app(state: AppState, web_dir: &str) -> Router`
  - `db::connect_with_retry(url: &str) -> PgPool`
  - `db::migrate(&PgPool) -> sqlx::Result<()>`
  - `#[cfg(test)] db::test_pool() -> PgPool` (fresh, migrated database)
  - `models::ItemSummary`, `models::ITEM_COLS: &str` (a SELECT column list for `items i`), `models::item_from_row(&PgRow) -> ItemSummary`
  - `api::CurrentUser(pub i64)` extractor, `api::ApiError` (`NotFound`, `BadRequest(String)`, `Internal`), `api::ApiResult<T> = Result<T, ApiError>`, impl `From<sqlx::Error> for ApiError`
  - `api::catalog::routes()`, `api::user::routes()`, `plan::routes()`, each `-> Router<AppState>`

- [ ] **Step 1: Cargo + tooling**

```bash
git switch -c feat/v1
cargo init --name warframe-tracker
cargo add axum@0.8 tokio@1 --features tokio/full
cargo add sqlx@0.9 --no-default-features --features runtime-tokio,postgres,tls-rustls-ring-webpki,json,chrono
cargo add serde --features derive
cargo add serde_json anyhow chrono --features chrono/serde
cargo add tower-http --features fs
cargo add --dev tower --features util
cargo add --dev http-body-util
```

`mise.toml`:
```toml
[tools]
rust = { version = "1.94.0", components = "clippy,rustfmt" }
node = "25"

[env]
DATABASE_URL = "postgres://postgres:postgres@localhost:5433/warframe"

[tasks.db]
description = "Start only postgres in docker"
run = "docker compose up -d --wait db"

[tasks.run]
depends = ["db"]
run = "cargo run -- serve"

[tasks.import]
depends = ["db"]
run = "cargo run -- import"

[tasks.lint]
run = ["cargo fmt --check", "cargo clippy --all-targets -- -D warnings"]

[tasks.test]
depends = ["db"]
run = "cargo test"

[tasks.web]
dir = "web"
run = "npm ci && npm run build"
```

`compose.yaml`:
```yaml
name: warframe-tracker
services:
  db:
    image: postgres:17
    environment:
      POSTGRES_PASSWORD: postgres
      POSTGRES_DB: warframe
    ports:
      - "5433:5432"
    volumes:
      - pgdata:/var/lib/postgresql/data
    healthcheck:
      test: ["CMD-SHELL", "pg_isready -U postgres -d warframe"]
      interval: 2s
      retries: 15
volumes:
  pgdata:
```

`.gitignore`: `/target`, `web/node_modules`, `web/dist`, `/wiki-json`.

- [ ] **Step 2: Schema** (`schema.sql`, run on every boot under an advisory lock)

```sql
SELECT pg_advisory_xact_lock(hashtext('warframe-tracker-migrate'));

CREATE TABLE IF NOT EXISTS wiki_sources (
  title text PRIMARY KEY,
  rev_id bigint NOT NULL,
  fetched_at timestamptz NOT NULL DEFAULT now(),
  source text NOT NULL
);

CREATE TABLE IF NOT EXISTS import_runs (
  id bigserial PRIMARY KEY,
  finished_at timestamptz NOT NULL DEFAULT now(),
  summary jsonb NOT NULL
);

CREATE TABLE IF NOT EXISTS items (
  id bigserial PRIMARY KEY,
  wiki_name text NOT NULL,
  category text NOT NULL CHECK (category IN
    ('warframe','weapon','mod','arcane','resource','component','relic')),
  subcategory text,
  is_prime boolean NOT NULL DEFAULT false,
  is_vaulted boolean,
  image text,
  wiki_link text,
  data jsonb NOT NULL DEFAULT '{}',
  removed_at timestamptz,
  UNIQUE (category, wiki_name)
);
CREATE INDEX IF NOT EXISTS items_name_lower ON items (lower(wiki_name));

CREATE TABLE IF NOT EXISTS recipes (
  result_item_id bigint NOT NULL REFERENCES items(id),
  ingredient_item_id bigint NOT NULL REFERENCES items(id),
  count integer NOT NULL CHECK (count > 0),
  PRIMARY KEY (result_item_id, ingredient_item_id)
);
CREATE TABLE IF NOT EXISTS recipe_meta (
  result_item_id bigint PRIMARY KEY REFERENCES items(id),
  credits integer, build_seconds integer
);
CREATE TABLE IF NOT EXISTS relic_drops (
  relic_item_id bigint NOT NULL REFERENCES items(id),
  part_item_id bigint NOT NULL REFERENCES items(id),
  rarity text,
  PRIMARY KEY (relic_item_id, part_item_id)
);
CREATE TABLE IF NOT EXISTS drop_sources (
  id bigserial PRIMARY KEY,
  source_kind text NOT NULL CHECK (source_kind IN ('mission','enemy','container','planet')),
  source_name text NOT NULL,
  rotation text,
  item_id bigint NOT NULL REFERENCES items(id),
  chance numeric,
  quantity integer
);
CREATE INDEX IF NOT EXISTS drop_sources_item ON drop_sources (item_id);

CREATE TABLE IF NOT EXISTS users (
  id bigserial PRIMARY KEY,
  name text NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now()
);
INSERT INTO users (id, name) VALUES (1, 'default') ON CONFLICT DO NOTHING;
SELECT setval('users_id_seq', GREATEST((SELECT max(id) FROM users), 1));

CREATE TABLE IF NOT EXISTS wishlist (
  user_id bigint NOT NULL REFERENCES users(id),
  item_id bigint NOT NULL REFERENCES items(id),
  added_at timestamptz NOT NULL DEFAULT now(),
  completed_at timestamptz,
  PRIMARY KEY (user_id, item_id)
);
CREATE TABLE IF NOT EXISTS component_progress (
  user_id bigint NOT NULL REFERENCES users(id),
  item_id bigint NOT NULL REFERENCES items(id),
  obtained boolean NOT NULL,
  PRIMARY KEY (user_id, item_id)
);
CREATE TABLE IF NOT EXISTS inventory (
  user_id bigint NOT NULL REFERENCES users(id),
  item_id bigint NOT NULL REFERENCES items(id),
  count bigint NOT NULL CHECK (count >= 0),
  updated_at timestamptz NOT NULL DEFAULT now(),
  source text NOT NULL DEFAULT 'manual' CHECK (source IN ('manual','plugin')),
  PRIMARY KEY (user_id, item_id)
);
```

- [ ] **Step 3: Write failing test for `db::test_pool` + migrate idempotency** (in `src/db.rs`)

```rust
#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn migrate_is_idempotent_and_seeds_default_user() {
        let pool = super::test_pool().await;
        super::migrate(&pool).await.unwrap(); // second run
        let name: String = sqlx::query_scalar("SELECT name FROM users WHERE id = 1")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(name, "default");
    }
}
```

Run `mise run test`. Expected: compile failure (no `test_pool`).

- [ ] **Step 4: Implement `src/db.rs`**

```rust
use sqlx::PgPool;
use std::time::Duration;

pub async fn connect_with_retry(url: &str) -> PgPool {
    let mut delay = Duration::from_millis(500);
    loop {
        match PgPool::connect(url).await {
            Ok(p) => return p,
            Err(e) => {
                eprintln!("db connect failed ({e}); retrying in {delay:?}");
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(30));
            }
        }
    }
}

pub async fn migrate(pool: &PgPool) -> sqlx::Result<()> {
    // raw_sql runs the multi-statement file in one implicit transaction,
    // so the advisory xact lock serializes concurrent boots.
    sqlx::raw_sql(include_str!("../schema.sql")).execute(pool).await?;
    Ok(())
}

/// Fresh database per call so tests (and parallel worktrees) never share state.
// ponytail: test databases are never dropped; `docker compose down -v` clears them.
#[cfg(test)]
pub async fn test_pool() -> PgPool {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let base = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let admin = PgPool::connect(&base).await.unwrap();
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
    let name = format!("t_{}_{}_{}", std::process::id(), nanos, N.fetch_add(1, Ordering::Relaxed));
    sqlx::raw_sql(&format!("CREATE DATABASE {name}")).execute(&admin).await.unwrap();
    let (prefix, _) = base.rsplit_once('/').unwrap();
    let pool = PgPool::connect(&format!("{prefix}/{name}")).await.unwrap();
    migrate(&pool).await.unwrap();
    pool
}
```

- [ ] **Step 5: `src/models.rs`**

```rust
use serde::Serialize;
use sqlx::{postgres::PgRow, Row};

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct ItemSummary {
    pub id: i64,
    pub name: String,
    pub category: String,
    pub subcategory: Option<String>,
    pub is_prime: bool,
    pub is_vaulted: Option<bool>,
    pub image_url: Option<String>,
    pub wiki_url: Option<String>,
}

/// Columns for `FROM items i`; pair with `item_from_row`.
pub const ITEM_COLS: &str =
    "i.id, i.wiki_name, i.category, i.subcategory, i.is_prime, i.is_vaulted, i.image, i.wiki_link";

pub fn item_from_row(r: &PgRow) -> ItemSummary {
    let image: Option<String> = r.get("image");
    let link: Option<String> = r.get("wiki_link");
    ItemSummary {
        id: r.get("id"),
        name: r.get("wiki_name"),
        category: r.get("category"),
        subcategory: r.get("subcategory"),
        is_prime: r.get("is_prime"),
        is_vaulted: r.get("is_vaulted"),
        image_url: image.map(|f| format!("https://wiki.warframe.com/w/Special:FilePath/{}", f.replace(' ', "_"))),
        wiki_url: link.map(|l| format!("https://wiki.warframe.com/w/{}", l.replace(' ', "_"))),
    }
}
```

- [ ] **Step 6: `src/api/mod.rs`**

```rust
pub mod catalog;
pub mod user;

use crate::AppState;
use axum::{extract::FromRequestParts, http::{request::Parts, StatusCode}, response::{IntoResponse, Response}, Json, Router};

pub fn router() -> Router<AppState> {
    Router::new()
        .merge(catalog::routes())
        .merge(user::routes())
        .merge(crate::plan::routes())
}

/// The only place a request learns who its user is. Replace the body with a session lookup to add auth.
pub struct CurrentUser(pub i64);

impl<S: Send + Sync> FromRequestParts<S> for CurrentUser {
    type Rejection = ApiError;
    async fn from_request_parts(_: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(CurrentUser(1))
    }
}

#[derive(Debug)]
pub enum ApiError { NotFound, BadRequest(String), Internal(String) }
pub type ApiResult<T> = Result<T, ApiError>;

impl From<sqlx::Error> for ApiError {
    fn from(e: sqlx::Error) -> Self {
        // FK violation on a write means the referenced item doesn't exist.
        if let sqlx::Error::Database(d) = &e { if d.code().as_deref() == Some("23503") { return ApiError::NotFound; } }
        ApiError::Internal(e.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (code, msg) = match self {
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m),
            ApiError::Internal(m) => { eprintln!("internal error: {m}"); (StatusCode::INTERNAL_SERVER_ERROR, "internal error".into()) }
        };
        (code, Json(serde_json::json!({ "error": msg }))).into_response()
    }
}
```

`src/api/catalog.rs`, `src/api/user.rs` and `src/plan.rs` stubs:
```rust
use crate::AppState;
use axum::Router;
pub fn routes() -> Router<AppState> { Router::new() }
```
`src/import.rs` stub: `pub async fn run_import(_pool: &sqlx::PgPool) -> anyhow::Result<()> { anyhow::bail!("not implemented") }`

- [ ] **Step 7: `src/lib.rs` + `src/main.rs`**

```rust
// lib.rs
pub mod api; pub mod db; pub mod import; pub mod lua; pub mod models; pub mod plan; pub mod tree; pub mod wiki;

use axum::Router;
use sqlx::PgPool;
use tower_http::services::{ServeDir, ServeFile};

#[derive(Clone)]
pub struct AppState { pub pool: PgPool }

pub fn app(state: AppState, web_dir: &str) -> Router {
    let spa = ServeDir::new(web_dir).fallback(ServeFile::new(format!("{web_dir}/index.html")));
    Router::new().nest("/api", api::router()).fallback_service(spa).with_state(state)
}
```
(`lua`, `tree`, `wiki` start as empty files and are filled in by Task 2.)

```rust
// main.rs
use warframe_tracker::{app, db, import, AppState};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cmd = std::env::args().nth(1).unwrap_or_else(|| "serve".into());
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    match cmd.as_str() {
        "serve" => {
            let pool = db::connect_with_retry(&url).await;
            db::migrate(&pool).await?;
            let web_dir = std::env::var("WEB_DIR").unwrap_or_else(|_| "web/dist".into());
            let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
            println!("listening on :3000");
            axum::serve(listener, app(AppState { pool }, &web_dir)).await?;
        }
        "import" => {
            let pool = db::connect_with_retry(&url).await;
            db::migrate(&pool).await?;
            import::run_import(&pool).await?;
        }
        other => anyhow::bail!("unknown command {other}; expected serve | import | dump <dir>"),
    }
    Ok(())
}
```

- [ ] **Step 8: Verify and commit**

Run `mise run test` and `mise run lint`. Expected: 1 test passes, clippy clean.

```bash
git add -A && git commit -m "feat: scaffold server, schema and db helpers"
```

---

### Task 2: Lua evaluation, wiki client, `dump`, shared build tree

**Files:**
- Create: `src/lua.rs`, `src/wiki.rs`, `src/tree.rs`, `tests/fixtures/*.json`
- Modify: `src/main.rs` (add `dump`), `Cargo.toml`

**Interfaces:**
- Produces:
  - `lua::eval_module(title: &str, sources: &HashMap<String, String>) -> anyhow::Result<serde_json::Value>`
  - `wiki::MODULES: &[&str]` (all titles to fetch, including `Module:Table`)
  - `wiki::WikiClient::new(base: &str) -> Self` (base = `https://wiki.warframe.com`)
  - `WikiClient::revisions(&self, titles: &[&str]) -> anyhow::Result<HashMap<String, i64>>`
  - `WikiClient::raw(&self, title: &str) -> anyhow::Result<String>`
  - `tree::load_tree(pool: &PgPool, user_id: i64, root_item_id: i64) -> sqlx::Result<Option<ProgressNode>>`
  - `tree::ProgressNode { item: ItemSummary, needed: i64, have: i64, obtained: bool, covered: bool, children: Vec<ProgressNode> }` (Serialize)
  - `ProgressNode::progress_pct(&self) -> f64`, the share of top-level children covered (100 if the node is covered or has no children)
  - The `dump <dir>` subcommand writes `<dir>/<title with '/' and ':' → '_'>.json` for every module except `Module:Table`.

- [ ] **Step 1: Add deps**

```bash
cargo add mlua --features lua51,vendored,serialize
cargo add reqwest --no-default-features --features rustls-tls,json
```

- [ ] **Step 2: Failing tests for `lua.rs`**

```rust
#[cfg(test)]
mod tests {
    use super::eval_module;
    use std::collections::HashMap;

    fn src(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn evaluates_table_literal_with_tuples() {
        let s = src(&[("Module:A", r#"return { X = { Count = 2, Parts = { { "Neurodes", "Resource", 12.5 } } } }"#)]);
        let v = eval_module("Module:A", &s).unwrap();
        assert_eq!(v["X"]["Count"], 2);
        assert_eq!(v["X"]["Parts"][0][0], "Neurodes");
        assert_eq!(v["X"]["Parts"][0][2], 12.5);
    }

    #[test]
    fn require_resolves_only_known_modules() {
        let s = src(&[
            ("Module:A", "local T = require('Module:T') return { n = T.double(2) }"),
            ("Module:T", "return { double = function(x) return x * 2 end }"),
        ]);
        assert_eq!(eval_module("Module:A", &s).unwrap()["n"], 4);
        let bad = src(&[("Module:A", "return require('Module:Missing')")]);
        assert!(eval_module("Module:A", &bad).is_err());
    }

    #[test]
    fn sandbox_blocks_os_io_and_load() {
        for code in ["return os.time()", "return io.open('x')", "return loadstring('return 1')()"] {
            let s = src(&[("Module:A", code)]);
            assert!(eval_module("Module:A", &s).is_err(), "{code} should fail");
        }
    }

    #[test]
    fn functions_in_returned_table_are_dropped() {
        let s = src(&[("Module:A", "return { a = 1, f = function() end }")]);
        let v = eval_module("Module:A", &s).unwrap();
        assert_eq!(v["a"], 1);
        assert!(v.get("f").is_none());
    }
}
```

Run `cargo test lua`. Expected: compile failure.

- [ ] **Step 3: Implement `src/lua.rs`**

```rust
use anyhow::{anyhow, Context};
use mlua::{DeserializeOptions, Lua, LuaOptions, LuaSerdeExt, StdLib, Value};
use std::collections::HashMap;

/// Evaluate a wiki data module the way Scribunto would, minus `mw` and any I/O.
pub fn eval_module(title: &str, sources: &HashMap<String, String>) -> anyhow::Result<serde_json::Value> {
    let lua = Lua::new_with(StdLib::STRING | StdLib::TABLE | StdLib::MATH, LuaOptions::default())?;
    let g = lua.globals();
    for f in ["load", "loadstring", "loadfile", "dofile"] { g.set(f, Value::Nil)?; }

    let sources = sources.clone();
    let loaded = lua.create_table()?;
    let require = lua.create_function(move |lua, name: String| {
        if let Ok(v @ Value::Table(_)) = loaded.get::<Value>(name.as_str()) { return Ok(v); }
        let code = sources.get(&name).ok_or_else(|| mlua::Error::runtime(format!("require of unknown module {name}")))?;
        let v: Value = lua.load(code.as_str()).set_name(name.as_str()).eval()?;
        loaded.set(name.as_str(), v.clone())?;
        Ok(v)
    })?;
    g.set("require", require.clone())?;

    let value: Value = require.call(title.to_string()).with_context(|| format!("evaluating {title}"))?;
    let opts = DeserializeOptions::new().deny_unsupported_types(false);
    lua.from_value_with(value, opts).map_err(|e| anyhow!("converting {title} to JSON: {e}"))
}
```

If a real module fails with a recursive-table error, add `.deny_recursive_tables(false)` and note it in a `ponytail:` comment.

- [ ] **Step 4: Run lua tests.** Expected: all 4 pass.

- [ ] **Step 5: Failing test for `wiki.rs`** (a local axum stub server, no network)

```rust
#[cfg(test)]
mod tests {
    use super::WikiClient;
    use axum::{extract::Query, routing::get, Router};
    use std::collections::HashMap;

    async fn stub() -> String {
        let app = Router::new()
            .route("/api.php", get(|| async {
                r#"{"query":{"pages":{"1":{"title":"Module:A","revisions":[{"revid":42}]},"-1":{"title":"Module:Gone","missing":""}}}}"#
            }))
            .route("/index.php", get(|Query(q): Query<HashMap<String, String>>| async move {
                format!("return '{}'", q["title"])
            }));
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn parses_revisions_and_skips_missing() {
        let c = WikiClient::new(&stub().await);
        let revs = c.revisions(&["Module:A", "Module:Gone"]).await.unwrap();
        assert_eq!(revs.get("Module:A"), Some(&42));
        assert!(!revs.contains_key("Module:Gone"));
    }

    #[tokio::test]
    async fn fetches_raw_source() {
        let c = WikiClient::new(&stub().await);
        assert_eq!(c.raw("Module:A").await.unwrap(), "return 'Module:A'");
    }
}
```

- [ ] **Step 6: Implement `src/wiki.rs`**

```rust
use anyhow::Context;
use std::collections::HashMap;

pub const MODULES: &[&str] = &[
    "Module:Warframes/data",
    "Module:Weapons/data/primary",
    "Module:Weapons/data/secondary",
    "Module:Weapons/data/melee",
    "Module:Mods/data",
    "Module:Arcane/data",
    "Module:Blueprints/data",
    "Module:Resources/data",
    "Module:Void/data",
    "Module:DropTables/data",
    "Module:Missions/data",
    "Module:Table",
];

pub struct WikiClient { base: String, http: reqwest::Client }

impl WikiClient {
    pub fn new(base: &str) -> Self {
        let ua = format!("warframe-tracker/{} (+https://github.com/graytonio/warframe-tracker)", env!("CARGO_PKG_VERSION"));
        let http = reqwest::Client::builder().user_agent(ua).build().expect("reqwest client");
        Self { base: base.trim_end_matches('/').to_string(), http }
    }

    /// One request for every title (API limit is 50 titles; MODULES is well under that).
    pub async fn revisions(&self, titles: &[&str]) -> anyhow::Result<HashMap<String, i64>> {
        let v: serde_json::Value = self.http.get(format!("{}/api.php", self.base))
            .query(&[("action", "query"), ("format", "json"), ("prop", "revisions"),
                     ("rvprop", "ids"), ("titles", &titles.join("|"))])
            .send().await?.error_for_status()?.json().await?;
        let pages = v["query"]["pages"].as_object().context("no query.pages in response")?;
        Ok(pages.values().filter_map(|p| {
            Some((p["title"].as_str()?.to_string(), p["revisions"][0]["revid"].as_i64()?))
        }).collect())
    }

    pub async fn raw(&self, title: &str) -> anyhow::Result<String> {
        Ok(self.http.get(format!("{}/index.php", self.base))
            .query(&[("title", title), ("action", "raw")])
            .send().await?.error_for_status()?.text().await?)
    }
}
```

- [ ] **Step 7: Run the wiki tests.** Expected: both pass.

- [ ] **Step 8: Add `dump` to `main.rs`** (it doesn't need a DB, so match it before reading `DATABASE_URL`)

```rust
if cmd == "dump" {
    let dir = std::env::args().nth(2).unwrap_or_else(|| "wiki-json".into());
    std::fs::create_dir_all(&dir)?;
    let client = warframe_tracker::wiki::WikiClient::new("https://wiki.warframe.com");
    let mut sources = std::collections::HashMap::new();
    for t in warframe_tracker::wiki::MODULES { sources.insert(t.to_string(), client.raw(t).await?); }
    for t in warframe_tracker::wiki::MODULES.iter().filter(|t| **t != "Module:Table") {
        let v = warframe_tracker::lua::eval_module(t, &sources)?;
        let file = format!("{dir}/{}.json", t.replace([':', '/'], "_"));
        std::fs::write(&file, serde_json::to_vec_pretty(&v)?)?;
        println!("wrote {file}");
    }
    return Ok(());
}
```

Run `cargo run -- dump wiki-json`. Expected: 11 JSON files. If a module fails to evaluate, fix `lua.rs` (e.g. stub a missing global) and add a regression test reproducing it.

- [ ] **Step 9: Trimmed fixtures for Task 3.** With `jq`, copy a small slice of each dump into `tests/fixtures/` (same file names). Each fixture keeps the real nesting and top-level keys but only these entries:
  - Warframes: `Saryn`, `Saryn Prime`
  - Weapons primary: `Braton`, `Braton Prime`
  - Mods: `Serration`
  - Arcane: `Arcane Energize`
  - Blueprints: `Saryn Prime`, `Saryn Prime Chassis`, `Saryn Prime Neuroptics`, `Saryn Prime Systems`, `Braton Prime`
  - Resources: `Neurodes`, `Orokin Cell`, `Rubedo`, `Nano Spores`, `Alloy Plate`, `Salvage`, `Neural Sensors`, `Nitain Extract`
  - Void: every relic whose drops include `Saryn Prime` or `Braton Prime`
  - DropTables: one mission whose rewards include one of those relics, the `Aerial Commander` enemy, and one container
  - Missions: the whole `RegionResources` block

  Record the `jq` commands in `tests/fixtures/README.md`.

- [ ] **Step 10: Failing tests for `tree.rs`** (seed via SQL)

```rust
#[cfg(test)]
mod tests {
    use super::load_tree;
    use crate::db::test_pool;

    /// Saryn Prime ← {Chassis ×1, Neuroptics ×1, Orokin Cell ×1}; Chassis ← {Rubedo ×1200}; Neuroptics ← {Rubedo ×750}
    async fn seed(pool: &sqlx::PgPool) -> i64 {
        sqlx::raw_sql(r#"
          INSERT INTO items (id, wiki_name, category) VALUES
            (1,'Saryn Prime','warframe'),(2,'Saryn Prime Chassis','component'),
            (3,'Saryn Prime Neuroptics','component'),(4,'Orokin Cell','resource'),(5,'Rubedo','resource');
          INSERT INTO recipes VALUES (1,2,1),(1,3,1),(1,4,1),(2,5,1200),(3,5,750);
        "#).execute(pool).await.unwrap();
        1
    }

    #[tokio::test]
    async fn builds_tree_with_multiplied_counts() {
        let pool = test_pool().await;
        let root = seed(&pool).await;
        let t = load_tree(&pool, 1, root).await.unwrap().unwrap();
        assert_eq!(t.children.len(), 3);
        let chassis = t.children.iter().find(|c| c.item.name == "Saryn Prime Chassis").unwrap();
        assert_eq!(chassis.children[0].needed, 1200);
        assert!(!t.covered);
    }

    #[tokio::test]
    async fn obtained_or_inventory_marks_covered() {
        let pool = test_pool().await;
        let root = seed(&pool).await;
        sqlx::raw_sql("INSERT INTO component_progress VALUES (1,2,true); INSERT INTO inventory (user_id,item_id,count) VALUES (1,4,1);")
            .execute(&pool).await.unwrap();
        let t = load_tree(&pool, 1, root).await.unwrap().unwrap();
        let by = |n: &str| t.children.iter().find(|c| c.item.name == n).unwrap();
        assert!(by("Saryn Prime Chassis").covered && by("Saryn Prime Chassis").obtained);
        assert!(by("Orokin Cell").covered);
        assert!(!by("Saryn Prime Neuroptics").covered);
        assert!((t.progress_pct() - 66.666).abs() < 0.1);
    }

    #[tokio::test]
    async fn unknown_root_is_none() {
        let pool = test_pool().await;
        assert!(load_tree(&pool, 1, 999).await.unwrap().is_none());
    }
}
```

- [ ] **Step 11: Implement `src/tree.rs`**

```rust
use crate::models::{item_from_row, ItemSummary, ITEM_COLS};
use serde::Serialize;
use sqlx::{PgPool, Row};
use std::collections::HashMap;

#[derive(Serialize, Clone, Debug)]
pub struct ProgressNode {
    pub item: ItemSummary,
    pub needed: i64,
    pub have: i64,
    pub obtained: bool,
    pub covered: bool,
    pub children: Vec<ProgressNode>,
}

impl ProgressNode {
    pub fn progress_pct(&self) -> f64 {
        if self.covered || self.children.is_empty() { return 100.0; }
        let done = self.children.iter().filter(|c| c.covered).count();
        done as f64 * 100.0 / self.children.len() as f64
    }
}

const MAX_DEPTH: usize = 8; // guards against cyclic wiki recipes

pub async fn load_tree(pool: &PgPool, user_id: i64, root: i64) -> sqlx::Result<Option<ProgressNode>> {
    // Every item reachable from root, with this user's progress and inventory.
    let sql = format!(r#"
        WITH RECURSIVE reach(id, depth) AS (
            SELECT $2::bigint, 0
            UNION
            SELECT r.ingredient_item_id, reach.depth + 1 FROM recipes r JOIN reach ON r.result_item_id = reach.id
            WHERE reach.depth < {MAX_DEPTH}
        )
        SELECT DISTINCT {ITEM_COLS},
               COALESCE(cp.obtained, false) AS obtained, COALESCE(inv.count, 0) AS have
        FROM reach JOIN items i ON i.id = reach.id
        LEFT JOIN component_progress cp ON cp.user_id = $1 AND cp.item_id = i.id
        LEFT JOIN inventory inv ON inv.user_id = $1 AND inv.item_id = i.id"#);
    let rows = sqlx::query(&sql).bind(user_id).bind(root).fetch_all(pool).await?;
    if rows.is_empty() { return Ok(None); }
    let nodes: HashMap<i64, (ItemSummary, bool, i64)> = rows.iter()
        .map(|r| { let it = item_from_row(r); (it.id, (it, r.get("obtained"), r.get("have"))) })
        .collect();
    let ids: Vec<i64> = nodes.keys().copied().collect();
    let edges: Vec<(i64, i64, i32)> = sqlx::query_as(
        "SELECT result_item_id, ingredient_item_id, count FROM recipes WHERE result_item_id = ANY($1) ORDER BY ingredient_item_id")
        .bind(&ids).fetch_all(pool).await?;
    let mut kids: HashMap<i64, Vec<(i64, i64)>> = HashMap::new();
    for (p, c, n) in edges { kids.entry(p).or_default().push((c, n as i64)); }
    Ok(Some(build(root, 1, 0, &nodes, &kids)))
}

fn build(id: i64, needed: i64, depth: usize, nodes: &HashMap<i64, (ItemSummary, bool, i64)>, kids: &HashMap<i64, Vec<(i64, i64)>>) -> ProgressNode {
    let (item, obtained, have) = nodes[&id].clone();
    let children = if depth >= MAX_DEPTH { vec![] } else {
        kids.get(&id).into_iter().flatten()
            .filter(|(c, _)| nodes.contains_key(c))
            .map(|(c, n)| build(*c, needed * n, depth + 1, nodes, kids)).collect()
    };
    ProgressNode { covered: obtained || have >= needed, item, needed, have, obtained, children }
}
```

- [ ] **Step 12: Run all tests plus lint, then commit**

```bash
mise run test && mise run lint
git add -A && git commit -m "feat: sandboxed lua eval, wiki client, dump command, build tree"
```

---

### Task 3: Importer (map module JSON → game tables, revision gating)

**Files:**
- Modify: `src/import.rs` (replaces the stub)
- Test: inline `#[cfg(test)]` in `src/import.rs`, using `tests/fixtures/*.json`

**Interfaces:**
- Consumes: `lua::eval_module`, `wiki::{WikiClient, MODULES}`, `db::test_pool`
- Produces:
  - `pub async fn run_import(pool: &PgPool) -> anyhow::Result<()>` (signature unchanged from the stub), which calls `run_import_with(pool, &WikiClient::new("https://wiki.warframe.com"))`
  - `pub async fn run_import_with(pool: &PgPool, client: &WikiClient) -> anyhow::Result<Option<ImportSummary>>`. Returns `None` when no revision changed **and** an `import_runs` row exists.
  - `pub async fn map_modules(tx: &mut PgConnection, modules: &HashMap<String, Value>) -> anyhow::Result<ImportSummary>`
  - `#[derive(Serialize, Default)] pub struct ImportSummary { pub items: usize, pub recipes: usize, pub relic_drops: usize, pub drop_sources: usize, pub unresolved: Vec<String> }`

**Inspect the real data first.** The lead gives you `$WIKI_JSON`, a directory of full JSON dumps. Before writing the mapping, open the dumps with `jq` and confirm each shape below. Where the real data differs, follow the data and update this task's notes in your final report.

**Mapping rules** (keys are module titles; values are evaluated JSON):

1. **Items:**

   | Module | Key path | category | subcategory |
   |---|---|---|---|
   | Warframes/data | `.Warframes` | `warframe` | — |
   | Weapons/data/{slot} | top level | `weapon` | slot name |
   | Mods/data | `.Mods` | `mod` | `Type` field |
   | Arcane/data | `.Arcanes` | `arcane` | — |
   | Resources/data | `.Resources` | `resource` | — |
   | Void/data | `.RelicData` keys | `relic` | `Tier` |

   - Fields: `wiki_name` = the map key; `image` = `Image`; `wiki_link` = `Link` (default: the key); `data` = the whole record; `is_prime` = name ends with `" Prime"` or contains `" Prime "`.
   - Relics: `is_vaulted` = `Vaulted` is present and not null.
   - Upsert on `(category, wiki_name)`, setting `removed_at = NULL`. Afterwards, `UPDATE items SET removed_at = now()` for any `(category, wiki_name)` in the v1 categories not seen in this run.
2. **Recipes** from `Blueprints/data`:
   - Flatten every top-level group (`Blueprints`, `Suits`, …) into a single `name → blueprint` map.
   - For each blueprint `B` with `Result = R` and each part `{Count, Name, Type}`:
     - **Resolve the part:**
       - Strip a leading `"Prime "` from `Name`.
       - If `"{R} {stripped}"` is a key in the blueprint map, or equals a relic-part component name (rule 3), it's a **component**: upsert a `component` item with that name, with `is_prime` inherited from R.
       - Otherwise, resolve `Name` against the existing items, preferring the category that `Type` implies (`Resource` → resource; `Item`/`Weapon` → weapon, then warframe, then component).
       - If nothing matches, push `"{R}: {Name}"` to `unresolved` and skip the part.
     - Insert `recipes(R, part, Count)`, summing counts if the same pair repeats.
     - `recipe_meta(R, Credits, Time)`.
   - Every non-component result item R with a blueprint also gets a `component` item named `"{R} Blueprint"` as a recipe ingredient (count 1). That's the blueprint the player has to obtain.
3. **Relic drops** from `Void/data.RelicData[relic].Drops[] = {Item, Part, Rarity}`:
   - Component name:
     - `Part` in (`"Blueprint"`, `""`): `"{Item} Blueprint"`
     - otherwise: `"{Item} {Part minus trailing ' Blueprint'}"`
   - Upsert the component (with `is_prime = true`), then `relic_drops(relic, component, Rarity)`.
   - Component `is_vaulted`: true only if every relic that drops it is vaulted. Set it with a single `UPDATE … FROM (SELECT part_item_id, bool_and(r.is_vaulted) …)` after inserting the drops.
4. **Drop sources** from `DropTables/data`:
   - `Missions[*].Rewards`: either a map from rotation (`A`/`B`/`C`) to entries, or a flat list (rotation NULL). Each entry is `[name, type, chance, quantity?]`. → `source_kind = 'mission'`, `source_name = Name`.
   - `Enemies[*]`: `Mods` and `Resources` entries `[name, type, chance]`. → `'enemy'`. Multiply the entry chance by `ModChance/100` or `ResourceChance/100`.
   - `Containers[*].Resources`: the same, but `'container'`, using `ResourceChance`.
   - Resolve each name by its type hint (`Mod`, `Arcane`, `Resource`, `Relic`, `Blueprint` → component).
     - Relic entries look like `"Axi A1 Relic"` or `"Axi A1"`: strip a trailing `" Relic"` and any refinement suffix such as `" (Intact)"`.
     - Unresolved names count into `unresolved` but don't fail the import.
     - `"Region Resource"` and `"Credits Cache"` are skipped silently.
5. **Planet sources** from `Missions/data.RegionResources[planet] = [names]`: `'planet'`, `source_name = planet`, chance NULL.
6. `recipes`, `recipe_meta`, `relic_drops` and `drop_sources` are emptied with `DELETE` at the start of `map_modules`. Everything runs inside the caller's transaction.

**`run_import_with` flow:**
1. Call `client.revisions(MODULES)`.
2. Load the `wiki_sources` rows.
3. For each title whose revision is missing or different, call `client.raw` and upsert `wiki_sources(title, rev_id, now(), source)`. Any error aborts the run.
4. If nothing changed and `import_runs` is non-empty, print `"wiki unchanged"` and return `Ok(None)`.
5. Otherwise:
   - Build `sources` from all `wiki_sources` rows.
   - `eval_module` each title except `Module:Table`.
   - Begin a transaction, call `map_modules`, insert `import_runs(summary)`, then commit.
   - Print the counts, plus the first 20 unresolved names.

Steps 3 and 5 share one transaction, so a failed eval leaves the stored revisions unchanged and the next run retries.

- [ ] **Step 1: Failing test — fixture import builds Saryn Prime correctly**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::test_pool;

    fn fixtures() -> HashMap<String, serde_json::Value> {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
        crate::wiki::MODULES.iter().filter(|t| **t != "Module:Table").map(|t| {
            let f = format!("{dir}/{}.json", t.replace([':', '/'], "_"));
            (t.to_string(), serde_json::from_slice(&std::fs::read(&f).unwrap()).unwrap())
        }).collect()
    }

    async fn import(pool: &PgPool) -> ImportSummary {
        let mut tx = pool.begin().await.unwrap();
        let s = map_modules(&mut tx, &fixtures()).await.unwrap();
        tx.commit().await.unwrap();
        s
    }

    async fn id(pool: &PgPool, cat: &str, name: &str) -> i64 {
        sqlx::query_scalar("SELECT id FROM items WHERE category=$1 AND wiki_name=$2")
            .bind(cat).bind(name).fetch_one(pool).await
            .unwrap_or_else(|_| panic!("missing {cat}/{name}"))
    }

    #[tokio::test]
    async fn saryn_prime_recipe_resolves_components_and_blueprint() {
        let pool = test_pool().await;
        import(&pool).await;
        let sp = id(&pool, "warframe", "Saryn Prime").await;
        let names: Vec<String> = sqlx::query_scalar(
            "SELECT i.wiki_name FROM recipes r JOIN items i ON i.id=r.ingredient_item_id WHERE r.result_item_id=$1 ORDER BY 1")
            .bind(sp).fetch_all(&pool).await.unwrap();
        assert_eq!(names, ["Orokin Cell", "Saryn Prime Blueprint", "Saryn Prime Chassis",
                           "Saryn Prime Neuroptics", "Saryn Prime Systems"]);
        let chassis = id(&pool, "component", "Saryn Prime Chassis").await;
        let rubedo: i32 = sqlx::query_scalar("SELECT count FROM recipes WHERE result_item_id=$1 AND ingredient_item_id=$2")
            .bind(chassis).bind(id(&pool, "resource", "Rubedo").await).fetch_one(&pool).await.unwrap();
        assert_eq!(rubedo, 1200);
    }

    #[tokio::test]
    async fn relic_parts_link_to_components() {
        let pool = test_pool().await;
        import(&pool).await;
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM relic_drops rd JOIN items i ON i.id=rd.part_item_id WHERE i.wiki_name='Saryn Prime Neuroptics'")
            .fetch_one(&pool).await.unwrap();
        assert!(n >= 1);
    }

    #[tokio::test]
    async fn reimport_keeps_ids_and_marks_removed() {
        let pool = test_pool().await;
        import(&pool).await;
        let sp = id(&pool, "warframe", "Saryn Prime").await;
        sqlx::query("INSERT INTO wishlist (user_id,item_id) VALUES (1,$1)").bind(sp).execute(&pool).await.unwrap();
        let mut m = fixtures();
        m.get_mut("Module:Warframes/data").unwrap()["Warframes"].as_object_mut().unwrap().remove("Saryn");
        let mut tx = pool.begin().await.unwrap();
        map_modules(&mut tx, &m).await.unwrap();
        tx.commit().await.unwrap();
        assert_eq!(id(&pool, "warframe", "Saryn Prime").await, sp);
        let removed: bool = sqlx::query_scalar("SELECT removed_at IS NOT NULL FROM items WHERE wiki_name='Saryn' AND category='warframe'")
            .fetch_one(&pool).await.unwrap();
        assert!(removed);
        let wl: i64 = sqlx::query_scalar("SELECT count(*) FROM wishlist").fetch_one(&pool).await.unwrap();
        assert_eq!(wl, 1);
    }

    #[tokio::test]
    async fn drop_sources_and_planets_import() {
        let pool = test_pool().await;
        let s = import(&pool).await;
        assert!(s.drop_sources > 0);
        let earth: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM drop_sources d JOIN items i ON i.id=d.item_id WHERE d.source_kind='planet' AND d.source_name='Earth' AND i.wiki_name='Neurodes'")
            .fetch_one(&pool).await.unwrap();
        assert_eq!(earth, 1);
    }
}
```

Run `cargo test import`. Expected: compile failure.

- [ ] **Step 2: Implement `map_modules`** using the rules above. Use one helper for every item write:

```rust
async fn upsert_item(tx: &mut PgConnection, cat: &str, name: &str, sub: Option<&str>, is_prime: bool,
                     image: Option<&str>, link: Option<&str>, data: &Value) -> sqlx::Result<i64> {
    sqlx::query_scalar(
        "INSERT INTO items (wiki_name, category, subcategory, is_prime, image, wiki_link, data, removed_at)
         VALUES ($1,$2,$3,$4,$5,$6,$7,NULL)
         ON CONFLICT (category, wiki_name) DO UPDATE SET subcategory=EXCLUDED.subcategory,
           is_prime=EXCLUDED.is_prime, image=EXCLUDED.image, wiki_link=EXCLUDED.wiki_link,
           data=EXCLUDED.data, removed_at=NULL
         RETURNING id")
        .bind(name).bind(cat).bind(sub).bind(is_prime).bind(image).bind(link).bind(data)
        .fetch_one(&mut *tx).await
}
```

Keep a `HashMap<(String,String), i64>` keyed by (category, name) for name resolution, so rows aren't re-queried. Batch inserts with `UNNEST($1::bigint[], …)` for recipes and drop_sources (they run into the thousands).

- [ ] **Step 3: Run the import tests.** Expected: 4 pass.

- [ ] **Step 4: Failing test for `run_import_with` gating.** Use a stub server like Task 2's: revisions are served from an `Arc<AtomicI64>`, and `/index.php` returns the fixture's source for each title. The fixture source is a Lua `return` of the fixture JSON, generated in the test by a small `json_to_lua(&Value) -> String` helper, so the test runs through the real eval path. Assert that:
  - (a) the first run returns `Some(summary)` with `items > 0`;
  - (b) the second run with the same revisions returns `None`, and the stub counts zero `/index.php` hits;
  - (c) after bumping one revision, the run returns `Some` and the stub counts exactly one `/index.php` hit.

- [ ] **Step 5: Implement `run_import_with` and `run_import`** following the flow above. Run the tests: all pass.

- [ ] **Step 6: Real-data check (needs network).** Run `mise run import`. Expected: a summary with thousands of items, and `unresolved` below about 5% of recipes plus drop_sources. List the top unresolved patterns in your report; if one pattern dominates, fix it with a mapping rule and a fixture test.

- [ ] **Step 7: Lint and commit**

```bash
mise run lint && git add -A && git commit -m "feat: wiki importer with revision-gated refresh"
```

---

### Task 4: Catalog, wishlist, progress, inventory, meta APIs

**Files:**
- Modify: `src/api/catalog.rs`, `src/api/user.rs`

**Interfaces:**
- Consumes: `AppState`, `api::{CurrentUser, ApiError, ApiResult}`, `models::{ItemSummary, ITEM_COLS, item_from_row}`, `tree::{load_tree, ProgressNode}`
- Produces: the routes in the API Contract for `/items`, `/items/:id`, `/meta` (catalog.rs) and `/wishlist`, `/progress`, `/inventory` (user.rs). Also `pub async fn item_sources(pool, item_id) -> sqlx::Result<Vec<Source>>` plus `#[derive(Serialize, Clone)] pub struct Source` (fields exactly as in the contract) in `catalog.rs`. **Task 5 has its own copy of this function in `plan.rs`, so don't import across**; Task 8 deduplicates.

Route syntax is axum 0.8: path params are `/{id}`, not `/:id`.

- [ ] **Step 1: Test harness + failing tests** (in `src/api/user.rs`; mirror for catalog)

```rust
#[cfg(test)]
mod tests {
    use crate::{app, db::test_pool, AppState};
    use axum::{body::Body, http::{Request, StatusCode}};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    async fn call(pool: &sqlx::PgPool, method: &str, uri: &str, body: Option<serde_json::Value>) -> (StatusCode, serde_json::Value) {
        let mut req = Request::builder().method(method).uri(uri);
        let body = match body { Some(b) => { req = req.header("content-type", "application/json"); Body::from(b.to_string()) } None => Body::empty() };
        let res = app(AppState { pool: pool.clone() }, "web/dist").oneshot(req.body(body).unwrap()).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, if bytes.is_empty() { serde_json::Value::Null } else { serde_json::from_slice(&bytes).unwrap() })
    }

    async fn seed(pool: &sqlx::PgPool) {
        sqlx::raw_sql(r#"
          INSERT INTO items (id, wiki_name, category, is_prime) VALUES
            (1,'Saryn Prime','warframe',true),(2,'Saryn Prime Chassis','component',true),(3,'Rubedo','resource',false);
          INSERT INTO recipes VALUES (1,2,1),(2,3,1200);
        "#).execute(pool).await.unwrap();
    }

    #[tokio::test]
    async fn wishlist_add_list_complete_delete() {
        let pool = test_pool().await; seed(&pool).await;
        assert_eq!(call(&pool, "POST", "/api/wishlist", Some(serde_json::json!({"item_id": 1}))).await.0, StatusCode::NO_CONTENT);
        assert_eq!(call(&pool, "POST", "/api/wishlist", Some(serde_json::json!({"item_id": 1}))).await.0, StatusCode::NO_CONTENT);
        let (s, v) = call(&pool, "GET", "/api/wishlist", None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v[0]["item"]["name"], "Saryn Prime");
        assert_eq!(v[0]["tree"]["children"][0]["children"][0]["needed"], 1200);
        assert_eq!(v[0]["progress_pct"], 0.0);
        call(&pool, "PUT", "/api/progress/2", Some(serde_json::json!({"obtained": true}))).await;
        assert_eq!(call(&pool, "GET", "/api/wishlist", None).await.1[0]["progress_pct"], 100.0);
        call(&pool, "PATCH", "/api/wishlist/1", Some(serde_json::json!({"completed": true}))).await;
        assert!(call(&pool, "GET", "/api/wishlist", None).await.1[0]["completed_at"].is_string());
        assert_eq!(call(&pool, "DELETE", "/api/wishlist/1", None).await.0, StatusCode::NO_CONTENT);
        assert_eq!(call(&pool, "GET", "/api/wishlist", None).await.1, serde_json::json!([]));
    }

    #[tokio::test]
    async fn unknown_item_writes_are_404() {
        let pool = test_pool().await;
        for (m, u, b) in [("POST", "/api/wishlist", serde_json::json!({"item_id": 999999})),
                          ("PUT", "/api/progress/999999", serde_json::json!({"obtained": true})),
                          ("PATCH", "/api/inventory/999999", serde_json::json!({"count": 3}))] {
            let (s, v) = call(&pool, m, u, Some(b)).await;
            assert_eq!(s, StatusCode::NOT_FOUND, "{m} {u}");
            assert_eq!(v["error"], "not found");
        }
    }

    #[tokio::test]
    async fn inventory_bulk_put_and_negative_rejected() {
        let pool = test_pool().await; seed(&pool).await;
        let req = Request::builder().method("PUT").uri("/api/inventory")
            .header("content-type", "application/json").header("x-inventory-source", "plugin")
            .body(Body::from(r#"[{"item_id":3,"count":500}]"#)).unwrap();
        let res = app(AppState { pool: pool.clone() }, "web/dist").oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::NO_CONTENT);
        let (_, v) = call(&pool, "GET", "/api/inventory", None).await;
        assert_eq!(v[0]["count"], 500); assert_eq!(v[0]["source"], "plugin");
        let (s, _) = call(&pool, "PATCH", "/api/inventory/3", Some(serde_json::json!({"count": -1}))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }
}
```

Catalog tests (in `catalog.rs`, using the same `call`/`seed` helpers copied over):
  - `GET /api/items?q=saryn` returns the Saryn Prime item.
  - `?category=resource` returns only Rubedo.
  - `GET /api/items/1` returns `tree.children[0].item.name == "Saryn Prime Chassis"`.
  - `GET /api/items/999` is 404.
  - `GET /api/meta` on an empty DB is 200 with `{"last_import_at": null, "item_count": 0, "sources": []}`.
  - Items with `removed_at` set don't appear in search.

- [ ] **Step 2: Run tests.** Expected: they fail (stub routers return 404 for everything).

- [ ] **Step 3: Implement `user.rs`**
  - **Wishlist GET:** select the user's wishlist rows ordered by `added_at`, call `tree::load_tree` per row, and build `{item: tree.item.clone(), added_at, completed_at, progress_pct: tree.progress_pct(), tree}`.
  - **POST:** `INSERT … ON CONFLICT DO NOTHING`. An FK violation becomes 404 through `From<sqlx::Error>`.
  - **DELETE/PATCH:** 404 if `rows_affected() == 0`.
  - **Progress PUT:** upsert `component_progress`.
  - **Inventory:**
    - Reject `count < 0` with `BadRequest("count must be >= 0")` before touching the DB.
    - Bulk PUT runs one transaction of upserts and sets `updated_at = now()`.
    - The `X-Inventory-Source` header must equal `plugin` to set `source='plugin'`; otherwise use `'manual'`.

- [ ] **Step 4: Implement `catalog.rs`**
  - **Search:** `WHERE removed_at IS NULL AND ($1 = '' OR wiki_name ILIKE '%' || $1 || '%') AND ($2 = '' OR category = $2) ORDER BY (lower(wiki_name) = lower($1)) DESC, length(wiki_name), wiki_name LIMIT $3`. Clamp `limit` to 1..=100.
  - **Detail:** `load_tree` (404 when `None`) plus `item_sources`.
  - **`item_sources`:** direct `drop_sources` rows, plus relic rows (`kind='relic'`, `name` = relic name, `rarity`, `vaulted` = the relic's `is_vaulted`) for any part in `relic_drops`, plus mission rows that drop those relics with `via` = the relic name.
  - **Meta:** `SELECT max(finished_at) FROM import_runs`, `count(*)` of items where `removed_at IS NULL`, and `wiki_sources` rows minus `source`.

- [ ] **Step 5: Run tests and lint, then commit**

```bash
mise run test && mise run lint
git add -A && git commit -m "feat: catalog, wishlist, progress, inventory and meta APIs"
```

---

### Task 5: Farm planner

**Files:**
- Modify: `src/plan.rs`

**Interfaces:**
- Consumes: `tree::{load_tree, ProgressNode}`, `models::*`, `api::{CurrentUser, ApiResult}`, `AppState`
- Produces:
  - the `GET /api/plan` route (JSON as in the contract)
  - `pub async fn build_plan(pool: &PgPool, user_id: i64) -> sqlx::Result<Plan>`
  - `#[derive(Serialize)] pub struct Plan { by_need: Vec<Need>, by_source: Vec<SourceGroup>, unsourced: Vec<Need> }`
  - A private `item_sources` (same contract `Source` shape; duplicated with Task 4 on purpose, and Task 8 merges the two)

**Algorithm:**
1. Fetch the wishlist rows where `completed_at IS NULL`, and call `load_tree` for each.
2. **`collect(node, root, sourced: &HashSet<i64>)`.** `sourced` holds the ids that appear in `relic_drops.part_item_id` or `drop_sources.item_id`. Apply in order:
   - If `node.obtained`, stop: a checked-off item hides its whole subtree.
   - If the node has children and `node.have >= node.needed`, stop: a component already in inventory.
   - If the node is a leaf, or `sourced` contains `node.item.id`, record a need: `needed += node.needed`, `for ∪= root`.
   - If the node has children, recurse into them.

   A prime part therefore needs both its relic drop and its crafting resources. Leaves ignore `node.covered`, because inventory is subtracted from the *total* in step 3. Comparing per node would undercount: with 1000 Rubedo in inventory and needs of 750 + 1200, the true remainder is 950.
3. Aggregate needs per `item.id`. Sum `needed` across every occurrence and root, set `have` = the inventory count once, and drop needs where `have >= needed`.
4. Attach `sources` per need. A need with no sources goes to `unsourced` instead of `by_need`.
5. **`by_source`:**
   - Group every `(kind, name, rotation)` across all needs' sources. `covers` holds `{item_id, name, chance, via}`, de-duplicated by `item_id`. `best_chance` is the max chance. `vaulted` is true when `kind = relic` and the relic is vaulted.
   - Sort: `vaulted` asc, `covers.len()` desc, `best_chance` desc (nulls last), name asc.

- [ ] **Step 1: Failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::build_plan;
    use crate::db::test_pool;

    // SP ← Chassis(1) ← Rubedo(1200); SP ← Neuroptics(1) ← Rubedo(750), Neurodes(2)
    // Neuroptics drops from relic Axi S1 (unvaulted), which drops from mission "Earth/Mantle" rot C.
    // Rubedo: planet Earth. Neurodes: planet Earth and mission "Lua/Tycho" rot A 10%.
    async fn seed(pool: &sqlx::PgPool) {
        sqlx::raw_sql(r#"
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
        "#).execute(pool).await.unwrap();
    }

    #[tokio::test]
    async fn empty_wishlist_is_empty_plan() {
        let pool = test_pool().await;
        let p = build_plan(&pool, 1).await.unwrap();
        assert!(p.by_need.is_empty() && p.by_source.is_empty() && p.unsourced.is_empty());
    }

    #[tokio::test]
    async fn totals_resources_and_ranks_multi_cover_first() {
        let pool = test_pool().await; seed(&pool).await;
        let p = build_plan(&pool, 1).await.unwrap();
        let need = |n: &str| p.by_need.iter().find(|x| x.item.name == n).unwrap_or_else(|| panic!("{n}"));
        assert_eq!(need("Rubedo").needed, 1950);
        assert_eq!(need("Neurodes").needed, 2);
        assert!(need("Saryn Prime Neuroptics").sources.iter().any(|s| s.kind == "mission" && s.via.as_deref() == Some("Axi S1")));
        assert_eq!(p.by_source[0].name, "Earth");          // covers Rubedo + Neurodes
        assert_eq!(p.by_source[0].covers.len(), 2);
    }

    #[tokio::test]
    async fn checked_component_removes_its_resources() {
        let pool = test_pool().await; seed(&pool).await;
        sqlx::raw_sql("INSERT INTO component_progress VALUES (1,2,true)").execute(&pool).await.unwrap();
        let p = build_plan(&pool, 1).await.unwrap();
        assert_eq!(p.by_need.iter().find(|x| x.item.name == "Rubedo").unwrap().needed, 750);
        assert!(p.by_need.iter().all(|x| x.item.name != "Saryn Prime Chassis"));
    }

    #[tokio::test]
    async fn inventory_subtracts_and_satisfied_needs_drop() {
        let pool = test_pool().await; seed(&pool).await;
        sqlx::raw_sql("INSERT INTO inventory (user_id,item_id,count) VALUES (1,4,2000),(1,5,1)").execute(&pool).await.unwrap();
        let p = build_plan(&pool, 1).await.unwrap();
        assert!(p.by_need.iter().all(|x| x.item.name != "Rubedo"));
        let n = p.by_need.iter().find(|x| x.item.name == "Neurodes").unwrap();
        assert_eq!((n.needed, n.have), (2, 1));
    }

    #[tokio::test]
    async fn no_source_goes_to_unsourced() {
        let pool = test_pool().await; seed(&pool).await;
        sqlx::raw_sql("INSERT INTO wishlist (user_id,item_id) VALUES (1,7)").execute(&pool).await.unwrap();
        let p = build_plan(&pool, 1).await.unwrap();
        assert!(p.unsourced.iter().any(|x| x.item.name == "Mystery Part"));
    }

    #[tokio::test]
    async fn vaulted_relic_groups_sort_last() {
        let pool = test_pool().await; seed(&pool).await;
        sqlx::raw_sql("UPDATE items SET is_vaulted = true WHERE id = 6").execute(&pool).await.unwrap();
        let p = build_plan(&pool, 1).await.unwrap();
        let last = p.by_source.last().unwrap();
        assert!(last.vaulted && last.name == "Axi S1");
    }
}
```

Also add one route test confirming that `GET /api/plan` on an empty DB returns 200 with `{"by_need":[],"by_source":[],"unsourced":[]}`.

- [ ] **Step 2: Run tests.** Expected: they fail to compile.
- [ ] **Step 3: Implement `build_plan` and the route** following the algorithm.
- [ ] **Step 4: Run tests and lint.** Expected: all pass.
- [ ] **Step 5: Commit** — `git add -A && git commit -m "feat: farm planner"`

---

### Task 6: React frontend

**Files:**
- Create: `web/**` (Vite react-ts), modelled on the contract above.

**Interfaces:**
- Consumes: the API Contract only. The dev server proxies `/api` to `http://localhost:3000`.
- Produces: `web/dist` from `npm run build`; `npm run typecheck` (`tsc --noEmit`).

- [ ] **Step 1: Scaffold**

```bash
npm create vite@latest web -- --template react-ts
cd web && npm install && npm install react-router-dom
```

In `vite.config.ts`, add `server: { proxy: { "/api": "http://localhost:3000" } }`. In `package.json` scripts, add `"typecheck": "tsc --noEmit -p tsconfig.app.json"`. Delete the Vite demo assets.

- [ ] **Step 2: `src/api.ts`.** Copy the contract types verbatim, plus one helper:

```ts
export async function api<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(`/api${path}`, {
    ...init,
    headers: init?.body ? { "content-type": "application/json", ...init?.headers } : init?.headers,
  });
  if (!res.ok) throw new Error((await res.json().catch(() => ({}))).error ?? res.statusText);
  return res.status === 204 ? (undefined as T) : res.json();
}
export function useApi<T>(path: string): { data: T | undefined; error: string | undefined; reload: () => void } { /* useState + useEffect + a reload counter */ }
```

- [ ] **Step 3: Layout + routes** (`App.tsx`)
  - Top nav: Wishlist (`/`), Plan (`/plan`), Inventory (`/inventory`).
  - Footer: "Data from the official Warframe Wiki (CC BY-SA)" with a link to the wiki, plus "last import: …" from `/api/meta`.
  - Banner when `item_count === 0`: "Game data not imported yet — run `warframe-tracker import`."

- [ ] **Step 4: Wishlist page**
  - **Search box:** debounced 250 ms, calls `GET /items?q=…&limit=10`.
    - The category filter is a `<select>` with all / warframe / weapon / mod / arcane.
    - Results show image (from `image_url`, `loading="lazy"`), name and category, plus an "Add" button that POSTs and then reloads.
  - **Each wishlist entry is a `<details>`:**
    - Summary: name, a progress bar from `progress_pct` (a `<progress>` element), a "Done" toggle (PATCH) and Remove (DELETE).
    - Body: the recursive `ProgressNode` tree.
      - Component nodes get a checkbox bound to `obtained` (PUT `/progress/:id`).
      - Resource leaves show `have / needed` with a number input that PATCHes `/inventory/:id` on blur.
      - Covered nodes are dimmed.

- [ ] **Step 5: Plan page**
  - Toggle between "What to run" (`by_source`) and "What I need" (`by_need`).
  - **`by_source` rows:**
    - `kind` badge; name, plus `rotation` if present; best chance as a %.
    - `covers` as chips, with `"via X"` for relic-routed drops.
    - Vaulted groups get a "Vaulted" badge and reduced opacity.
  - **`by_need` rows:** item, `have/needed`, the `for` names, and the sources listed in full.
  - **`unsourced` section:** "No farmable source on wiki", with wiki links.

- [ ] **Step 6: Inventory page.** A table from `GET /inventory` with editable counts (PATCH on blur), plus an "Add resource" search limited to `category=resource` that PATCHes count 0 to create the row.

- [ ] **Step 7: Styling** (`index.css`): plain CSS, a dark theme by default plus `prefers-color-scheme: light`. Layout must work at 375 px wide.

- [ ] **Step 8: Verify.** Run `npm run typecheck && npm run build`; both must exit 0. With the backend from `feat/v1` running (`mise run run`) **and** some data seeded (any rows inserted by hand, or a real import if one has run), load `npm run dev` and check every page renders without console errors. Use the Playwright MCP tools for this if they're available.

- [ ] **Step 9: Commit** — `git add web && git commit -m "feat: react frontend"`

---

### Task 7: Packaging, CI, homelab chart

**Files:**
- Create: `Dockerfile`, `.dockerignore`, `.github/workflows/ci.yml`, `.github/workflows/release.yml`, `README.md`
- Create in **`~/repos/homelab-flagops-templates`** on a new branch `add-warframe-tracker` (use `git worktree add` there if its checkout is dirty; **never push**): `apps/warframe-tracker/Chart.yaml`, `values.yaml`, `templates/pg.yaml`

**Interfaces:**
- Consumes: binary `warframe-tracker` with subcommands `serve` and `import`; `WEB_DIR`; `DATABASE_URL`; `web/` builds to `web/dist`.
- Produces: an image runnable as `warframe-tracker serve` (the default CMD) and as `warframe-tracker import`.

- [ ] **Step 1: Dockerfile**

```dockerfile
FROM node:25-bookworm-slim AS web
WORKDIR /web
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY web ./
RUN npm run build

FROM rust:1.94 AS build
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && touch src/lib.rs && cargo build --release && rm -rf src
COPY schema.sql ./
COPY src ./src
RUN touch src/main.rs src/lib.rs && cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /app/target/release/warframe-tracker /usr/local/bin/warframe-tracker
COPY --from=web /web/dist /app/web
ENV WEB_DIR=/app/web
EXPOSE 3000
ENTRYPOINT ["warframe-tracker"]
CMD ["serve"]
```

`ca-certificates` is needed because the importer makes HTTPS calls to the wiki. `.dockerignore`: `target`, `web/node_modules`, `web/dist`, `wiki-json`, `.git`.

- [ ] **Step 2: Workflows.** Copy `~/repos/self-hosted-zwilling-storage-tracker/.github/workflows/{ci,release}.yml`, replace the image name with `ghcr.io/graytonio/warframe-tracker`, and add a `web` job to CI: `actions/setup-node` (node 25, npm cache with `web/package-lock.json`), then `npm ci`, `npm run typecheck` and `npm run build` in `web/`. Read release.yml before editing; keep its tag/`Cargo.toml` version check.

- [ ] **Step 3: Verify the image locally**

```bash
docker build -t warframe-tracker:dev .
docker run --rm --network host -e DATABASE_URL=postgres://postgres:postgres@localhost:5433/warframe warframe-tracker:dev serve &
sleep 3 && curl -sf localhost:3000/api/meta && curl -sf localhost:3000/ | grep -q '<div id="root">'; kill %1
```

Expected: JSON from `/api/meta`, and the HTML shell from `/`. If `--network host` isn't usable on macOS Docker, use `host.docker.internal` in the URL with `-p 3000:3000`. Until Task 6 is merged, `web/` won't exist on your branch: create a minimal `web/package.json` + `index.html` placeholder **only inside a scratch copy** for the build check, and don't commit it.

- [ ] **Step 4: Homelab chart.** Copy `apps/zwilling-tracker/` as the template and read every file there first.
  - `Chart.yaml`: name `warframe-tracker`, the same bjw-s `app-template` dependency with `alias: warframe-tracker`.
  - `values.yaml`:
    - Controllers:
      - `main`: image `ghcr.io/graytonio/warframe-tracker` tag `0.1.0`, `args: ["serve"]`.
      - `import`: `type: cronjob`, `cronjob.schedule: "17 */6 * * *"`, `concurrencyPolicy: Forbid`, the same image, `args: ["import"]`.
    - Both controllers get the env `PGUSER`/`PGPASSWORD` from the secret `warframe.warframe-tracker-db.credentials.postgresql.acid.zalan.do` (the user `warframe` on cluster `warframe-tracker-db`), followed by `DATABASE_URL: postgres://$(PGUSER):$(PGPASSWORD)@warframe-tracker-db.warframe-tracker.svc.cluster.local:5432/warframe?sslmode=require`. `PGUSER`/`PGPASSWORD` must be listed **before** `DATABASE_URL`.
    - Service on port 3000 for `main` only.
    - Ingress host `warframe.[{ env "env_domain" }]` (unquoted scalar, per the FlagOps gotcha), using zwilling's annotations, `access: private` label and homepage annotations (`gethomepage.dev/name: Warframe Tracker`).
  - `templates/pg.yaml`: zwilling's `postgresql` CR with name `warframe-tracker-db`, `teamId` matching zwilling's convention, volume `2Gi`, PG 17, user `warframe: []`, database `warframe: warframe`.
  - Validate with `helm dependency build apps/warframe-tracker && helm template apps/warframe-tracker`. Before running that, substitute the `[{ … }]` FlagOps placeholders with literals in a scratch copy, since plain helm doesn't run FlagOps. Commit on the branch; **don't push**.

- [ ] **Step 5: README.** Cover what it is, `mise run db/run/import/test`, `cargo run -- dump wiki-json`, the web dev loop, and deploy notes. For the first import after deploy: `kubectl -n warframe-tracker create job --from=cronjob/<name> first-import`.

- [ ] **Step 6: Commit** — `git add -A && git commit -m "build: dockerfile, ci, release workflow, readme"`. In the final report, include the homelab branch name and commit hash.

---

### Task 8: Integrate, real-data run, smoke test, review (lead)

**Files:** merge commits only, plus fixes that come up.

- [ ] **Step 1: Merge** Task 3–7 branches into `feat/v1` one at a time, running `mise run test && mise run lint` after each merge.
- [ ] **Step 2: Deduplicate `item_sources`.** Keep Task 4's version in `catalog.rs` as `pub`, delete Task 5's copy, and import it in `plan.rs`. Re-run the tests.
- [ ] **Step 3: Real import:** run `mise run import`. Record the counts and the unresolved rate. Run it a second time; it must print `wiki unchanged`.
- [ ] **Step 4: Smoke test.** Run `mise run web && mise run run`, then in a browser (Playwright):
  - Search "Saryn Prime" and add it.
  - Expand it; check off Chassis.
  - On the Plan page, confirm Rubedo dropped by 1200, Neuroptics shows a relic route, and the first row covers more than one need.
  - Set an inventory count and confirm the plan updates.
- [ ] **Step 5: Final review.** Run a whole-branch review with the `superpowers:requesting-code-review` skill, fix what it finds, then present the branch for the user to merge or open a PR.
