use anyhow::{Context, anyhow};
use mlua::chunk::ChunkMode;
use mlua::{
    HookTriggers, Lua, LuaOptions, LuaSerdeExt, StdLib, Value, VmState, serde::DeserializeOptions,
};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Wiki modules are editable by anyone, so a vandalised one must fail the import, not hang or OOM it.
/// The largest real module evaluates in well under a second and ~100 MB.
const MEMORY_LIMIT: usize = 512 * 1024 * 1024;
const TIME_LIMIT: Duration = Duration::from_secs(30);

/// Lookup indexes a module builds from its own data. They have non-string keys
/// (JSON can't hold them) and duplicate every record, so they're dropped.
const DERIVED_KEYS: &[(&str, &[&str])] = &[("Module:Missions/data", &["by", "dictionary"])];

/// Evaluate a wiki data module the way Scribunto would, minus `mw` and any I/O.
/// `require` resolves only to titles present in `sources`.
pub fn eval_module(
    title: &str,
    sources: &HashMap<String, String>,
) -> anyhow::Result<serde_json::Value> {
    let lua = Lua::new_with(
        StdLib::STRING | StdLib::TABLE | StdLib::MATH,
        LuaOptions::default(),
    )?;
    lua.set_memory_limit(MEMORY_LIMIT)?;
    let deadline = Instant::now() + TIME_LIMIT;
    lua.set_global_hook(
        HookTriggers::new().every_nth_instruction(10_000),
        move |_, _| {
            if Instant::now() > deadline {
                return Err(mlua::Error::runtime("module took too long to evaluate"));
            }
            Ok(VmState::Continue)
        },
    )?;
    let g = lua.globals();
    for f in ["load", "loadstring", "loadfile", "dofile"] {
        g.set(f, Value::Nil)?;
    }

    let sources = sources.clone();
    let loaded = lua.create_table()?;
    let require = lua.create_function(move |lua, name: String| {
        if let Ok(v @ Value::Table(_)) = loaded.get::<Value>(name.as_str()) {
            return Ok(v);
        }
        let code = sources
            .get(&name)
            .ok_or_else(|| mlua::Error::runtime(format!("require of unknown module {name}")))?;
        // Text only: Lua 5.1 has no bytecode verifier.
        let v: Value = lua
            .load(code.as_str())
            .set_name(name.as_str())
            .set_mode(ChunkMode::Text)
            .eval()?;
        loaded.set(name.as_str(), v.clone())?;
        Ok(v)
    })?;
    g.set("require", require.clone())?;

    let value: Value = require
        .call(title.to_string())
        .with_context(|| format!("evaluating {title}"))?;
    if let (Value::Table(t), Some((_, keys))) =
        (&value, DERIVED_KEYS.iter().find(|(m, _)| *m == title))
    {
        for k in *keys {
            t.set(*k, Value::Nil)?;
        }
    }
    let opts = DeserializeOptions::new().deny_unsupported_types(false);
    lua.from_value_with(value, opts)
        .map_err(|e| anyhow!("converting {title} to JSON: {e}"))
}

#[cfg(test)]
mod tests {
    use super::eval_module;
    use std::collections::HashMap;

    fn src(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn evaluates_table_literal_with_tuples() {
        let s = src(&[(
            "Module:A",
            r#"return { X = { Count = 2, Parts = { { "Neurodes", "Resource", 12.5 } } } }"#,
        )]);
        let v = eval_module("Module:A", &s).unwrap();
        assert_eq!(v["X"]["Count"], 2);
        assert_eq!(v["X"]["Parts"][0][0], "Neurodes");
        assert_eq!(v["X"]["Parts"][0][2], 12.5);
    }

    #[test]
    fn require_resolves_only_known_modules() {
        let s = src(&[
            (
                "Module:A",
                "local T = require('Module:T') return { n = T.double(2) }",
            ),
            (
                "Module:T",
                "return { double = function(x) return x * 2 end }",
            ),
        ]);
        assert_eq!(eval_module("Module:A", &s).unwrap()["n"], 4);
        let bad = src(&[("Module:A", "return require('Module:Missing')")]);
        assert!(eval_module("Module:A", &bad).is_err());
    }

    #[test]
    fn sandbox_blocks_os_io_and_load() {
        for code in [
            "return os.time()",
            "return io.open('x')",
            "return loadstring('return 1')()",
        ] {
            let s = src(&[("Module:A", code)]);
            assert!(eval_module("Module:A", &s).is_err(), "{code} should fail");
        }
    }

    #[test]
    fn missions_derived_indexes_are_dropped() {
        let s = src(&[(
            "Module:Missions/data",
            "return { RegionResources = { Earth = { 'Rubedo' } }, by = { Planet = { [true] = {} } }, dictionary = {} }",
        )]);
        let v = eval_module("Module:Missions/data", &s).unwrap();
        assert_eq!(v["RegionResources"]["Earth"][0], "Rubedo");
        assert!(v.get("by").is_none());
    }

    #[test]
    fn runaway_modules_fail_instead_of_hanging_or_oom() {
        let bomb = src(&[("Module:A", "return string.rep('x', 2^30)")]);
        assert!(eval_module("Module:A", &bomb).is_err());
        // ponytail: this test spends TIME_LIMIT (30s) spinning; shrink it if the suite gets slow.
        let spin = src(&[("Module:A", "while true do end")]);
        assert!(eval_module("Module:A", &spin).is_err());
    }

    #[test]
    fn bytecode_chunks_are_rejected() {
        // "\27Lua" is the binary chunk signature.
        let s = src(&[("Module:A", "\x1bLuaQ\0")]);
        let err = eval_module("Module:A", &s).unwrap_err();
        assert!(format!("{err:#}").contains("binary"), "{err:#}");
    }

    #[test]
    fn functions_in_returned_table_are_dropped() {
        let s = src(&[("Module:A", "return { a = 1, f = function() end }")]);
        let v = eval_module("Module:A", &s).unwrap();
        assert_eq!(v["a"], 1);
        assert!(v.get("f").is_none());
    }
}
