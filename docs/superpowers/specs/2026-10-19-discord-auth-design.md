# Discord Auth — Design

Date: 2026-10-19
Status: Approved 2026-10-19
Builds on: `2026-10-19-warframe-tracker-design.md`, `2026-10-19-inventory-sync-design.md`

## Goal

Make Warframe Tracker multi-user and ready to expose publicly, without
storing passwords.
- **Who can sign in:** anyone with a Discord account, through "Sign in with Discord".
- **Signed-out visitors:** get a landing page and a read-only catalog.
- **Signed-in users:** each get their own wishlist, inventory, plan and sync
  tokens.

**Decisions** made during brainstorming:

| Question | Decision |
|---|---|
| Who may sign in | Anyone with a Discord account (open sign-up, in preparation for public exposure) |
| What signed-out visitors see | Landing page plus a read-only catalog (search, build trees, drop sources) |
| Approach | App-native OAuth2 with server-side sessions in Postgres (not stateless cookies, not oauth2-proxy) |

## Non-goals

- Passwords, email, and other identity providers.
- Roles and admin UI. Admin stays on the CLI.
- Rate limiting in the app. It belongs at the ingress (Traefik `ratelimit`)
  when the app goes public; the chart gets a note.
- Making the ingress public. That's a separate one-line chart change.

## Data

All schema changes are additive and applied on boot.

```
users
  + discord_id     text UNIQUE         -- Discord snowflake; NULL only for the legacy user 1 until claimed
  + display_name   text
  + avatar         text                -- Discord avatar hash; URL built at read time
  + last_login_at  timestamptz

sessions (
  id            bigserial PRIMARY KEY,
  user_id       bigint NOT NULL REFERENCES users(id) ON DELETE CASCADE,
  token_hash    bytea  NOT NULL UNIQUE,   -- sha256(cookie value), same scheme as api_tokens
  created_at    timestamptz NOT NULL DEFAULT now(),
  last_seen_at  timestamptz NOT NULL DEFAULT now(),
  expires_at    timestamptz NOT NULL
)

api_tokens
  - UNIQUE (name)            → + UNIQUE (user_id, name)   -- names scoped per user
```

User-owned tables (`wishlist`, `component_progress`, `inventory`,
`api_tokens`) get `ON DELETE CASCADE` on `user_id`, so account deletion
works. These are migrated in place: drop the FK and re-add it, guarded so
it runs once.

**Existing data.** On the first login whose Discord ID equals
`OWNER_DISCORD_ID`, if user 1 has no `discord_id`, that login claims user 1.
Every existing row (wishlist, inventory, progress, sync token) becomes
the owner's. Every other login gets a new user.

## Login flow

1. **`GET /auth/discord/login?return_to=<path>`**
   - `return_to` must be a relative path: start with `/` and not `//`.
     Otherwise it becomes `/`.
   - Generate a random 32-byte `state`. Store `state` and `return_to` in a
     cookie, `wft_oauth` (`HttpOnly; Secure; SameSite=Lax; Max-Age=600;
     Path=/auth`).
   - Redirect to `https://discord.com/oauth2/authorize` with
     `response_type=code`, `client_id`, `scope=identify`, `state` and
     `redirect_uri={PUBLIC_URL}/auth/discord/callback`.
2. **`GET /auth/discord/callback?code&state`**
   - **State check:** if `state` is missing or doesn't match the cookie,
     return `400`. If Discord passes `error=access_denied`, redirect to
     `/?login=cancelled`.
   - **Exchange the code:** `POST {DISCORD_API_BASE}/oauth2/token`, form
     encoded: `grant_type=authorization_code`, `code`, `redirect_uri`, and
     client id and secret as HTTP basic auth.
   - **Fetch the user:** `GET {DISCORD_API_BASE}/users/@me` with the access
     token. Use `id`, `global_name` (falling back to `username`) and
     `avatar`.
   - **Discard** the Discord access and refresh tokens. They are never
     stored or logged.
   - **Store the user:** upsert by `discord_id` and update the display name,
     avatar and `last_login_at`. Apply the owner claim above.
   - **Clean up:** delete expired sessions, for all users.
   - **Session:** create one (random 32 bytes, stored hashed, 30 days) and
     set the `wft_session` cookie (`HttpOnly; Secure; SameSite=Lax; Path=/;
     Max-Age=2592000`). Clear `wft_oauth`.
   - **Redirect** to `return_to`.
3. **`POST /auth/logout`:** delete this session's row, clear the cookie,
   return `204`.
4. **Sliding expiry:** a request whose session `last_seen_at` is more than
   an hour old updates `last_seen_at` and pushes `expires_at` out to now +
   30 days. That's at most one write per session per hour.

`Secure` is left off the cookies when `PUBLIC_URL` starts with
`http://localhost`, so local development works over plain HTTP.

**Dev login:** when `DEV_LOGIN=1` *and* `PUBLIC_URL` is a localhost URL,
`GET /auth/dev-login?as=<name>` creates or loads a user with
`discord_id = "dev:<name>"` and signs in. It is refused (`404`) in any
other configuration, and logged at startup whenever it's enabled.

## Authentication and authorization

`CurrentUser` resolves the user in this order:
1. **`Authorization: Bearer <token>`:** a valid token gives that token's
   user; an invalid one gives `401`. This is unchanged.
2. **`wft_session` cookie:** a valid, unexpired session gives its user.
3. **Otherwise:** `401 {"error":"sign in required"}`. The old fallback to
   user 1 is removed.

`OptionalUser` is the same, but yields `None` instead of a `401`. It is used
only by the public routes that show user progress.

| Route | Auth |
|---|---|
| `GET /api/items`, `GET /api/meta` | public |
| `GET /api/items/{id}` | `OptionalUser`; anonymous visitors see the tree with `have=0` and nothing obtained |
| `/auth/*`, static SPA | public |
| `GET/DELETE /api/me` | `CurrentUser` |
| wishlist, progress, inventory, plan | `CurrentUser` |
| `GET/POST /api/tokens`, `DELETE /api/tokens/{name}` | `CurrentUser` (session) |
| `POST /api/sync/inventory` | `CurrentUser` (token or session) |

`TokenUser` is folded into `CurrentUser`. The sync endpoint accepts both
token and session.

**CSRF:**
- Session cookies are `SameSite=Lax`, so browsers don't send them on
  cross-site `POST`, `PUT`, `PATCH` or `DELETE`.
- Every write endpoint requires `Content-Type: application/json`. Forms
  can't send that cross-site without a CORS preflight, which we never
  grant.
- When a write is authenticated by **session**, its `Origin` header must
  equal `PUBLIC_URL`'s origin. Otherwise it gets `403`. Bearer-token writes
  skip this check.

**Per-user limits** (each returns `400` with a clear message):

| Resource | Limit |
|---|---|
| Wishlist items | 300 |
| Sync tokens | 10 (unrevoked) |
| Paths per sync | 20,000 (existing) |

**`GET /api/me`** returns `{ id, display_name, avatar_url }`, where
`avatar_url` is
`https://cdn.discordapp.com/avatars/{discord_id}/{avatar}.png?size=64`, or
null.

**`DELETE /api/me`** deletes the user. Cascades remove their wishlist,
progress, inventory, tokens and sessions. It clears the cookie and returns
`204`.

## Tokens become self-service

- `GET /api/tokens` returns `[{ name, created_at, last_used_at }]`, the
  unrevoked tokens only.
- `POST /api/tokens {name}` returns `201 {name, token}`. The token is shown
  once. A name that's empty, longer than 64 characters, or already in use
  for this user gets `400`.
- `DELETE /api/tokens/{name}` revokes the token (`204`, or `404`).
- The CLI keeps `token create --name N [--user ID]` and
  `token revoke --name N [--user ID]` for admin use. `--user` defaults to 1.

## UI

- **Header:**
  - Signed out: a "Sign in with Discord" button, linking to
    `/auth/discord/login?return_to=<current path>`.
  - Signed in: the avatar and display name, a Settings link, and Sign out.
- **Signed out:**
  - `/` is a landing page: a one-line pitch, the sign-in button, and a
    catalog search.
  - `/items/:id` (a new route) is a read-only item page: the build tree
    and a sources table.
  - The nav shows only the catalog.
- **Signed in:**
  - `/` is the wishlist, as it is today.
  - `/items/:id` adds an "Add to wishlist" button.
  - The nav shows Wishlist, Plan, Inventory and Settings.
- **Settings:**
  - The sync token list, with revoke.
  - "New token": enter a name, and the token is shown once with a copy
    button and a ready-to-paste `WFT_URL=… WFT_TOKEN=… wft-sync --once`
    line.
  - "Delete my account", behind a typed confirmation.
- **Global 401 handling:** any API call that returns `401` while the UI
  thinks you're signed in clears that state and sends you to the landing
  page with a "Session expired, please sign in again" notice.
- **`?login=cancelled`:** the landing page shows a dismissible notice.

## Configuration

| Env | Where | Notes |
|---|---|---|
| `DISCORD_CLIENT_ID` | Secret | from the Discord application |
| `DISCORD_CLIENT_SECRET` | Secret | from the Discord application |
| `PUBLIC_URL` | Chart value | `https://warframe.graytonward.com`; builds the redirect URI and the Origin check |
| `OWNER_DISCORD_ID` | Chart value | your Discord user ID; claims user 1 |
| `DISCORD_API_BASE` | (tests) | defaults to `https://discord.com/api`; overridden only by tests |
| `DEV_LOGIN` | (local) | see Dev login |

**Startup checks:**
- `serve` refuses to start when `PUBLIC_URL` is unset.
- It also refuses when `DISCORD_CLIENT_ID` or `DISCORD_CLIENT_SECRET` is
  unset, unless dev login is enabled.
- The `import` and `token` subcommands need none of these.

## Deployment

- **Discord application:** create one at discord.com/developers. Under
  OAuth2, add these redirects:
  - `https://warframe.graytonward.com/auth/discord/callback`
  - optionally `http://localhost:3000/auth/discord/callback`
- **AWS Secrets Manager:** one secret, a JSON object with the keys
  `DISCORD_CLIENT_ID` and `DISCORD_CLIENT_SECRET`. The chart's
  `ExternalSecret` extracts it the same way `discord-file-sync-bot` does
  (`dataFrom.extract`). The secret name is the one input the chart needs
  from you. The proposal is `warframe-tracker-discord`.
- **Chart:**
  - `templates/secret.yaml` holds the `ExternalSecret`.
  - The `main` controller gets `envFrom` for that secret, plus `PUBLIC_URL`
    and `OWNER_DISCORD_ID` values.
  - The CronJob is unchanged.
  - A comment marks where a Traefik `ratelimit` middleware goes when the
    ingress becomes public.
- **Release order:**
  1. You create the Discord app and the AWS secret.
  2. Merge the chart PR (secret plus env) together with the image bump to
     the auth release (v0.4.0).

  The new image refuses to start without the Discord settings, so the two
  must land together.
- **`wft-sync`:** unchanged. Existing tokens keep working, because the
  owner claim keeps user 1's token attached to you.

## Testing

**Server**, with an axum stub for the Discord token and `users/@me`
endpoints, selected via `DISCORD_API_BASE`:
- **Login:**
  - A `state` mismatch gets `400`.
  - The happy path creates the user, session and cookie, and redirects to
    `return_to`.
  - An external `return_to` (`https://evil`, `//evil`) redirects to `/`.
  - `access_denied` redirects to `/?login=cancelled`.
  - The same Discord ID twice gives the same user, with the name and
    avatar updated.
  - The owner's first login claims user 1 and keeps user 1's wishlist and
    token. A second owner login doesn't create another user.
  - The Discord access token never appears in the DB.
- **Sessions:**
  - An expired session gets `401`.
  - Sliding expiry updates at most once an hour (use a backdated
    `last_seen_at`).
  - Logout deletes the row, after which the old cookie gets `401`.
- **Access:**
  - Each protected route gets `401` without auth.
  - The catalog routes return `200` while signed out, and anonymous
    `items/{id}` shows `have=0`.
  - A cookie-authenticated write with a foreign `Origin` gets `403`; the
    same write with a bearer token succeeds.
- **Limits and isolation:**
  - Wishlist 301 gets `400`, and token 11 gets `400`.
  - Two users can each have a token named `pc`.
  - Deleting user B leaves user A's rows intact and removes all of B's.
- **Config:** `serve` without `PUBLIC_URL` fails fast, and dev login is
  refused when `PUBLIC_URL` isn't localhost.

**Web:**
- The type check and build pass.
- Local click-through with `DEV_LOGIN`: landing page, sign in, add to
  wishlist, create and revoke a token, sign out, then a protected page
  redirects to the landing page.
