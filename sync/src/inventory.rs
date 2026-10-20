use anyhow::Context;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

/// Parse the inventory file. A parse error usually means WFHelper is mid-write.
pub fn read(path: impl AsRef<Path>) -> anyhow::Result<Value> {
    let path = path.as_ref();
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
}

/// `{ItemType: count}` over every top-level array. Entries without `ItemCount` (frames,
/// weapons) count 1 per copy; duplicate paths are summed.
pub fn flatten(v: &Value) -> BTreeMap<String, i64> {
    let mut m = BTreeMap::new();
    for arr in v
        .as_object()
        .into_iter()
        .flat_map(|o| o.values())
        .filter_map(Value::as_array)
    {
        for e in arr {
            if let Some(t) = e["ItemType"].as_str() {
                *m.entry(t.to_string()).or_default() += e["ItemCount"].as_i64().unwrap_or(1);
            }
        }
    }
    m
}

/// Highest rank (`UpgradeFingerprint` `lvl`) of any copy, per `ItemType`. DE stores the
/// fingerprint as a JSON string, sometimes wrapped twice; unranked copies have none.
pub fn ranks(v: &Value) -> BTreeMap<String, i64> {
    let mut m = BTreeMap::new();
    for arr in v
        .as_object()
        .into_iter()
        .flat_map(|o| o.values())
        .filter_map(Value::as_array)
    {
        for e in arr {
            let (Some(t), Some(lvl)) = (
                e["ItemType"].as_str(),
                fingerprint_level(&e["UpgradeFingerprint"]),
            ) else {
                continue;
            };
            let r = m.entry(t.to_string()).or_insert(lvl);
            *r = (*r).max(lvl);
        }
    }
    m
}

fn fingerprint_level(f: &Value) -> Option<i64> {
    let mut cur = f.clone();
    for _ in 0..3 {
        match cur {
            Value::String(s) => cur = serde_json::from_str(&s).ok()?,
            _ => break,
        }
    }
    cur["lvl"].as_i64().filter(|l| *l >= 0)
}

/// `LastInventorySync.$oid`: changes whenever DE's inventory does.
pub fn sync_id(v: &Value) -> Option<String> {
    v["LastInventorySync"]["$oid"].as_str().map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_counts_copies_and_ignores_noise() {
        let v = read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/inventory.json"
        ))
        .unwrap();
        let m = flatten(&v);
        assert_eq!(m["/Lotus/Types/Items/MiscItems/Neurode"], 42);
        assert_eq!(m["/Lotus/Powersuits/Saryn/SarynPrime"], 2);
        assert_eq!(m["/Lotus/Weapons/Tenno/Rifle/BratonPrime"], 1);
        assert_eq!(m.len(), 5);
        assert_eq!(sync_id(&v).as_deref(), Some("6700000000000000000000aa"));
    }

    #[test]
    fn ranks_take_the_highest_fingerprint_level() {
        let v = serde_json::json!({
            "Upgrades": [
                {"ItemType": "/A", "UpgradeFingerprint": "{\"lvl\":3}"},
                {"ItemType": "/A", "UpgradeFingerprint": "\"{\\\"lvl\\\":5}\""},
                {"ItemType": "/B", "UpgradeFingerprint": "{}"},
                {"ItemType": "/C", "UpgradeFingerprint": "corrupt"},
                {"ItemType": "/D"}
            ],
            "RawUpgrades": [{"ItemType": "/A", "ItemCount": 2}]
        });
        assert_eq!(ranks(&v), BTreeMap::from([("/A".to_string(), 5)]));
    }

    #[test]
    fn unparsable_file_is_an_error_not_a_panic() {
        let dir = std::env::temp_dir().join(format!("wft-sync-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("inventory.json");
        std::fs::write(&f, "{\"Suits\": [").unwrap(); // truncated mid-write
        assert!(read(&f).is_err());
        assert!(read(dir.join("missing.json")).is_err());
    }
}
