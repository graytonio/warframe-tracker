# Farm Locations — Design

Date: 2026-10-19
Status: Draft, awaiting review
Builds on: `2026-10-19-warframe-tracker-design.md`

## Goal

When a need only drops from enemies or containers (Isos, Nitain Extract,
bounty-only resources), the plan should still say **where to go**. Today it
lists "Thrax Centurion" or "Reinforced Carrypod", or shows an internal
drop-table name, and leaves the player to look up the location.

Success looks like this:
- The Plan page has a **"Where to go"** view: planets and open worlds,
  each listing which of the player's needs it covers and why ("region
  resource", "3 missions", "12 enemies").
- Bounty and activity tables show a real place ("Cambion Drift: Bounty
  Lvl 30-40 Stage 2"), not `CetusBounty1-2`.
- Every location comes from structured wiki data. Prose is shown only as
  a hint, labelled as such.

## What the wiki gives us

Findings from a read-only survey of the modules on 2026-10-19:

| Signal | Where | Imported today? |
|---|---|---|
| Node → main drop table | `Missions/data.MissionDetails[].DropTableAlias` | yes |
| Node → cache table (Sabotage/Exterminate caches) | `MissionDetails[].CacheDropTableAlias` (58 nodes) | **no** |
| Node → completion-bonus table | `MissionDetails[].ExtraDropTableAlias` (48 nodes, e.g. `CorpusProximaExtra` ×24) | **no** |
| Drop table → place | `DropTables/data.Missions[*].Link`, e.g. `"Cambion Drift#Bounties"` | **no** |
| Enemy → planets / tilesets | `Module:Enemies/data/<faction>` (12 subpages), `General.Planets{}`, `General.TileSets{}` | **no** |
| Container → place | `DropTables/data.Containers[*].Link`: specific for only 4 of 52; the rest say `"Storage Container"` | n/a |
| Resource → place (prose) | `Resources/data[*].Description`, a `Location:` line (154 of 308) | **no** |

Coverage: 107 resources have sources today but no node-mapped mission and
no planet row. 101 of them gain a location from the structured signals
above:

| Signal | Resources covered |
|---|---|
| Drop-table `Link` | 75 |
| Enemy planets / tilesets | 34 |
| Cache / extra alias | 14 |
| Specific container `Link` | 3 |

The remaining 6 have prose only: four Damaged Necramech parts, Rune
Marrow and Silphsela.

**Isos**, for example:
- Its only mission row is the `CorpusProximaExtra` completion bonus, which
  isn't node-mapped today.
- Its 31 enemy sources are Railjack fighters and crewships whose
  `Planets` are the Venus, Neptune, Pluto and Veil Proximas.
- Its prose names only three of those Proximas.

## Scope

**In:**
1. **Import fixes, no new tables:**
   - Map cache and extra aliases to nodes.
   - Name tables that have no node by their `Link`.
   - Skip `_IgnoreEntry` mission tables.
2. **New data:**
   - an enemy location table;
   - a structured `planet` on drop sources;
   - a prose location hint on resources.
3. **Planner:** a `by_location` roll-up, plus a `location_hint` on needs.
4. **UI:** a "Where to go" view on the Plan page, made the default view.

**Out:**
- Per-node enemy spawn tables (the wiki doesn't have them).
- Locations for generic "Storage Container" drops.
- Enemies with no wiki record (65 of 820 droppers: Necramechs, Eidolons,
  Kuva Thralls, …).
- Mission duration or efficiency ranking.

## Data changes (importer)

### 1. All three node aliases

When building `alias → nodes`, read `CacheDropTableAlias` and
`ExtraDropTableAlias` as well as `DropTableAlias`. A node's caches and
completion bonuses become mission sources for that node, with a suffix
saying which table it is:

- `"Venus Proxima / Beacon Shield Ring (Skirmish)"` — main table, unchanged
- `"Lua / Plato (Sabotage) — caches"`
- `"Venus Proxima / Beacon Shield Ring (Skirmish) — completion bonus"`

An alias that names a table which doesn't exist (e.g. `VeilProximaExtra`)
is ignored silently, as it is today for missing tables.

### 2. Place names for tables without a node

When no node uses a table, `source_name` is built as follows:

1. Take the record's `Link` up to any `#`. That is the place, unless it
   is one of the generic links (`Bounty`, `Storage Container`, `Mission`).
2. If there is a place and `Name` doesn't already start with it, use
   `"{place}: {Name}"`. For example, `Link = "Plains of Eidolon#Bounties"`
   with `Name = "Cetus Bounty Lvl 5-15 Stage 1"` gives
   `"Plains of Eidolon: Cetus Bounty Lvl 5-15 Stage 1"`.
3. Otherwise, use `Name`, as today.

Mission tables with `_IgnoreEntry = true` are skipped, matching how enemies
and containers are already handled.

### 3. `drop_sources.planet`

Schema: `ALTER TABLE drop_sources ADD COLUMN IF NOT EXISTS planet text`.
It holds a structured place for the planner to group by. Each kind fills
it like this:

| Kind | `planet` |
|---|---|
| mission, node-mapped | `MissionDetails.Planet` |
| mission, no node, place from `Link` | that place (e.g. `Cambion Drift`) |
| planet (region resource) | the planet |
| container with a specific `Link` | that `Link` (e.g. `Zariman Ten Zero` → normalised `Zariman`) |
| enemy | NULL (resolved through `enemy_locations`) |
| everything else | NULL |

### 4. `enemy_locations`

```
enemy_locations(
  enemy_name text NOT NULL,      -- matches drop_sources.source_name for kind 'enemy'
  planet text NOT NULL,
  tileset text NOT NULL DEFAULT '',  -- '' when the enemy has Planets but no TileSets
  PRIMARY KEY (enemy_name, planet, tileset)
)
```

- **Source modules:** add the 12 `Module:Enemies/data/<faction>` subpages
  to `wiki::MODULES`: anarchs, corpus, grineer, infestation, narmer,
  orokin, scaldra, sentient, stalker, techrot, themurmur, unaffiliated.
  Not the parent module, `pre-U36`, or `/doc`. They are pure table
  literals keyed by enemy name, and they reuse the existing revision
  gating (24 titles, under the 50-title API limit).
- **Rows:** the cross product of `General.Planets` × `General.TileSets`.
  Some records use `Tilesets`; accept both spellings.
- **Normalising planets:**
  - Trim each name, and drop empty names.
  - Apply a small alias table: `"Zariman Ten Zero"` → `"Zariman"`. Extend
    it only with names seen in the data.
  - Keep names that match no `MissionDetails.Planet` (e.g. `"Perita"`,
    `"Any near Kuva Fortress"`). The UI shows them as text, but they never
    resolve to nodes.
- The table is replaced on every import, like `drop_sources`.

### 5. `items.location_hint`

Schema: `ALTER TABLE items ADD COLUMN IF NOT EXISTS location_hint text`.

For resources, it holds the text after `Location:` in `Description`, up
to the end of that line, trimmed. Only resources get one. It's stored
verbatim and only ever displayed.

`IMPORT_VERSION` is bumped. If inventory sync merges first, this takes
the next number after it.

## Planner

`GET /api/plan` gains two things; nothing existing changes.

```ts
Need = { ...existing, location_hint: string | null }

Plan = { ...existing, by_location: LocationGroup[] }

LocationGroup = {
  place: string;                 // "Venus Proxima", "Cambion Drift", "Earth"
  known: boolean;                // place is a MissionDetails planet (nodes can be listed)
  covers: {
    item_id: number; name: string;
    via: string[];               // e.g. ["region resource", "3 missions", "12 enemies"]
  }[];
  nodes: string[];               // up to 5 example nodes, only when known (below)
}
```

How `by_location` is built:

1. Start from the same filtered sources as `by_source`. PvP is excluded
   unless `include_pvp=true`.
2. Map each source to a place:
   - `drop_sources.planet` when it's set;
   - for an enemy source, every `enemy_locations.planet` of that enemy;
   - relic routes (`via` a relic) map to the place of the mission that
     drops the relic.
3. Group by place. For each covered need, count its sources by kind,
   which gives the `via` strings: "region resource", "N missions", "N
   enemies", "N containers".
4. **Nodes:**
   - For a known place, list up to 5 `MissionDetails` nodes on it.
     Prefer nodes that appear as mission sources for these needs.
     Otherwise use nodes whose tileset matches a covering enemy's tileset.
   - The UI labels these "e.g.", because matching enemies to nodes by
     tileset is inference: an enemy doesn't spawn on every node with that
     tileset.
5. **Sort:** distinct needs covered (descending), then total source count
   (descending), then place (ascending).

A need with no place from any signal doesn't appear in `by_location`. It
still appears in `by_need`, with its `location_hint` if it has one.

## UI

Plan page:
- **Views:** "Where to go" (the new default), "What to run" (the
  existing `by_source`), and "What I need" (`by_need`). The chosen view
  is remembered in localStorage, like the PvP toggle.
- **"Where to go" row:** the place as a heading, then the needs covered
  as chips with their `via` text ("Isos — 4 enemies, completion bonus").
  For known places, add "e.g. Beacon Shield Ring, Nsu Grid".
- **"What I need" view:** show `location_hint` as a muted line, "Wiki
  says: Venus, Neptune, and Pluto Proximas", clearly separate from the
  structured sources.

## Testing

- **Importer (fixtures extended):**
  - A node with `CacheDropTableAlias` and `ExtraDropTableAlias` produces
    the "— caches" and "— completion bonus" sources with `planet` set.
  - A bounty table with `Link = "Cambion Drift#Bounties"` gets a
    `"Cambion Drift: …"` name and `planet = 'Cambion Drift'`.
  - An `_IgnoreEntry` mission table is skipped.
  - Enemy locations import: Planets × TileSets, the `Tilesets` spelling,
    `"Zariman Ten Zero"` normalised, empty names skipped.
  - `location_hint` is parsed from `"…\r\n\r\nLocation: Earth, Lua, Eris"`
    and is NULL when there's no `Location:` line.
- **Planner:**
  - An enemy-only need (an Isos-like seed) rolls up to its enemies'
    planets.
  - A region resource and a node-mapped mission on the same planet merge
    into one group, with both `via` strings.
  - PvP sources are excluded from `by_location` by default.
  - A need with no structured place appears in `by_need` with its
    `location_hint`, and not in `by_location`.
  - Sort order.
- **Real data check (lead):** after `mise run import`, Isos's
  `by_location` lists the four Proximas, and a Cambion Drift bounty
  resource shows `Cambion Drift`. Re-measure the coverage numbers above
  and record them in the PR.

## Open questions

1. Should "Where to go" also group by **open world** (Cetus/Plains,
   Fortuna/Orb Vallis, Necralisk/Cambion Drift, Zariman, Duviri) when the
   wiki gives that as the `Link` place? The proposal treats it like a
   planet; places come from the data as-is.
2. Should the default view stay "What to run" until you've seen "Where to
   go" on real data? The proposal makes "Where to go" the default.

## Future work

- Per-node enemy presence, if the wiki ever structures it.
- Expected-runs ranking (drop chance × rotation timing).
- Locations for generic storage containers (they drop on most tilesets of
  a faction; deliberately not inferred).
