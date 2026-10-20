# Inventory Sync Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship `wft-sync` and its server half:
- `wft-sync` is a small client binary that pushes WFHelper's `inventory.json` to the tracker.
- The server half adds part blueprint items ("blueprint owned"), API tokens, and a sync endpoint that maps game paths to items and writes an inventory snapshot.

**Architecture:**
- The repo becomes a Cargo workspace: root package `warframe-tracker` (the server, unchanged layout) plus member `sync/` (`wft-sync`, with blocking reqwest and no tokio, sqlx or mlua).
- All mapping logic lives on the server. The client only reads, flattens, and posts.
- Blueprint ownership is modelled as importer-created blueprint items, so the planner and UI need no change.

**Tech Stack:** Rust 1.94 (edition 2024), axum 0.8, sqlx 0.9, sha2, base64, rand; client uses reqwest 0.13 with the `blocking` feature, plus serde_json and anyhow.

**Spec:** `docs/superpowers/specs/2026-10-19-inventory-sync-design.md` (builds on `2026-10-19-warframe-tracker-design.md`)

## Global Constraints

- Everything in the v1 plan's Global Constraints still applies (`docs/superpowers/plans/2026-10-19-warframe-tracker.md`), especially:
  - `sqlx::AssertSqlSafe` for `format!` SQL, with only constants interpolated.
  - Tests use `db::test_pool()`.
  - Never run `docker compose down`.
  - Errors are JSON.
- The web UI keeps working with **no** `Authorization` header (it resolves to user 1).
- Only the token's SHA-256 is stored. The plaintext token is printed once and never logged by the server or the client.
- The sync endpoint writes one transaction per request, as a snapshot:
  - Upsert matched items with `source='plugin'`.
  - Delete `plugin` rows that are absent from the snapshot.
  - Never delete `manual` rows that are absent from it.
- Mapping only considers items with `removed_at IS NULL`.
- Request limits: at most 20,000 paths and counts `>= 0`; otherwise 400.
- Client: `sync/` must build for `x86_64-pc-windows-msvc` and `x86_64-unknown-linux-gnu`. It has no async runtime.
- `IMPORT_VERSION` goes from 2 to 3 (the blueprint-item mapping change).

## Review Focus

1. **Duplicate paths for one item.** Two game paths that map to the same item must be summed before the upsert. Otherwise `ON CONFLICT` on the same row twice in one statement errors. Owner: Task 3 test `duplicate_paths_sum`.
2. **Manual counts survive.** A resource the player typed in by hand, which the game doesn't report, must survive a sync. Owner: Task 3 test `snapshot_keeps_manual_absent_rows`.
3. **Mid-write file.** WFHelper replaces the file while it is being read. The client must log and retry, never exit or panic. Owner: Task 4 test `unparsable_file_is_an_error_not_a_panic`, plus the loop's behaviour.
4. **Re-import keeps progress.** After the blueprint-item import change, existing `component_progress` rows on parts still point at the same `item_id`s. Owner: Task 1 test `reimport_keeps_part_ids`.
5. **Token in logs.** The client must not print the token, even on a 401 or in its debug output of the request. Owner: Task 4 (review the code; there's no automated test).

## Execution Shape

- **Task 0 (lead):** workspace skeleton and dependencies, so workers never touch the root `Cargo.toml` or `Cargo.lock`.
- **Parallel worktrees after Task 0:**
  - **A:** Task 1, the importer.
  - **B:** Task 2, then Task 3 (tokens, then the sync endpoint).
  - **C:** Task 4, then Task 5 (client, then release, CI and docs).
- **Task 6 (lead):** merge, real import, deploy, end-to-end check.

---

### Task 0: Workspace skeleton (lead)

**Files:** `Cargo.toml`, `Cargo.lock`, `sync/Cargo.toml`, `sync/src/main.rs`, `mise.toml`, `Dockerfile`

- [ ] **Step 1.** Add to the root `Cargo.toml`:

```toml
[workspace]
members = ["sync"]
```

- [ ] **Step 2.** Run `cargo new sync --name wft-sync`. Then add its dependencies: `cargo add -p wft-sync anyhow serde_json` and `cargo add -p wft-sync reqwest --no-default-features --features rustls,json,blocking`.
- [ ] **Step 3.** Add server dependencies: `cargo add -p warframe-tracker sha2 base64 rand`.
- [ ] **Step 4.** In `mise.toml`, set the `lint` task to `cargo fmt --all --check` and `cargo clippy --workspace --all-targets -- -D warnings`, and the `test` task to `cargo test --workspace`.
- [ ] **Step 5.** In the `Dockerfile`, the build stage must copy `sync/Cargo.toml` and create a stub `sync/src/main.rs` before the dependency-cache build. Every `cargo build` becomes `cargo build --release -p warframe-tracker`.

```dockerfile
COPY Cargo.toml Cargo.lock ./
COPY sync/Cargo.toml sync/
RUN mkdir -p src sync/src && echo 'fn main() {}' > src/main.rs && touch src/lib.rs \
 && echo 'fn main() {}' > sync/src/main.rs && cargo build --release -p warframe-tracker && rm -rf src
COPY schema.sql ./
COPY src ./src
RUN touch src/main.rs src/lib.rs && cargo build --release -p warframe-tracker
```

- [ ] **Step 6.** Verify with `mise run lint`, `mise run test`, and `docker build -t wt:dev .`. All must pass. Then commit: `chore: cargo workspace with wft-sync member`.

---

### Task 1: Part blueprint items (importer)

**Files:** `src/import.rs` only.

**Interfaces:**
- **Produces** a component item named `"{R} Blueprint"` for every blueprint result `R`, including components. It is recorded as a recipe ingredient (count 1). Examples: "Saryn Prime Chassis" ← "Saryn Prime Chassis Blueprint", and "Forma" ← "Forma Blueprint" (which already existed).
- **Relic drops** link the blueprint item for any relic `Part` ending in `" Blueprint"`.
- **Drop tables:** a `Blueprint`/`Item` drop resolves exactly. The `strip_suffix(" Blueprint")` fallback that credited the built part is removed.

- [ ] **Step 1: Failing tests** (add to the `import.rs` tests, using the existing `fixtures()`, `import()`, `id()` and `ingredients()` helpers):

```rust
#[tokio::test]
async fn parts_have_their_own_blueprint_item() {
    let pool = test_pool().await;
    import(&pool).await;
    let chassis = id(&pool, "component", "Saryn Prime Chassis").await;
    let names = ingredients(&pool, chassis).await;
    assert!(names.contains(&"Saryn Prime Chassis Blueprint".to_string()), "{names:?}");
    // Prime weapon parts drop already built: no blueprint item.
    let barrel = id(&pool, "component", "Braton Prime Barrel").await;
    assert!(!ingredients(&pool, barrel).await.iter().any(|n| n.ends_with("Blueprint")));
}

#[tokio::test]
async fn relics_drop_the_part_blueprint() {
    let pool = test_pool().await;
    import(&pool).await;
    let parts: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT i.wiki_name FROM relic_drops rd JOIN items i ON i.id = rd.part_item_id
         WHERE i.wiki_name LIKE 'Saryn Prime%' ORDER BY 1")
        .fetch_all(&pool).await.unwrap();
    assert!(parts.contains(&"Saryn Prime Neuroptics Blueprint".to_string()), "{parts:?}");
    assert!(!parts.contains(&"Saryn Prime Neuroptics".to_string()), "{parts:?}");
}

#[tokio::test]
async fn reimport_keeps_part_ids() {
    let pool = test_pool().await;
    import(&pool).await;
    let before = id(&pool, "component", "Saryn Prime Chassis").await;
    sqlx::query("INSERT INTO component_progress (user_id, item_id, obtained) VALUES (1, $1, true)")
        .bind(before).execute(&pool).await.unwrap();
    import(&pool).await;
    assert_eq!(id(&pool, "component", "Saryn Prime Chassis").await, before);
}
```

Then fix the existing tests these changes break (`relic_parts_link_to_components`, `generic_parts_blueprint_drops_and_refined_relics`) to assert the new behaviour. Don't delete them.

- [ ] **Step 2.** Run `mise run test`. The new tests should fail.
- [ ] **Step 3: Implement.**
  - In the relic loop, the `p =>` arm becomes:
    ```rust
    p => Some(component(tx, &mut ids, &format!("{item} {p}"), item).await?),
    ```
    It no longer calls `strip_suffix`. `Part: "Chassis Blueprint"` now gives `"{item} Chassis Blueprint"`.
  - In the recipe loop, drop the `if rcat != "component"` guard, so every result gets `"{r} Blueprint"`.
  - Recipe part resolution must still give `"Saryn Prime Chassis"` for part `"Prime Chassis"`. `bps.contains_key(comp)` already covers this, because `relic_parts` now holds blueprint names.
  - In `drop_item`, remove the `.or_else(... strip_suffix(" Blueprint") ...)` fallback, and update its comment.
  - Bump `IMPORT_VERSION` to 3.
- [ ] **Step 4.** `mise run test` and `mise run lint` must both pass.
- [ ] **Step 5: Real data.** Run `mise run import` against the dev DB (it re-maps because of the version bump). Report three things:
  - the item, recipe and drop-source counts;
  - the unresolved count compared with the v1 baseline of 524;
  - `SELECT count(*) FROM items WHERE category='component' AND wiki_name LIKE '% Blueprint'`.

  Unresolved must not grow by more than ~5%. If it does, find the pattern, fix it, and add a test.
- [ ] **Step 6.** Commit: `feat(import): part blueprints are their own items`.

---

### Task 2: API tokens

**Files:** `schema.sql`, `src/tokens.rs` (new), `src/lib.rs` (`pub mod tokens;`), `src/api/mod.rs`, `src/main.rs`

**Interfaces:**
- **Produces:**
  - `tokens::create(pool, user_id: i64, name: &str) -> sqlx::Result<String>`, which returns the plaintext token.
  - `tokens::revoke(pool, name: &str) -> sqlx::Result<bool>`.
  - `tokens::authenticate(pool, token: &str) -> sqlx::Result<Option<i64>>`, which returns `user_id` and bumps `last_used_at`.
  - `api::TokenUser(pub i64)`, an extractor that requires a valid token (otherwise 401).
  - `CurrentUser` changes:
    - `Bearer` present and valid: the token's user.
    - `Bearer` present and invalid: 401.
    - Header absent: user 1.
  - `ApiError::Unauthorized` gives `401 {"error":"invalid token"}`.
  - CLI: `warframe-tracker token create --name N [--user ID]` prints the token to stdout. `warframe-tracker token revoke --name N` exits 0, or 1 if there's no such token.

- [ ] **Step 1: Schema.**

```sql
CREATE TABLE IF NOT EXISTS api_tokens (
  id bigserial PRIMARY KEY,
  user_id bigint NOT NULL REFERENCES users(id),
  name text NOT NULL UNIQUE,
  token_hash bytea NOT NULL UNIQUE,
  created_at timestamptz NOT NULL DEFAULT now(),
  last_used_at timestamptz,
  revoked_at timestamptz
);
```

- [ ] **Step 2: Failing tests** (in `src/tokens.rs`, plus a route-level test in `src/api/mod.rs`):

```rust
#[tokio::test]
async fn create_authenticate_revoke() {
    let pool = crate::db::test_pool().await;
    let t = super::create(&pool, 1, "pc").await.unwrap();
    assert!(t.len() >= 43); // 32 bytes base64url
    assert_eq!(super::authenticate(&pool, &t).await.unwrap(), Some(1));
    assert_eq!(super::authenticate(&pool, "garbage").await.unwrap(), None);
    assert!(super::revoke(&pool, "pc").await.unwrap());
    assert_eq!(super::authenticate(&pool, &t).await.unwrap(), None);
    let stored: Vec<u8> = sqlx::query_scalar("SELECT token_hash FROM api_tokens").fetch_one(&pool).await.unwrap();
    assert_ne!(stored, t.as_bytes()); // only the hash is stored
}
```

Route-level test: `GET /api/wishlist` with no header returns 200. With `Authorization: Bearer nope` it returns 401 and a JSON body. With a valid token it returns 200.

- [ ] **Step 3: Implement `tokens.rs`.**

```rust
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use sqlx::PgPool;

fn hash(token: &str) -> Vec<u8> { Sha256::digest(token.as_bytes()).to_vec() }

pub async fn create(pool: &PgPool, user_id: i64, name: &str) -> sqlx::Result<String> {
    let token = URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>());
    sqlx::query("INSERT INTO api_tokens (user_id, name, token_hash) VALUES ($1, $2, $3)")
        .bind(user_id).bind(name).bind(hash(&token)).execute(pool).await?;
    Ok(token)
}

pub async fn authenticate(pool: &PgPool, token: &str) -> sqlx::Result<Option<i64>> {
    sqlx::query_scalar(
        "UPDATE api_tokens SET last_used_at = now()
         WHERE token_hash = $1 AND revoked_at IS NULL RETURNING user_id")
        .bind(hash(token)).fetch_optional(pool).await
}

pub async fn revoke(pool: &PgPool, name: &str) -> sqlx::Result<bool> {
    let r = sqlx::query("UPDATE api_tokens SET revoked_at = now() WHERE name = $1 AND revoked_at IS NULL")
        .bind(name).execute(pool).await?;
    Ok(r.rows_affected() > 0)
}
```

- [ ] **Step 4: Extractors.** Both `CurrentUser` and `TokenUser` must implement `FromRequestParts<AppState>`, not a generic `S`, because they need the pool. The existing generic `impl<S>` becomes `impl FromRequestParts<AppState>`. Read the bearer token from `parts.headers` with `header::AUTHORIZATION`, stripping the `"Bearer "` prefix.
- [ ] **Step 5: CLI.** In `main.rs`, `"token"` dispatches on `args[2]` (`create` / `revoke`). It parses `--name` and `--user` with a small hand-written loop over the args (no clap) and connects with `connect_with_retry` and `migrate`.
- [ ] **Step 6.** `mise run test` and `mise run lint`, then commit: `feat: api tokens and bearer auth`.

---

### Task 3: `POST /api/sync/inventory`

**Files:** `src/api/sync.rs` (new), `src/api/mod.rs` (`pub mod sync;` and `.merge(sync::routes())`), `schema.sql` (index)

**Interfaces:**
- **Consumes:** `api::TokenUser` from Task 2.
- **Request:** `{"items": {"<path>": <count>}}`.
- **Response 200:** `{"matched": n, "updated": n, "unmatched": [..≤200]}`.

- [ ] **Step 1: Index.**
  `CREATE INDEX IF NOT EXISTS items_internal_name ON items ((data->>'InternalName'));`
- [ ] **Step 2: Failing tests** (route-level, with a token from `tokens::create`):

```rust
// seed: (1,'Neurodes','resource', data {"InternalName":"/Lotus/Types/Items/MiscItems/Neurode"}),
//       (2,'Saryn Prime','warframe', data {"InternalName":"/Lotus/Powersuits/Saryn/SarynPrime"}),
//       (3,'Saryn Prime Chassis','component'), (4,'Saryn Prime Chassis Blueprint','component'),
//       (5,'Saryn Prime Blueprint','component'), (6,'Rubedo','resource', no InternalName)
#[tokio::test] async fn maps_exact_and_component_paths() {
  // POST {"/Lotus/Types/Items/MiscItems/Neurode": 40,
  //       "/Lotus/Types/Recipes/WarframeRecipes/SarynPrimeChassisComponent": 1,
  //       "/Lotus/Types/Recipes/WarframeRecipes/SarynPrimeChassisBlueprint": 1,
  //       "/Lotus/Types/Recipes/WarframeRecipes/SarynPrimeBlueprint": 1,
  //       "/Lotus/Powersuits/Saryn/SarynPrime": 1,
  //       "/Lotus/Unknown/Thing": 3}
  // → matched 5, unmatched ["/Lotus/Unknown/Thing"]; GET /api/inventory shows 5 rows, all source "plugin"
}
#[tokio::test] async fn duplicate_paths_sum() { /* two paths → same item: count is the sum */ }
#[tokio::test] async fn snapshot_removes_absent_plugin_rows() { /* sync {A:1,B:1}, then {A:2} → B row gone, A = 2 */ }
#[tokio::test] async fn snapshot_keeps_manual_absent_rows() { /* PATCH Rubedo manual 500, sync {Neurode:1} → Rubedo 500 manual still there */ }
#[tokio::test] async fn manual_row_in_snapshot_becomes_plugin() { /* PATCH Neurodes manual 5, sync {Neurode:40} → count 40, source plugin */ }
#[tokio::test] async fn requires_token_and_validates() { /* no header → 401; negative count → 400; 20_001 paths → 400 */ }
#[tokio::test] async fn owned_frame_leaves_the_plan() { /* wishlist Saryn Prime, sync it → build_plan by_need excludes it and its parts */ }
```

Write each one out in full in the worker's step style, using the `call` helper pattern from `src/api/user.rs` tests plus an `Authorization` header.

- [ ] **Step 3: Implement.**

```rust
/// "/Lotus/.../SarynPrimeChassisComponent" -> "saryn prime chassis"
fn component_name(path: &str) -> String {
    let seg = path.rsplit('/').next().unwrap_or(path);
    let seg = seg.strip_suffix("Component").unwrap_or(seg);
    let mut out = String::new();
    for (i, c) in seg.char_indices() {
        if i > 0 && c.is_uppercase() { out.push(' '); }
        out.extend(c.to_lowercase());
    }
    out
}
```

Flow:
1. Validate the request.
2. Run one query for exact matches: `SELECT data->>'InternalName', min(id) … = ANY($1) GROUP BY 1`.
3. For the paths left over, run one query matching component names: `lower(wiki_name) = ANY($1) AND category='component'`.
4. Sum the counts per `item_id`.
5. In a transaction, upsert with `UNNEST`. `updated` = `rows_affected` of an `ON CONFLICT … DO UPDATE … WHERE inventory.count IS DISTINCT FROM EXCLUDED.count OR inventory.source <> 'plugin'`.
6. Still in the transaction: `DELETE FROM inventory WHERE user_id=$1 AND source='plugin' AND item_id <> ALL($2)`.
7. Commit.

Add a unit test for `component_name` covering `SarynPrimeChassisComponent`, `BratonPrimeBarrel` and `SarynPrimeBlueprint`.
- [ ] **Step 4.** `mise run test` and `mise run lint`, then commit: `feat: inventory sync endpoint`.

---

### Task 4: `wft-sync` client

**Files:** `sync/src/main.rs`, `sync/src/inventory.rs`, `sync/tests/fixtures/inventory.json`

**Interfaces:**
- **Consumes:** `POST {url}/api/sync/inventory` with `Authorization: Bearer {token}`, body `{"items": {...}}`.
- **Produces:**
  - `inventory::flatten(&serde_json::Value) -> BTreeMap<String, i64>`
  - `inventory::sync_id(&Value) -> Option<String>`, which reads `LastInventorySync.$oid`
  - `inventory::read(path) -> anyhow::Result<Value>`

- [ ] **Step 1: Fixture.** Hand-build `sync/tests/fixtures/inventory.json` from WFHelper's documented shape, with no real account data:

```json
{
  "LastInventorySync": { "$oid": "6700000000000000000000aa" },
  "Suits": [ { "ItemType": "/Lotus/Powersuits/Saryn/SarynPrime" }, { "ItemType": "/Lotus/Powersuits/Saryn/SarynPrime" } ],
  "LongGuns": [ { "ItemType": "/Lotus/Weapons/Tenno/Rifle/BratonPrime" } ],
  "MiscItems": [
    { "ItemType": "/Lotus/Types/Items/MiscItems/Neurode", "ItemCount": 40 },
    { "ItemType": "/Lotus/Types/Items/MiscItems/Neurode", "ItemCount": 2 },
    { "ItemType": "/Lotus/Types/Recipes/WarframeRecipes/SarynPrimeChassisComponent", "ItemCount": 1 }
  ],
  "Recipes": [ { "ItemType": "/Lotus/Types/Recipes/WarframeRecipes/SarynPrimeChassisBlueprint", "ItemCount": 1 } ],
  "PlayerLevel": 30,
  "Unrelated": [ { "NoItemType": true } ]
}
```

- [ ] **Step 2: Failing tests** (`#[cfg(test)]` in `inventory.rs`):

```rust
#[test] fn flattens_counts_copies_and_ignores_noise() {
    let v = read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/inventory.json")).unwrap();
    let m = flatten(&v);
    assert_eq!(m["/Lotus/Types/Items/MiscItems/Neurode"], 42);
    assert_eq!(m["/Lotus/Powersuits/Saryn/SarynPrime"], 2);
    assert_eq!(m["/Lotus/Weapons/Tenno/Rifle/BratonPrime"], 1);
    assert_eq!(m.len(), 5);
    assert_eq!(sync_id(&v).as_deref(), Some("6700000000000000000000aa"));
}
#[test] fn unparsable_file_is_an_error_not_a_panic() {
    let dir = std::env::temp_dir().join(format!("wft-sync-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("inventory.json");
    std::fs::write(&f, "{\"Suits\": [").unwrap(); // truncated mid-write
    assert!(read(&f).is_err());
}
```

- [ ] **Step 3: Implement `inventory.rs`.**

```rust
pub fn flatten(v: &Value) -> BTreeMap<String, i64> {
    let mut m = BTreeMap::new();
    for arr in v.as_object().into_iter().flat_map(|o| o.values()).filter_map(Value::as_array) {
        for e in arr {
            if let Some(t) = e["ItemType"].as_str() {
                *m.entry(t.to_string()).or_default() += e["ItemCount"].as_i64().unwrap_or(1);
            }
        }
    }
    m
}
```

- [ ] **Step 4: Implement `main.rs`.**
  - **Args:** hand-parse `--url`, `--token`, `--file`, `--interval <secs>` and `--once`. Each falls back to its env var (`WFT_URL`, `WFT_TOKEN`, `WFT_FILE`, `WFT_INTERVAL`, default 60).
  - **Default file:** WFHelper's own location, verified from its source (no `dirs` crate). The base folder is the first of:
    1. `$WFHELPER_USER_DATA`, if set. WFHelper honours this override, so the client does too.
    2. Windows: `%APPDATA%\WFHelper`.
    3. Otherwise: `$XDG_CONFIG_HOME/WFHelper`, falling back to `~/.config/WFHelper`.

    The file is `api-helper/inventory.json` inside that folder.
    - Source: WFHelper `config/runtime/appIdentity.ts`, which sets `userData` to `app.getPath("appData")` + `"WFHelper"`, and `services/apiHelperRunner.ts`, which writes `inventory.json` under `userData/api-helper`.
    - Test it with a pure `fn default_file(os: &str, env: impl Fn(&str) -> Option<String>) -> Option<PathBuf>`, with one case per OS plus one for the override. If the file doesn't exist, the client logs that path, so it's obvious what to pass with `--file`.
  - **Loop:** follow the spec's Loop section: mtime gate, `sync_id` gate, then POST with `reqwest::blocking::Client` and a 30 s timeout.
  - **401:** print `"token rejected (401); create a new one with \`warframe-tracker token create\`"` and exit 2.
  - **Other errors:** `eprintln!` them and continue.
  - **Logs:** one summary line per push.
  - **Token safety:** never format the token or the full request.
- [ ] **Step 5.** `mise run test`, `mise run lint`, then `cargo build -p wft-sync --release`. Commit: `feat: wft-sync client`.

---

### Task 5: Release binaries, CI, docs

**Files:** `.github/workflows/release.yml`, `.github/workflows/ci.yml` (only if needed), `README.md`

- [ ] **Step 1: Release job.** Add a job to `release.yml`:

```yaml
  sync-binaries:
    needs: github-release
    strategy:
      matrix:
        include:
          - { os: ubuntu-latest, target: x86_64-unknown-linux-gnu, bin: wft-sync, asset: wft-sync-linux-x86_64 }
          - { os: windows-latest, target: x86_64-pc-windows-msvc, bin: wft-sync.exe, asset: wft-sync-windows-x86_64.exe }
    runs-on: ${{ matrix.os }}
    permissions:
      contents: write
    steps:
      - uses: actions/checkout@v7
      - uses: Swatinem/rust-cache@v2
        with:
          save-if: false
      - run: cargo build --release -p wft-sync --target ${{ matrix.target }}
      - shell: bash
        env:
          GH_TOKEN: ${{ github.token }}
        run: |
          cp target/${{ matrix.target }}/release/${{ matrix.bin }} ${{ matrix.asset }}
          gh release upload "$GITHUB_REF_NAME" ${{ matrix.asset }} --repo "$GITHUB_REPOSITORY"
```

  Use the toolchain from `rust-toolchain`/mise if the runner's default isn't 1.94. Copy the approach the test job already uses (`jdx/mise-action`).
- [ ] **Step 2: CI.** The `test` job already runs `mise run lint` and `mise run test`, which now cover the workspace. Add a `sync-windows` CI job that runs `cargo build -p wft-sync` on `windows-latest`, so a Windows break shows up before release.
- [ ] **Step 3: README.** Add a "Syncing inventory from the game" section covering:
  - what it reads, and that it requires WFHelper to be running;
  - `token create` on the cluster: `kubectl -n warframe-tracker exec deploy/warframe-tracker-production-main -- warframe-tracker token create --name gaming-pc`;
  - running `wft-sync` with env vars;
  - a Windows Task Scheduler one-liner (`schtasks /create /sc onlogon /tn wft-sync /tr "..."`);
  - a Linux systemd user unit;
  - revoking a token.
- [ ] **Step 4.** Commit: `build: release wft-sync binaries; docs`.

---

### Task 6: Integrate and release (lead)

- [ ] **Step 1: Merge.** Merge A, B and C into `feat/inventory-sync`, running `mise run test` and `mise run lint` after each.
- [ ] **Step 2: Smoke test against the dev DB:**
  - `cargo run -- token create --name dev`
  - `WFT_URL=http://localhost:3000 WFT_TOKEN=… WFT_FILE=sync/tests/fixtures/inventory.json cargo run -p wft-sync -- --once`

  Expect a summary line. `GET /api/inventory` should show the plugin rows.
- [ ] **Step 3: Plan check.** Wishlist Saryn Prime with only the Chassis Blueprint synced: the plan still lists the Chassis resources and drops the Chassis Blueprint relic need.
- [ ] **Step 4: Release.**
  1. Open the PR and get CI green.
  2. Merge, bump `Cargo.toml` to `0.2.0`, and tag `v0.2.0`.
  3. Bump the image tag in the homelab `values.yaml` (`0.1.0` → `0.2.0`) with a PR.
  4. After the rollout, run `token create` on the cluster.
  5. Hand the token and the release binary link to the user.
- [ ] **Step 5: Real file.** Ask the user to run `wft-sync --once` against their real WFHelper file and share the `unmatched` count/sample, to tune the mapping.
