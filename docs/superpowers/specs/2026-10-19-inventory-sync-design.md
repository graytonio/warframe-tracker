# Inventory Sync (file-watching) — Design

Date: 2026-10-19
Status: Draft, awaiting review
Builds on: `2026-10-19-warframe-tracker-design.md`

## Goal

Keep the tracker's inventory current without typing counts. This version
reads the `inventory.json` that [WFHelper](https://github.com/WFHelper/WFHelper)
(or Sainan's `warframe-api-helper`) already writes on the gaming PC, and
pushes it to the tracker. It never touches the game process or DE's servers.

Success looks like this:
- A player running WFHelper sees their resource counts and built parts
  reflected in the tracker within about a minute of WFHelper refreshing.
- The farm plan shrinks to match: owned resources are subtracted, built
  components drop their subtrees, and owned frames and weapons drop off
  the plan.

## Background (from the WFHelper research)

- WFHelper refreshes inventory about every 10 minutes and writes DE's raw
  inventory response (~0.9 MB JSON) to a file. It has no plugin system or
  local API; the file is its only output.
- The payload has top-level arrays (`Suits`, `LongGuns`, `Pistols`,
  `Melee`, `RawUpgrades`, `Upgrades`, `MiscItems`, `Recipes`, `Arcanes`,
  …). Their entries carry `ItemType`, a game path such as
  `/Lotus/Types/Items/MiscItems/Neurode`, plus `ItemCount` for stackable
  items. Owned frames and weapons appear once per copy, with no count.
- The wiki's `InternalName` uses the same paths. Coverage in our imported
  data:

  | Category | Records with `InternalName` |
  |---|---|
  | Warframes | 135 / 135 |
  | Primary weapons | 200 / 200 |
  | Arcanes | 166 / 166 |
  | Mods | 1643 / 1658 |
  | Resources | 922 / 1244 (the rest are mostly categories and cosmetics) |
  | Components, part blueprints, relics | none |

## Scope

**In:**
- A `wft-sync` client binary.
- A token-authenticated sync endpoint on the tracker that maps game paths
  to items.
- Token management.
- Mapping of resources, mods, arcanes, frames, weapons, built components
  and main blueprints.

**Out (deferred, listed under Future work):**
- Reading game memory directly.
- **Owned part blueprints** (e.g. an unbuilt Chassis Blueprint).
- Relics.
- Auto-completing wishlist items.
- Auth for the web UI.

## Architecture

```
gaming PC                                    homelab
┌──────────────────────────────┐            ┌──────────────────────────────┐
│ WFHelper ──writes──▶ inventory.json        │ warframe-tracker serve       │
│                         │    │  HTTPS     │  POST /api/sync/inventory    │
│ wft-sync ──polls mtime──┘    ├───────────▶│   token → user               │
│   flatten {ItemType: count}  │  Bearer    │   map paths → item_id        │
└──────────────────────────────┘  token     │   snapshot-write inventory   │
                                            └──────────────────────────────┘
```

The client is deliberately thin: read, flatten, post. Mapping happens on
the server, which already holds the wiki data, so a mapping fix never needs
a new client release.

## Client: `wft-sync`

A separate crate, `sync/`, in this repo, added as a Cargo workspace member.
Its own small dependency set (reqwest with rustls, serde_json, anyhow)
keeps cross-compiling to Windows easy. It must not pull in sqlx or mlua.

**Configuration** (flags, each with an env var):

| Flag | Env | Default |
|---|---|---|
| `--url` | `WFT_URL` | required, e.g. `https://warframe.graytonward.com` |
| `--token` | `WFT_TOKEN` | required; the env var is preferred so it stays out of shell history |
| `--file` | `WFT_FILE` | WFHelper's path for the OS (see Open questions) |
| `--interval` | `WFT_INTERVAL` | `60s` |
| `--once` | — | run one sync and exit |

**Loop:**
1. Stat `--file`. Skip the tick if the mtime is unchanged since the last
   successful push. The first tick always reads.
2. Read and parse the file. A parse error usually means WFHelper is
   mid-write: log a warning and retry on the next tick.
3. Skip the push if `LastInventorySync.$oid` equals the last pushed value.
   Otherwise flatten the inventory (below).
4. `POST /api/sync/inventory`. On success, remember the mtime and oid and
   log the summary (`matched=… unmatched=… updated=…`). On failure, log it
   and retry on the next tick. It never exits on a transient error.
   - A `401` means the token is bad: log it clearly and exit non-zero, so a
     service manager shows the failure.

**Flatten:** walk every top-level array whose entries are objects with an
`ItemType`. Add `ItemCount` when present, otherwise 1 per entry, which
counts copies of frames and weapons. Duplicate paths are summed. Output:
`{ "<ItemType>": count }`.

**Running it:** Windows via Task Scheduler at logon (`wft-sync.exe`);
Linux via a systemd user service. Examples go in the README.

**Distribution:** the release workflow builds `wft-sync` for
`x86_64-unknown-linux-gnu` and `x86_64-pc-windows-msvc`, on their native
runners in a matrix, and attaches both to the GitHub release.

The server image only builds the server package (`cargo build -p
warframe-tracker`). The Dockerfile still has to copy `sync/Cargo.toml`
and a stub `sync/src/main.rs` before its dependency-cache layer, because
Cargo needs every workspace member's manifest to resolve the workspace.

## Server changes

### Tokens

```
api_tokens(
  id bigserial PK,
  user_id FK users,
  name text,                       -- "gaming-pc"
  token_hash bytea unique,         -- sha256(token)
  created_at, last_used_at null,
  revoked_at null
)
```

- `warframe-tracker token create --name <name> [--user 1]` prints a random
  32-byte token, base64url-encoded, exactly once, and stores only its
  hash.
- `warframe-tracker token revoke --name <name>` sets `revoked_at`.
- On the cluster, run both with `kubectl exec` into the web pod.

### `CurrentUser` extractor

| `Authorization` header | Result |
|---|---|
| `Bearer <token>`, valid and not revoked | that token's `user_id`; also bumps `last_used_at` |
| `Bearer <token>`, invalid or revoked | `401 {"error": "invalid token"}` |
| absent | `UserId(1)`, as today, so the web UI is unchanged |

A new `TokenUser` extractor is the same, except that an absent header is
also a 401. Only the sync endpoint uses it.

### `POST /api/sync/inventory` (`TokenUser`)

Request:
```json
{ "items": { "/Lotus/Types/Items/MiscItems/Neurode": 412, "...": 1 } }
```

Response `200`:
```json
{ "matched": 812, "updated": 37, "unmatched": ["/Lotus/Types/..."] }
```
`unmatched` is capped at 200 entries.

**Mapping**, in order, first match wins; only items with no `removed_at`
are considered:
1. **Exact path:** `items.data->>'InternalName' = path`. Add an expression
   index on `(data->>'InternalName')`.
2. **Component by name:**
   - Take the last path segment, strip a trailing `Component`, and split
     the CamelCase into words. `SarynPrimeChassisComponent` becomes
     "Saryn Prime Chassis", and `BratonPrimeBarrel` becomes "Braton Prime
     Barrel".
   - Match that against component `wiki_name`s, ignoring case.
   - A path ending in `Blueprint` maps only when the resulting name is a
     main-blueprint component: `SarynPrimeBlueprint` becomes "Saryn Prime
     Blueprint". Part blueprints such as `SarynPrimeChassisBlueprint` are
     deliberately left unmatched (see Future work).
3. Anything else goes to `unmatched`.

**Writes**, in one transaction, as a snapshot of the user's game
inventory:
- Matched items are upserted into `inventory` with `count` and
  `source='plugin'`. `updated` counts rows whose count changed.
- `plugin` rows for this user that are absent from the snapshot are
  deleted. The item was consumed or sold.
- `manual` rows absent from the snapshot are left alone. A manual row
  that *is* in the snapshot becomes `plugin`: the game is the source of
  truth for items it reports.
- Validation: counts must be `>= 0` and the map may hold at most 20,000
  entries. Otherwise `400`.

**Planner effect:** none needed. An owned frame or weapon gives the
wishlist root `have = 1 >= 1`, so it drops out of the plan. A built
component (`have >= needed`) hides its subtree. Resources are subtracted
as they are today.

## Error handling

- **Client:** see Loop. It never logs the token.
- **Server:** a malformed body is a JSON `400` (existing middleware). A
  database error rolls back the transaction, so a partial snapshot is
  never written. An unknown path is not an error.

## Testing

- **Client:** `flatten` unit tests on a trimmed inventory fixture
  (`sync/tests/fixtures/inventory.json`). The fixture has to be built from
  WFHelper's documented shape until a real file is available (see Open
  questions). Tests cover summed duplicates, count-less entries counting
  as 1, the `LastInventorySync` oid being read, and an unparsable file
  returning an error, not a panic.
- **Server:**
  - Token: create, then authenticate. Revoked and garbage tokens get 401.
    An absent header still resolves to user 1 on normal routes but gets
    401 on sync.
  - Mapping: an exact path match; `SarynPrimeChassisComponent` mapping to
    the component; `SarynPrimeBlueprint` mapping to the main blueprint;
    `SarynPrimeChassisBlueprint` and unknown paths landing in `unmatched`.
  - Snapshot: plugin rows absent from the snapshot are deleted, manual
    rows absent from it are kept, and a manual row present in it becomes
    plugin. Negative counts get 400.
  - Planner: after syncing an owned "Saryn Prime", it no longer appears
    in `by_need`.

## Open questions (resolve before or during planning)

1. **Exact WFHelper output path.** The research shows it sits under
   Electron's `userData/api-helper/inventory.json`. The folder name
   (`WFHelper`, `wfhelper`, …) under `%APPDATA%` and `~/.config` needs
   confirming on a real install. Until then, `--file` stays required, with
   a best-guess default.
2. **A real inventory file** for the fixture and to measure the unmatched
   rate. Strip account identifiers before committing it.
3. **Windows or Linux first** for the gaming PC. Both are planned; this
   decides which one gets smoke-tested first.

## Future work

- **Part blueprints.** Owning a Chassis *blueprint* but not the built
  part should remove the relic or drop need while keeping the crafting
  resources. That needs a separate "blueprint owned" notion per component,
  for example a `component_progress.blueprint_owned` flag that the planner
  reads, instead of overloading `have`.
- **Relics** (`/Lotus/Types/Game/Projections/...`) need a path → relic
  mapping that the wiki data doesn't carry.
- **Reading memory directly** (porting WFHelper's MIT
  `gameMemoryAuthz.ts`), so WFHelper isn't required. This carries a ToS
  gray-area risk, so it would be opt-in.
- **Auth for the web UI**, reusing `api_tokens` users once multi-user
  support lands.
