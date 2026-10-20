# warframe-tracker

Self-hosted Warframe farming tracker. It imports game data (warframes, weapons, mods, arcanes, their components, resources, relics and drop tables) from the official wiki's Lua data modules, lets you wishlist items, check off components and record inventory counts, and turns what's still missing into a grouped farm plan.

One Rust binary with three subcommands:

| Command | Does |
|---|---|
| `warframe-tracker serve` | API + the React UI on port 3000 (default) |
| `warframe-tracker import` | Fetch changed wiki modules and upsert game data. Unchanged revisions are skipped, and user data is never touched |
| `warframe-tracker dump <dir>` | Dev tool: write every evaluated module as JSON |

Config: `DATABASE_URL` (required) and `WEB_DIR` (static UI dir, default `web/dist`). The schema is applied on startup.

## Run it

```sh
docker run -d -p 3000:3000 -e DATABASE_URL=postgres://user:pass@host:5432/warframe ghcr.io/graytonio/warframe-tracker:latest
docker run --rm -e DATABASE_URL=... ghcr.io/graytonio/warframe-tracker:latest import
```

The UI shows a banner until the first import has run.

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

Bump `version` in `Cargo.toml`, commit, then `git tag vX.Y.Z && git push --tags`. CI checks that the tag matches `Cargo.toml`, publishes a multi-arch image to `ghcr.io/graytonio/warframe-tracker`, and creates a GitHub release.

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
