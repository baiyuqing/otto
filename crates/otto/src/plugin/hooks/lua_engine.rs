//! Pure computation VM. This is a capability restriction, not OS isolation.
use mlua::{HookTriggers, Lua, LuaSerdeExt, StdLib, Value as LuaValue, VmState, chunk::ChunkMode};
use serde_json::Value;
use std::{
    collections::HashSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

pub(super) const MAX_SOURCE: u64 = 256 << 10;
const MEMORY: usize = 16 << 20;
const INSTRUCTIONS: u64 = 2_000_000;
const NODES: usize = 8192;
const DEPTH: usize = 64;

pub(super) fn run(
    source: &[u8],
    input: &[u8],
    deadline: Instant,
    stopped: Arc<AtomicBool>,
) -> Result<String, String> {
    execute(
        source,
        input,
        deadline,
        stopped,
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
        #[cfg(test)]
        None,
    )
}
fn execute(
    source: &[u8],
    input: &[u8],
    deadline: Instant,
    stopped: Arc<AtomicBool>,
    ticks: Arc<std::sync::atomic::AtomicU64>,
    #[cfg(test)] observer: Option<Arc<super::lua_handler_tests::Observer>>,
) -> Result<String, String> {
    checkpoint(&stopped, deadline)?;
    if source.len() as u64 > MAX_SOURCE
        || std::str::from_utf8(source).is_err()
        || source.starts_with(b"\x1bLua")
    {
        return Err("Lua source must be UTF-8 text at most 256 KiB; bytecode is forbidden".into());
    }
    if input.len() > super::MAX_INPUT {
        return Err("Lua input exceeds 1 MiB".into());
    }
    let lua = Lua::new_with(StdLib::MATH, mlua::LuaOptions::default())
        .map_err(|_| "cannot initialize Lua")?;
    lua.set_memory_limit(MEMORY)
        .map_err(|_| "cannot bound Lua memory")?;
    let globals = lua.globals();
    // No file/module loaders, process, environment, output, GC controls or catchable budget loops.
    for name in [
        "dofile",
        "loadfile",
        "load",
        "require",
        "collectgarbage",
        "print",
        "warn",
        "pcall",
        "xpcall",
        "setmetatable",
        "getmetatable",
    ] {
        globals
            .set(name, LuaValue::Nil)
            .map_err(|_| "cannot restrict Lua")?;
    }
    lua.set_hook(HookTriggers::new().every_nth_instruction(1000), {
        let stopped = stopped.clone();
        move |_, _| {
            #[cfg(test)]
            if let Some(observer) = &observer {
                observer.running.notify_one();
                while !observer.release.load(Ordering::Acquire) {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
            if stopped.load(Ordering::Relaxed)
                || Instant::now() >= deadline
                || ticks.fetch_add(1000, Ordering::Relaxed) >= INSTRUCTIONS
            {
                return Err(mlua::Error::runtime(
                    "Lua execution budget exhausted or cancelled",
                ));
            }
            Ok(VmState::Continue)
        }
    })
    .map_err(|_| "cannot bound Lua instructions")?;
    let json = lua.create_table().map_err(|_| "Lua memory limit")?;
    json.set("null", lua.null())
        .map_err(|_| "Lua memory limit")?;
    json.set(
        "array",
        lua.create_function(|lua, table: mlua::Table| {
            table.set_metatable(Some(lua.array_metatable()))?;
            Ok(table)
        })
        .map_err(|_| "Lua memory limit")?,
    )
    .map_err(|_| "Lua memory limit")?;
    globals.set("json", json).map_err(|_| "Lua memory limit")?;
    let input: Value = serde_json::from_slice(input).map_err(|_| "invalid Lua JSON input")?;
    let mut input_nodes = NODES;
    check_input(&input, 0, &mut input_nodes, &stopped, deadline)?;
    let input = lua.to_value(&input).map_err(|_| "Lua input memory limit")?;
    checkpoint(&stopped, deadline)?;
    let result: LuaValue = lua
        .load(source)
        .set_name("plugin.lua")
        .set_mode(ChunkMode::Text)
        .call(input)
        .map_err(|_| "Lua handler failed (invalid source, runtime error or resource limit)")?;
    if stopped.load(Ordering::Relaxed) || Instant::now() >= deadline {
        return Err("Lua execution cancelled or timed out".into());
    }
    let mut budget = Budget {
        nodes: NODES,
        bytes: super::MAX_OUTPUT,
        seen: HashSet::new(),
        stopped: &stopped,
        deadline,
    };
    let result = output(&lua, result, 0, &mut budget)?;
    let encoded = serde_json::to_string(&result).map_err(|_| "invalid Lua JSON output")?;
    if encoded.len() > super::MAX_OUTPUT {
        return Err("Lua output exceeds 64 KiB".into());
    }
    checkpoint(&stopped, deadline)?;
    Ok(encoded)
}
fn checkpoint(stopped: &AtomicBool, deadline: Instant) -> Result<(), String> {
    if stopped.load(Ordering::Relaxed) || Instant::now() >= deadline {
        return Err("Lua execution cancelled or timed out".into());
    }
    Ok(())
}
fn check_input(
    value: &Value,
    depth: usize,
    nodes: &mut usize,
    stopped: &AtomicBool,
    deadline: Instant,
) -> Result<(), String> {
    checkpoint(stopped, deadline)?;
    if depth > DEPTH || *nodes == 0 {
        return Err("Lua input exceeds depth/node limit".into());
    }
    *nodes -= 1;
    match value {
        Value::Array(values) => {
            for value in values {
                check_input(value, depth + 1, nodes, stopped, deadline)?;
            }
        }
        Value::Object(values) => {
            for value in values.values() {
                check_input(value, depth + 1, nodes, stopped, deadline)?;
            }
        }
        _ => {}
    }
    Ok(())
}
struct Budget<'a> {
    nodes: usize,
    bytes: usize,
    seen: HashSet<usize>,
    stopped: &'a AtomicBool,
    deadline: Instant,
}
impl Budget<'_> {
    fn bytes(&mut self, size: usize) -> Result<(), String> {
        self.bytes = self
            .bytes
            .checked_sub(size)
            .ok_or("Lua output exceeds 64 KiB")?;
        Ok(())
    }
}
fn output(
    lua: &Lua,
    value: LuaValue,
    depth: usize,
    budget: &mut Budget<'_>,
) -> Result<Value, String> {
    checkpoint(budget.stopped, budget.deadline)?;
    if depth > DEPTH || budget.nodes == 0 {
        return Err("Lua output exceeds depth/node limit".into());
    }
    budget.nodes -= 1;
    budget.bytes(1)?;
    Ok(match value {
        LuaValue::Nil => Value::Null,
        LuaValue::LightUserData(pointer) if pointer.0.is_null() => Value::Null,
        LuaValue::Boolean(value) => Value::Bool(value),
        LuaValue::Integer(value) => Value::from(value),
        LuaValue::Number(value) => {
            Value::Number(serde_json::Number::from_f64(value).ok_or("non-finite Lua number")?)
        }
        LuaValue::String(value) => {
            budget.bytes(value.as_bytes().len())?;
            Value::String(
                value
                    .to_str()
                    .map_err(|_| "Lua output must be UTF-8")?
                    .to_owned(),
            )
        }
        LuaValue::Table(table) => {
            let pointer = table.to_pointer() as usize;
            if !budget.seen.insert(pointer) {
                return Err("Lua output has cycles or shared tables".into());
            }
            let array = table
                .metatable()
                .is_some_and(|meta| meta == lua.array_metatable());
            let mut object = serde_json::Map::new();
            let mut elements = std::collections::BTreeMap::new();
            for entry in table.pairs::<LuaValue, LuaValue>() {
                let (key, value) = entry.map_err(|_| "invalid Lua output table")?;
                let value = output(lua, value, depth + 1, budget)?;
                match key {
                    LuaValue::String(key) if !array => {
                        budget.bytes(key.as_bytes().len())?;
                        object.insert(
                            key.to_str()
                                .map_err(|_| "Lua key must be UTF-8")?
                                .to_owned(),
                            value,
                        );
                    }
                    LuaValue::Integer(key) if key > 0 => {
                        elements.insert(key, value);
                    }
                    _ => {
                        return Err(
                            "Lua output keys must be strings or contiguous positive integers"
                                .into(),
                        );
                    }
                }
            }
            if !elements.is_empty() || array {
                if !object.is_empty() || elements.keys().copied().ne(1..=elements.len() as i64) {
                    return Err("Lua output array is sparse or has mixed keys".into());
                }
                Value::Array(elements.into_values().collect())
            } else {
                Value::Object(object)
            }
        }
        _ => return Err("Lua output is not JSON-compatible".into()),
    })
}

#[cfg(test)]
#[path = "lua_engine_tests.rs"]
mod tests;

#[cfg(test)]
pub(super) fn run_observed(
    source: &[u8],
    input: &[u8],
    deadline: Instant,
    stopped: Arc<AtomicBool>,
    observer: Arc<super::lua_handler_tests::Observer>,
) -> Result<String, String> {
    execute(
        source,
        input,
        deadline,
        stopped,
        Arc::new(std::sync::atomic::AtomicU64::new(0)),
        Some(observer),
    )
}
