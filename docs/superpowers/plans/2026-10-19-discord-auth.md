# Discord Auth Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Sign in with Discord, using server-side sessions. The app becomes multi-user: a public read-only catalog, every personal route behind auth, self-service sync tokens, and account deletion.

**Architecture:**
- **Config:** a `Config` struct is loaded once from env and carried in `AppState`.
- **New modules:**
  - `src/auth.rs`: the OAuth routes, session cookies, and dev login.
  - `src/sessions.rs`: session create, lookup, touch and delete, all hashed the same way as `tokens.rs`.
- **Extractors in `src/api/mod.rs`:** `CurrentUser` and `OptionalUser` resolve bearer token, then session cookie, then `401`/`None`. `TokenUser` is removed. The CSRF `Origin` check lives inside `CurrentUser` for non-GET session-authenticated requests.
- **Web:** gains an auth context, a landing page, an item page, and a Settings page.

**Tech Stack:** existing crates, plus `axum-extra` (the `cookie` feature) for cookie parsing and building. `reqwest` (rustls, json, query) is already a server dependency, and the `form` feature is added for the token exchange. No OAuth library: the flow is about 60 lines and the spec pins every parameter.

**Spec:** `docs/superpowers/specs/2026-10-19-discord-auth-design.md`. Approved; it is authoritative on behaviour.

## Global Constraints

- Everything in earlier plans' Global Constraints still applies: wrap built SQL in `sqlx::AssertSqlSafe`, tests use `db::test_pool()`, never run `docker compose down`, errors are JSON.
- Secrets never reach logs or the DB: Discord client secret, Discord access and refresh tokens, session cookie values, API tokens. Only SHA-256 hashes of session and API tokens are stored.
- Cookies:
  - `wft_session`: `HttpOnly; SameSite=Lax; Path=/; Max-Age=2592000`.
  - `wft_oauth`: `HttpOnly; SameSite=Lax; Path=/auth; Max-Age=600`.
  - Both add `Secure` unless `PUBLIC_URL` starts with `http://localhost`.
- `return_to` must start with `/` and not `//`; anything else becomes `/`.
- Dev login works only when `DEV_LOGIN=1` **and** `PUBLIC_URL` is a localhost URL. In every other configuration the route returns 404.
- Session-authenticated writes (any method other than GET/HEAD) need `Origin` equal to `PUBLIC_URL`'s origin, otherwise `403 {"error":"cross-site request"}`. Bearer-authenticated writes skip the check.
- Limits: 300 wishlist items, 10 unrevoked tokens per user, 20,000 sync paths (existing). Each returns `400` with a message.
- `serve` fails at startup when `PUBLIC_URL` is missing, or when the Discord client id/secret are missing and dev login isn't enabled. `import` and `token` need none of these.

## Review Focus

1. **Existing tests silently relying on the user-1 fallback.** Every API test helper must authenticate explicitly, with a session cookie or bearer for user 1. A test that "passes" because it hit a 401 path it never asserted on is a bug. Owner: Task 2. Every `call` helper gets an auth parameter or default, and `unauthenticated_requests_are_401` covers each protected route.
2. **Owner claim race and idempotency.** A second login by the owner must not create a second user or re-claim anything. A non-owner must never claim user 1. Owner: Task 3 tests `owner_claims_user_1_once` and `non_owner_never_claims`.
3. **Cascade blast radius.** Deleting user B must not touch user A's rows. Game tables have no `user_id` and must be untouched. Owner: Task 4 test `delete_account_only_removes_own_rows`.
4. **Open redirect through `return_to`.** `https://evil`, `//evil`, `/\evil` and `javascript:` all become `/`. Owner: Task 3 test `return_to_is_sanitised`.
5. **SPA 401 loop.** A signed-out user on a protected page must land on `/` exactly once, with no redirect loop between `/api/me` and the landing page. Owner: Task 5 (manual click-through, plus code review of the 401 handler).

## Execution Shape

One agent, in one worktree, Tasks 1→6 in order, one commit per task. Task 7, the homelab chart, is the lead's job after you provide the secret name and Discord user ID.

---

### Task 1: Config, schema, sessions

**Files:**
- `src/config.rs` (new)
- `src/sessions.rs` (new)
- `src/lib.rs`
- `schema.sql`
- `src/main.rs`
- `src/api/sync.rs`
- Every test that builds `AppState` (mechanical change)

**Interfaces (produces):**

```rust
// config.rs
#[derive(Clone, Debug)]
pub struct Config {
    pub public_url: String,                 // no trailing slash
    pub discord_client_id: Option<String>,
    pub discord_client_secret: Option<String>,
    pub discord_api_base: String,           // default "https://discord.com/api"
    pub owner_discord_id: Option<String>,
    pub dev_login: bool,                    // DEV_LOGIN=1 && public_url is localhost
}
impl Config {
    pub fn from_env() -> anyhow::Result<Self>;   // enforces the startup rules
    pub fn for_tests() -> Self;                  // public_url "http://localhost:3000", dev_login true, discord None
    pub fn secure_cookies(&self) -> bool;        // !public_url.starts_with("http://localhost")
    pub fn origin(&self) -> &str;                // scheme://host[:port] of public_url
}

// lib.rs
#[derive(Clone)] pub struct AppState { pub pool: PgPool, pub config: std::sync::Arc<Config> }
impl AppState { pub fn new(pool: PgPool, config: Config) -> Self; }

// sessions.rs  (same hashing as tokens.rs; reuse its hash fn by making it pub(crate))
pub const TTL_DAYS: i64 = 30;
pub async fn create(pool: &PgPool, user_id: i64) -> sqlx::Result<String>;           // returns cookie value
pub async fn lookup(pool: &PgPool, cookie: &str) -> sqlx::Result<Option<i64>>;      // unexpired only; touches if last_seen_at > 1h old
pub async fn delete(pool: &PgPool, cookie: &str) -> sqlx::Result<()>;
pub async fn purge_expired(pool: &PgPool) -> sqlx::Result<u64>;
```

- [ ] **Step 1: Schema** (`schema.sql`, idempotent):

```sql
ALTER TABLE users ADD COLUMN IF NOT EXISTS discord_id text UNIQUE;
ALTER TABLE users ADD COLUMN IF NOT EXISTS display_name text;
ALTER TABLE users ADD COLUMN IF NOT EXISTS avatar text;
ALTER TABLE users ADD COLUMN IF NOT EXISTS last_login_at timestamptz;

CREATE TABLE IF NOT EXISTS sessions (
  id bigserial PRIMARY KEY,
  user_id bigint NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  token_hash bytea NOT NULL UNIQUE,
  created_at timestamptz NOT NULL DEFAULT now(),
  last_seen_at timestamptz NOT NULL DEFAULT now(),
  expires_at timestamptz NOT NULL
);

-- Token names are per user (spec). Old global unique → per-user unique.
ALTER TABLE api_tokens DROP CONSTRAINT IF EXISTS api_tokens_name_key;
CREATE UNIQUE INDEX IF NOT EXISTS api_tokens_user_name ON api_tokens (user_id, name);

-- ON DELETE CASCADE for user-owned tables (re-created idempotently).
DO $$
DECLARE t text;
BEGIN
  FOREACH t IN ARRAY ARRAY['wishlist','component_progress','inventory','api_tokens'] LOOP
    EXECUTE format('ALTER TABLE %I DROP CONSTRAINT IF EXISTS %I', t, t || '_user_id_fkey');
    EXECUTE format('ALTER TABLE %I ADD CONSTRAINT %I FOREIGN KEY (user_id) REFERENCES users(id) ON DELETE CASCADE', t, t || '_user_id_fkey');
  END LOOP;
END $$;
```

  The drop-and-re-add runs on every boot and is cheap at this size, safe under the existing advisory lock. Add a `ponytail:` note: guard it with a `pg_constraint.confdeltype` check if boot time ever matters.

- [ ] **Step 2: Failing tests.**

  `sessions.rs`:
  - create → lookup returns the user.
  - A garbage cookie returns `None`.
  - Setting `expires_at` in the past makes lookup return `None`.
  - Touch: with `last_seen_at` backdated 2h, lookup bumps `expires_at`. With it backdated 10 min, lookup doesn't.
  - delete → lookup returns `None`.
  - `purge_expired` removes only expired rows.
  - The stored hash isn't the cookie value.

  `config.rs` (pure functions, by injecting an env lookup closure: `from_env_with(|k| ...)`):
  - Missing `PUBLIC_URL` is an error.
  - Discord client id/secret missing and no dev login is an error.
  - `DEV_LOGIN=1` with a non-localhost `PUBLIC_URL` is an error ("dev login requires a localhost PUBLIC_URL").
  - `origin()` of `https://warframe.graytonward.com/` is `https://warframe.graytonward.com`.

- [ ] **Step 3: Implement.**
  - Wire `Config::from_env()` into `main.rs` `serve` only.
  - Mechanically replace `AppState { pool: X }` with `AppState::new(X, Config::for_tests())` in all tests.

- [ ] **Step 4.** Run `mise run test && mise run lint`. All existing tests still pass, because the fallback is untouched until Task 2. Commit: `feat(auth): config, schema, sessions`.

---

### Task 2: Extractors, CSRF, and authenticating every existing test

**Files:**
- `src/api/mod.rs`
- `src/api/user.rs`
- `src/api/catalog.rs`
- `src/api/sync.rs`
- `src/plan.rs` (route only)
- `src/tree.rs` (anonymous support)
- All API test helpers

**Interfaces:**
- `CurrentUser(pub i64)` checks bearer, then the `wft_session` cookie, then returns `ApiError::Unauthorized("sign in required")`.
- `OptionalUser(pub Option<i64>)` is the same, but with no auth it gives `None`. A present-but-invalid bearer is still `401`.
- `TokenUser` is removed; `sync.rs` uses `CurrentUser`.
- `ApiError::Unauthorized(String)` and `ApiError::Forbidden(String)` become JSON 401 and 403. Existing `invalid token` messages stay.
- `tree::load_tree(pool, user_id: Option<i64>, root)`: `None` means no progress or inventory joins, so everything is `have=0`, `obtained=false`.
- **CSRF:** in `CurrentUser`, when the user came from the **session cookie** and the method isn't GET/HEAD, require `parts.headers[ORIGIN] == config.origin()`. Otherwise `Forbidden("cross-site request")`.

- [ ] **Step 1: A shared test helper,** so each file stops hand-rolling auth. Add to `src/api/mod.rs`:

```rust
#[cfg(test)]
pub mod test_support {
    use crate::{AppState, app, config::Config};
    use axum::{body::Body, http::{Request, StatusCode}};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    pub enum Auth<'a> { None, Session(&'a str), Bearer(&'a str) }

    /// Session cookie for `user_id` (creating the user row if needed).
    pub async fn login(pool: &sqlx::PgPool, user_id: i64) -> String {
        sqlx::query("INSERT INTO users (id, name) VALUES ($1, 'test') ON CONFLICT DO NOTHING")
            .bind(user_id).execute(pool).await.unwrap();
        crate::sessions::create(pool, user_id).await.unwrap()
    }

    pub async fn call(pool: &sqlx::PgPool, auth: Auth<'_>, method: &str, uri: &str,
                      body: Option<serde_json::Value>) -> (StatusCode, serde_json::Value) {
        let mut req = Request::builder().method(method).uri(uri)
            .header("origin", "http://localhost:3000");
        match auth {
            Auth::None => {}
            Auth::Session(c) => req = req.header("cookie", format!("wft_session={c}")),
            Auth::Bearer(t) => req = req.header("authorization", format!("Bearer {t}")),
        }
        let body = match body {
            Some(b) => { req = req.header("content-type", "application/json"); Body::from(b.to_string()) }
            None => Body::empty(),
        };
        let res = app(AppState::new(pool.clone(), Config::for_tests()), "web/dist")
            .oneshot(req.body(body).unwrap()).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, if bytes.is_empty() { serde_json::Value::Null } else { serde_json::from_slice(&bytes).unwrap() })
    }
}
```

  Port every existing API test (`user.rs`, `catalog.rs`, `sync.rs`, `plan.rs` route tests, `mod.rs` JSON-error tests) to `test_support::call` with `Auth::Session(&login(&pool, 1).await)`. Keep each test's assertions unchanged. Delete the per-file `call` copies.

- [ ] **Step 2: Failing tests** (`src/api/mod.rs`):

```rust
#[tokio::test] async fn unauthenticated_requests_are_401() {
    // For each of: GET /api/wishlist, POST /api/wishlist, PUT /api/progress/1, GET /api/inventory,
    // PUT /api/inventory, PATCH /api/inventory/1, GET /api/plan, POST /api/sync/inventory,
    // GET /api/tokens, GET /api/me  → 401 {"error": "sign in required"}
}
#[tokio::test] async fn catalog_is_public() {
    // GET /api/items?q=… → 200; GET /api/items/{id} → 200 with tree.have == 0 even if user 1 has inventory; GET /api/meta → 200
}
#[tokio::test] async fn session_writes_need_same_origin() {
    // POST /api/wishlist with session cookie + Origin https://evil → 403 {"error":"cross-site request"}
    // same with no Origin header → 403
    // same with bearer token + Origin https://evil → 204
}
#[tokio::test] async fn expired_session_is_401() { /* backdate expires_at → GET /api/wishlist 401 */ }
```

  Write them in full.

- [ ] **Step 3: Implement.** Parse cookies with `axum_extra::extract::cookie::CookieJar::from_headers(&parts.headers)`. Add the dependency with `cargo add axum-extra --features cookie`.
- [ ] **Step 4.** Run `mise run test && mise run lint`. Commit: `feat(auth): session/bearer extractors, CSRF origin check, public catalog`.

---

### Task 3: Discord OAuth routes and dev login

**Files:** `src/auth.rs` (new), `src/lib.rs` (`.nest("/auth", auth::router())`), `Cargo.toml` (`reqwest` `form` feature).

**Interfaces:**
- `auth::router() -> Router<AppState>`:
  - `GET /discord/login`
  - `GET /discord/callback`
  - `POST /logout`
  - `GET /dev-login`
- `fn safe_return_to(s: Option<&str>) -> String`
- `async fn upsert_discord_user(pool, owner: Option<&str>, discord_id, display_name, avatar) -> sqlx::Result<i64>`, which includes the owner claim:

```sql
-- in a transaction:
-- 1. existing user with this discord_id → update name/avatar/last_login_at, return id
-- 2. else if discord_id = OWNER_DISCORD_ID and user 1 has discord_id IS NULL →
--      UPDATE users SET discord_id=$1, display_name=$2, avatar=$3, last_login_at=now() WHERE id=1 AND discord_id IS NULL RETURNING id
-- 3. else INSERT INTO users (name, discord_id, display_name, avatar, last_login_at) VALUES ($2,$1,$2,$3,now()) RETURNING id
```

- [ ] **Step 1: Failing tests** (`src/auth.rs`). Stub Discord with an axum server on `127.0.0.1:0`:
  - `POST /oauth2/token` returns `{"access_token":"discord-secret-at","token_type":"Bearer"}`, asserting the form has `grant_type=authorization_code`, the `code`, and the `redirect_uri`.
  - `GET /users/@me` returns `{"id":"111","username":"tenno","global_name":"Tenno","avatar":"abc"}`.

  Point `Config.discord_api_base` at it, with `discord_client_id`/`discord_client_secret` set.

```rust
#[tokio::test] async fn login_redirects_to_discord_with_state_cookie() { /* 303 to …/oauth2/authorize?…scope=identify…state=…; Set-Cookie wft_oauth HttpOnly, Path=/auth */ }
#[tokio::test] async fn callback_rejects_state_mismatch() { /* 400 */ }
#[tokio::test] async fn callback_happy_path() {
  // login?return_to=/plan → take state from Location + cookie → callback?code=c&state=… with the cookie
  // → 303 Location /plan; Set-Cookie wft_session (HttpOnly, SameSite=Lax); users row discord_id '111', display_name 'Tenno', avatar 'abc'
  // → GET /api/me with that cookie → {"id":…, "display_name":"Tenno", "avatar_url":"https://cdn.discordapp.com/avatars/111/abc.png?size=64"}
  // → no row anywhere contains "discord-secret-at" (check sessions/users/api_tokens text casts)
}
#[tokio::test] async fn callback_access_denied_goes_home() { /* ?error=access_denied&state=… → 303 /?login=cancelled */ }
#[tokio::test] async fn same_discord_id_same_user_updates_profile() { }
#[tokio::test] async fn owner_claims_user_1_once() {
  // seed wishlist row for user 1; owner "111" logs in twice → both resolve to id 1; wishlist row still there; users count unchanged
}
#[tokio::test] async fn non_owner_never_claims() { /* owner = "999"; "111" logs in → new id != 1; user 1 discord_id still NULL */ }
#[test] fn return_to_is_sanitised() {
  for bad in ["https://evil", "//evil", "/\\evil", "javascript:alert(1)", ""] { assert_eq!(safe_return_to(Some(bad)), "/"); }
  assert_eq!(safe_return_to(Some("/plan?x=1")), "/plan?x=1");
  assert_eq!(safe_return_to(None), "/");
}
#[tokio::test] async fn logout_kills_session() { /* POST /auth/logout (with Origin) → 204, cookie cleared; old cookie → /api/me 401 */ }
#[tokio::test] async fn dev_login_only_on_localhost() {
  // Config::for_tests() (dev_login true) → GET /auth/dev-login?as=alice → 303 + session; discord_id "dev:alice"
  // config with public_url https://x and dev_login false → 404
}
```

- [ ] **Step 2: Implement.**
  - Build the authorize URL with `reqwest::Url::parse_with_params`.
  - Do the token exchange with `reqwest::Client::new().post(format!("{base}/oauth2/token")).basic_auth(id, Some(secret)).form(&[...])`.
  - Log Discord failures (status only, never bodies, which may hold tokens) and return a `502` JSON error.
  - Call `purge_expired` on each successful login.
  - `/auth/logout` uses `CurrentUser`, so the Origin check applies, and also deletes the session.
- [ ] **Step 3.** Run `mise run test && mise run lint`. Commit: `feat(auth): discord oauth login, logout, dev login`.

---

### Task 4: `/api/me`, self-service tokens, limits, account deletion

**Files:**
- `src/api/me.rs` (new; `/me` and `/tokens` routes)
- `src/api/mod.rs` (merge)
- `src/tokens.rs` (per-user names, revoke by user + name)
- `src/api/user.rs` (wishlist limit)
- `src/main.rs` (CLI `--user` on revoke)

- [ ] **Step 1: Failing tests.**
  - `me_returns_profile_or_401`.
  - `tokens_crud`: POST `{name:"pc"}` returns `201 {name, token}`. GET lists it without the token. The bearer works on `/api/wishlist`. DELETE `/api/tokens/pc` returns `204`, after which the bearer gets `401`. DELETE again returns `404`.
  - `token_names_are_per_user`: users 1 and 2 can each create `pc`, and a duplicate for the same user gets `400`.
  - `token_name_validation`: empty and 65-character names get `400`.
  - `token_limit_10`: the 11th token gets `400`.
  - `wishlist_limit_300`: seed 300 items plus wishlist rows, then the 301st POST gets `400`.
  - `delete_account_only_removes_own_rows`:
    - Users A and B each have wishlist, inventory, progress, tokens and sessions.
    - `DELETE /api/me` as B returns `204` and clears the cookie.
    - B has no rows in any user table. A's counts are unchanged.
    - `items`, `recipes` and `drop_sources` counts are unchanged.
- [ ] **Step 2: Implement.**
  - `tokens::create` checks the per-user name and count limits and returns a typed error. `tokens::revoke(pool, user_id, name)`.
  - The CLI `token revoke --name N [--user ID]` defaults to user 1.
  - `DELETE /api/me` runs `DELETE FROM users WHERE id=$1` and lets the cascades do the rest.
- [ ] **Step 3.** Run `mise run test && mise run lint`. Commit: `feat(auth): profile, self-service tokens, limits, account deletion`.

---

### Task 5: Web — auth context, landing, item page, settings

**Files:**
- `web/src/api.ts`
- `web/src/App.tsx`
- `web/src/auth.tsx` (new)
- `web/src/Landing.tsx` (new)
- `web/src/ItemPage.tsx` (new)
- `web/src/Settings.tsx` (new)
- `web/src/components.tsx`
- `web/src/index.css`
- `web/vite.config.ts` (proxy `/auth` as well)

- [ ] **Step 1: API client.**
  - Add types `Me`, `TokenRow` and `NewToken`.
  - `api()` sends `credentials: 'same-origin'`; the browser sets `Origin` itself.
  - On `401`, call a registered `onUnauthorized()` hook. It flips auth state to signed-out exactly once and navigates to `/?expired=1`.
  - Guard against loops: the hook is a no-op while already signed out, and `/api/me` probing never triggers it.
- [ ] **Step 2: `AuthProvider`.**
  - Fetch `/api/me` once on load, giving one of three states: `loading`, `{me}` or `signedOut`.
  - `signIn()` sets `location.href = '/auth/discord/login?return_to=' + encodeURIComponent(path)`.
  - `signOut()` POSTs to `/auth/logout`, then sends the user to `/`.
- [ ] **Step 3: Routes.**

  | Path | Signed out | Signed in |
  |---|---|---|
  | `/` | `Landing` | `Wishlist` |
  | `/items/:id` | `ItemPage` (read-only) | `ItemPage` + "Add to wishlist" |
  | `/plan`, `/inventory`, `/settings` | redirect to `/` | the page |

  Header:
  - **Signed out:** "Sign in with Discord".
  - **Signed in:** avatar (`<img alt="">` next to the name), display name, Settings link, Sign out button.

  Nav links appear only when signed in.
- [ ] **Step 4: `Landing`.**
  - A one-line pitch and the sign-in button.
  - The `ItemSearch` component; picking a result navigates to `/items/:id`.
  - Notices for `?login=cancelled` and `?expired=1`.
- [ ] **Step 5: `ItemPage`.**
  - `GET /api/items/:id` drives the build tree and the sources table.
  - Reuse the `ProgressNode` tree rendering from `Wishlist.tsx`; extract it into `components.tsx` if it isn't exported already. Make it read-only when signed out (no checkboxes or count inputs).
- [ ] **Step 6: `Settings`.**
  - The token list with Revoke buttons.
  - The new-token form shows the token once, inside `<code>`, with a Copy button (`navigator.clipboard.writeText`) and the line `WFT_URL=${location.origin} WFT_TOKEN=<token> wft-sync --once`.
  - A "Delete my account" section: the button stays disabled until the user types `delete`, then calls `DELETE /api/me` and redirects to `/`.
- [ ] **Step 7.** Run `cd web && npm run typecheck && npm run build`. Commit: `feat(web): discord sign-in, landing, item page, settings`.

---

### Task 6: Local verification and PR (same agent)

- [ ] **Step 1: Local run.** `PUBLIC_URL=http://localhost:3100 DEV_LOGIN=1 PORT=3100 cargo run -- serve` in the background, with `web/dist` built. Then run through this with curl, holding a cookie jar and `-H 'Origin: http://localhost:3100'` on writes:
  1. `/api/wishlist` returns 401 while signed out.
  2. `/auth/dev-login?as=alice` gives a session.
  3. `/api/me` works.
  4. Add a wishlist item.
  5. Create a token, then use it as a bearer on `/api/plan`.
  6. Revoke it.
  7. `/auth/logout`, after which `/api/wishlist` returns 401.
  8. Also run `GET /api/items?q=saryn` signed out.

  If Playwright MCP is usable, click through the same flow in a browser and check that there's no redirect loop. If not, say so.
- [ ] **Step 2: Clean up.**
  - Remove the dev users you created (`DELETE FROM users WHERE discord_id LIKE 'dev:%'`); their rows cascade.
  - Confirm user 1's rows are untouched (record their counts before and after).
  - Stop the server.
- [ ] **Step 3: Final checks.**
  - `mise run lint` and `mise run test` pass.
  - The web build passes.
  - `docker build -t wt:dev .` succeeds.
- [ ] **Step 4: PR.**
  - Bump `Cargo.toml` and `sync/Cargo.toml` to `0.4.0`.
  - Push `feat/discord-auth` and open a PR against `main`.
  - Body: a per-task summary, test evidence, the curl transcript with secrets redacted, deviations and why, and a **"Deploy prerequisites"** section repeating the spec's Deployment list.
  - Don't merge, tag, or touch the homelab repo.

---

### Task 7: Chart and deploy (lead, after the user provides inputs)

Inputs needed: the AWS secret name and the `OWNER_DISCORD_ID`.

1. Homelab PR: `apps/warframe-tracker/templates/secret.yaml`, an `ExternalSecret` using `dataFrom.extract` as in `discord-file-sync-bot`.
2. Changes to the `main` controller only:
   - `envFrom: [{secretRef: {name: warframe-tracker-discord}}]`;
   - `PUBLIC_URL: https://warframe.[{ env "env_domain" }]` (unquoted FlagOps scalar);
   - `OWNER_DISCORD_ID`.
3. The image tag goes to `0.4.0`.
4. A comment marks the Traefik `ratelimit` middleware for when the ingress goes public.
5. Validate with `helm template` against a scratch copy.
6. Merge the app PR, tag `v0.4.0`, confirm the image is published, then merge the homelab PR.
7. Watch the rollout. Sign in as the owner and confirm user 1's wishlist is intact.
