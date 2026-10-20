# warframe-tracker

Self-hosted Warframe farming tracker. It imports game data (warframes, weapons, mods, arcanes, their components, resources, relics and drop tables) from the official wiki's Lua data modules, lets you wishlist items, check off components and record inventory counts, and turns what's still missing into a grouped farm plan.

One Rust server binary with these subcommands, plus the `wft-sync` client (see [Syncing inventory](#syncing-inventory-from-the-game)):

| Command | Does |
|---|---|
| `warframe-tracker serve` | API + the React UI on port 3000 (default) |
| `warframe-tracker import` | Fetch changed wiki modules and upsert game data. Unchanged revisions are skipped, and user data is never touched |
| `warframe-tracker token create --name N [--user ID]` | Print a new API token for `wft-sync` (only its hash is stored) |
| `warframe-tracker token revoke --name N [--user ID]` | Revoke a token (exit 1 if there's no such token). `--user` defaults to 1 |
| `warframe-tracker dump <dir>` | Dev tool: write every evaluated module as JSON |

Config: `DATABASE_URL` (required) and `WEB_DIR` (static UI dir, default `web/dist`). The schema is applied on startup.

`serve` also needs `PUBLIC_URL` (e.g. `https://warframe.graytonward.com`) plus `DISCORD_CLIENT_ID` and `DISCORD_CLIENT_SECRET` for "Sign in with Discord" (redirect URI `{PUBLIC_URL}/auth/discord/callback`). `OWNER_DISCORD_ID` is the Discord user whose first sign-in claims the pre-auth data (user 1). For local development, `PUBLIC_URL=http://localhost:3000 DEV_LOGIN=1` replaces Discord with `/auth/dev-login?as=<name>`.

## Run it

```sh
docker run -d -p 3000:3000 -e DATABASE_URL=postgres://user:pass@host:5432/warframe ghcr.io/graytonio/warframe-tracker:latest
docker run --rm -e DATABASE_URL=... ghcr.io/graytonio/warframe-tracker:latest import
```

The UI shows a banner until the first import has run.

## Syncing inventory from the game

`wft-sync` is a small client for the gaming PC. It reads the `inventory.json` that [WFHelper](https://github.com/WFHelper/WFHelper) writes, and pushes it to the tracker whenever it changes. **WFHelper must be running**: it's what fetches your inventory (about every 10 minutes). `wft-sync` never touches the game or DE's servers.

What it syncs: resource counts, owned frames and weapons, built components, and owned blueprints (main and part blueprints). The game's report replaces the counts it covers. Counts you typed in by hand for things the game doesn't report are kept.

1. Create a token under **Settings** in the web app (or, as an admin, on the server). It's shown once; only its hash is stored.

   ```sh
   kubectl -n warframe-tracker exec deploy/warframe-tracker-production-main -- warframe-tracker token create --name gaming-pc
   ```

2. Download `wft-sync-windows-x86_64.exe` or `wft-sync-linux-x86_64` from the [latest release](https://github.com/graytonio/warframe-tracker/releases/latest), and run it once to check it works:

   ```sh
   export WFT_URL=https://warframe.graytonward.com WFT_TOKEN=<token>
   wft-sync --once
   ```

   It prints `synced N paths: matched=… unmatched=… updated=…`.

| Flag | Env | Default |
|---|---|---|
| `--url` | `WFT_URL` | required |
| `--token` | `WFT_TOKEN` | required. Prefer the env var, so the token stays out of shell history |
| `--file` | `WFT_FILE` | WFHelper's file: `%APPDATA%\WFHelper\api-helper\inventory.json` on Windows, `~/.config/WFHelper/api-helper/inventory.json` on Linux (`$WFHELPER_USER_DATA` overrides the folder) |
| `--interval` | `WFT_INTERVAL` | `60` seconds between checks |
| `--once` | | sync once and exit |

It skips the push when the file hasn't changed, retries on errors (a half-written file, the server restarting), and exits with status 2 if the token is rejected.

**Windows**: run it at logon with Task Scheduler.

```bat
setx WFT_URL https://warframe.graytonward.com
setx WFT_TOKEN <token>
schtasks /create /sc onlogon /tn wft-sync /tr "C:\Tools\wft-sync.exe"
```

**Linux**: a systemd user service. Put `WFT_TOKEN=<token>` in `~/.config/wft-sync.env` (`chmod 600`), then save this as `~/.config/systemd/user/wft-sync.service` and run `systemctl --user enable --now wft-sync`.

```ini
[Unit]
Description=Warframe tracker inventory sync
After=network-online.target

[Service]
Environment=WFT_URL=https://warframe.graytonward.com
EnvironmentFile=%h/.config/wft-sync.env
ExecStart=%h/.local/bin/wft-sync
Restart=on-failure
RestartSec=60
# 2 = token rejected; restarting won't fix that.
RestartPreventExitStatus=2

[Install]
WantedBy=default.target
```

**Revoke** a token (for example, a lost PC):

```sh
kubectl -n warframe-tracker exec deploy/warframe-tracker-production-main -- warframe-tracker token revoke --name gaming-pc
```

## Develop

Requires [mise](https://mise.jdx.dev) and Docker. The dev Postgres runs in Docker Compose on host port 5433.

| Command | Does |
|---|---|
| `mise run db` | Start only Postgres |
| `mise run run` | Run the server natively against the Compose Postgres |
| `mise run import` | Import game data from the wiki |
| `mise run test` | Tests (starts Postgres; each test gets its own database) |
| `mise run lint` | `cargo fmt --check` + clippy |
| `mise run web` | Build the frontend into `web/dist` |
| `cargo run -- dump wiki-json` | Dump every wiki module as JSON into `wiki-json/` (gitignored) |

Frontend dev loop: run `mise run run` for the API, then `cd web && npm install && npm run dev`. Vite proxies `/api` to `localhost:3000`. Run `npm run typecheck` before you push.

## Release

Bump `version` in `Cargo.toml`, commit, then `git tag vX.Y.Z && git push --tags`. CI checks that the tag matches `Cargo.toml`, publishes an amd64 image to `ghcr.io/graytonio/warframe-tracker`, creates a GitHub release, and attaches `wft-sync` binaries for Linux and Windows.

## Deploy

Deployed from `apps/warframe-tracker/` in `homelab-flagops-templates` (bjw-s `app-template`):

- controller `main` runs `serve` behind a private ingress at `warframe.<env_domain>`.
- controller `import` is a CronJob that runs `import` every 6 hours. The import isn't run at startup, so a slow wiki never blocks the web pod.
- `templates/pg.yaml` is a Zalando `postgresql` cluster `warframe-tracker-db`. Credentials come from the operator's secret, and `DATABASE_URL` uses `sslmode=require`, because Spilo rejects non-SSL connections.

After the first deploy, trigger an import instead of waiting for the schedule:

```sh
kubectl -n warframe-tracker get cronjob
kubectl -n warframe-tracker create job --from=cronjob/<name> first-import
```
