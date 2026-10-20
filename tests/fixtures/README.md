# Wiki fixtures

Trimmed slices of real `cargo run -- dump wiki-json` output (2026-10-19), with the
same top-level nesting as the full modules. Used by importer tests; no network.

Regenerate: run the dump, then repeat the `jq` filters below.

| File | Kept |
|---|---|
| Warframes | `.Warframes` → Saryn, Saryn Prime |
| Weapons primary / secondary / melee | Braton, Braton Prime / Lex Prime / Skana |
| Mods | `.Mods` → Serration |
| Arcane | `.Arcanes` → Arcane Energize |
| Blueprints | every group (`Blueprints`, `Suits`) → Saryn Prime (+ Chassis, Neuroptics, Systems), Braton Prime |
| Resources | `.Resources` → Neurodes, Orokin Cell, Rubedo, Nano Spores, Alloy Plate, Salvage, Neural Sensors, Nitain Extract; `GenericComponents` (whole, added by Task 3) |
| Void | `.RelicData` → every relic whose `Drops` include Saryn Prime or Braton Prime |
| DropTables | first 2 `.Missions` whose rewards name one of those relics; `Enemies["Aerial Commander"]`; `Containers.Carrypod` |
| Missions | `RegionResources` (whole) + `MissionDetails` rows whose `DropTableAlias` is one of the kept missions |
| Enemies/data/corpus | Alad V (Planets + TileSets), 002-ER (Planets only), Taro Crewship (Railjack, drops Isos), Scrofa Crewman (`Tilesets` spelling), Derivator Crewman (`Zariman Ten Zero`) |
| Enemies/data/* (other 11) | `{}` |

Filters (`pick` = `with_entries(select(.key as $k | $names | index($k)))`):

```sh
jq --argjson names '["Saryn","Saryn Prime"]' '{Warframes: (.Warframes | pick)}' Module_Warframes_data.json
jq --argjson names '[...]' 'map_values(pick)' Module_Blueprints_data.json
jq '{RelicData: (.RelicData | with_entries(select(.value.Drops | map(.Item) | index("Saryn Prime") or index("Braton Prime"))))}' Module_Void_data.json
jq --argjson relics "$(jq -c '.RelicData|keys' fixtures/Module_Void_data.json)" '{
  Missions: (.Missions | with_entries(select(.value.Rewards | tostring | . as $s | $relics | any(. as $r | $s | contains("\"" + $r + "\"")))) | to_entries[0:2] | from_entries),
  Enemies: (.Enemies | {"Aerial Commander": .["Aerial Commander"]}),
  Containers: (.Containers | {"Carrypod": .Carrypod})}' Module_DropTables_data.json
jq --argjson a "$(jq -c '.Missions|keys' fixtures/Module_DropTables_data.json)" \
  '{RegionResources, MissionDetails: [.MissionDetails[] | select(.DropTableAlias as $d | $a | index($d))]}' Module_Missions_data.json
jq --argjson names '["Alad V","002-ER","Taro Crewship","Scrofa Crewman","Derivator Crewman"]' \
  'with_entries(select(.key as $k | $names | index($k)))' Module_Enemies_data_corpus.json
for f in anarchs grineer infestation narmer orokin scaldra sentient stalker techrot themurmur unaffiliated; do
  echo '{}' > fixtures/Module_Enemies_data_$f.json; done
```
