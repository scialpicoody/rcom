//! The `JSON` builtin: llcom's `JSON:encode`/`JSON:decode` surface (its bundled
//! Friedl JSON.lua, `json = require("JSON")` in the README sample) implemented
//! over `serde_json` per docs/design.md ("Builtin libs: ... JSON (serde-backed)").
//!
//! Table conversion follows llcom's JSON closely enough for its documented uses:
//! a Lua table that is a contiguous 1..n sequence encodes as a JSON array,
//! anything else (string or sparse keys) as an object with `tostring`-ed keys.
//! Decoding maps arrays to 1-based tables. `null` decodes to `json.null`
//! (a sentinel distinct from `nil`, so `t[k] == json.null` is testable; encode
//! treats the sentinel as `null`).

use mlua::{Lua, MultiValue, Table, Value};
use serde_json::{Map, Number, Value as Json};

/// The Lua-side `JSON` table: `{ encode = fn, decode = fn, null = sentinel, VERSION }`.
pub fn module(lua: &Lua) -> mlua::Result<Table> {
    let null = lua.create_string("json.null")?; // unique sentinel value

    let encode = {
        let null = null.clone();
        lua.create_function(move |lua, args: MultiValue| {
            let _ = &null;
            let value = unwrap_colon_call(args);
            let json = to_json(lua, value)?;
            serde_json::to_string(&json)
                .map_err(|e| mlua::Error::runtime(format!("JSON encode failed: {e}")))
                .and_then(|s| Ok(lua.create_string(s)?))
        })?
    };

    let decode = {
        let null = null.clone();
        lua.create_function(move |lua, args: MultiValue| {
            let payload = unwrap_colon_call(args);
            let text = match payload {
                Value::String(s) => s,
                _ => return Err(mlua::Error::runtime("JSON decode expects a string")),
            };
            let bytes = text.as_bytes();
            let text = std::str::from_utf8(&bytes)
                .map_err(|_| mlua::Error::runtime("JSON text must be UTF-8"))?;
            let json: Json = serde_json::from_str(text)
                .map_err(|e| mlua::Error::runtime(format!("JSON decode failed: {e}")))?;
            to_lua(lua, json, &null)
        })?
    };

    let module = lua.create_table()?;
    module.set("encode", encode)?;
    module.set("decode", decode)?;
    module.set("null", null)?;
    module.set("VERSION", "rcom-serde-1")?;
    Ok(module)
}

/// llcom scripts call `json:encode(t)` (colon → first arg is the module itself);
/// plain `json.encode(t)` also happens. Accept both: two args → drop the first.
fn unwrap_colon_call(mut args: MultiValue) -> Value {
    if args.len() >= 2 {
        args.pop_front();
    }
    args.pop_front().unwrap_or(Value::Nil)
}

/// Lua value → JSON. Tables recurse; the `null` sentinel maps to JSON null
/// (a string equal to the sentinel marker is indistinguishable — acceptable,
/// documented: scripts round-tripping the sentinel treat it as null).
pub fn to_json(lua: &Lua, value: Value) -> mlua::Result<Json> {
    match value {
        Value::Nil => Ok(Json::Null),
        Value::Boolean(b) => Ok(Json::Bool(b)),
        Value::Integer(i) => Ok(Json::Number(Number::from(i))),
        Value::Number(f) => Ok(Json::Number(
            Number::from_f64(f).unwrap_or_else(|| Number::from(0)),
        )),
        Value::String(s) => Ok(Json::String(
            String::from_utf8_lossy(s.as_bytes().as_ref()).into_owned(),
        )),
        Value::Table(t) => table_to_json(lua, t),
        _ => Err(mlua::Error::runtime(format!(
            "cannot encode a {} value as JSON",
            value.type_name()
        ))),
    }
}

fn is_null_sentinel(value: &Value) -> bool {
    matches!(value, Value::String(s) if s.as_bytes().as_ref() == b"json.null".as_ref())
}

fn table_to_json(lua: &Lua, table: Table) -> mlua::Result<Json> {
    // A pure contiguous 1..n sequence (and nothing else) → array; a mixed or
    // keyed table → object with tostring-ed keys (Friedl's rule, llcom parity:
    // {1,2} stays an array, {1, x=2} becomes {"1":1,"x":2}).
    let len = table.raw_len();
    let mut count = 0usize;
    let mut pure_array = true;
    for pair in table.pairs::<Value, Value>() {
        let (key, _) = pair?;
        count += 1;
        match key {
            Value::Integer(i) if i >= 1 && i <= len as i64 => {}
            _ => {
                pure_array = false;
                break;
            }
        }
    }
    if pure_array && count == len && len > 0 {
        let mut array = Vec::with_capacity(len);
        for i in 1..=len {
            array.push(to_json(lua, table.get::<Value>(i)?)?);
        }
        return Ok(Json::Array(array));
    }

    let mut object = Map::new();
    for pair in table.pairs::<Value, Value>() {
        let (key, value) = pair?;
        let key = match key {
            Value::String(s) => String::from_utf8_lossy(s.as_bytes().as_ref()).into_owned(),
            Value::Integer(i) => i.to_string(),
            Value::Number(f) => format!("{f}"),
            Value::Boolean(b) => b.to_string(),
            _ => return Err(mlua::Error::runtime("JSON object keys must be strings")),
        };
        if is_null_sentinel(&value) {
            object.insert(key, Json::Null);
            continue;
        }
        let value = match value {
            // nil values are simply absent from Lua tables — skip them like Friedl does.
            Value::Nil => continue,
            v => to_json(lua, v)?,
        };
        object.insert(key, value);
    }
    Ok(Json::Object(object))
}

/// JSON → Lua. Arrays become 1-based tables; nulls become the `json.null` sentinel.
pub fn to_lua(lua: &Lua, json: Json, null: &mlua::String) -> mlua::Result<Value> {
    Ok(match json {
        Json::Null => Value::String(null.clone()),
        Json::Bool(b) => Value::Boolean(b),
        Json::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Integer(i)
            } else {
                Value::Number(n.as_f64().unwrap_or(0.0))
            }
        }
        Json::String(s) => Value::String(lua.create_string(s)?),
        Json::Array(array) => {
            let table = lua.create_table()?;
            for (i, item) in array.into_iter().enumerate() {
                table.set(i + 1, to_lua(lua, item, null)?)?;
            }
            Value::Table(table)
        }
        Json::Object(object) => {
            let table = lua.create_table()?;
            for (key, item) in object {
                table.set(key, to_lua(lua, item, null)?)?;
            }
            Value::Table(table)
        }
    })
}

/// Invokes `JSON:encode(self, value)` shape: llcom calls it with the colon syntax,
/// so the module table itself arrives as the first argument and is skipped.
pub fn encode_call(module: &Table, value: Value) -> mlua::Result<String> {
    let encode: mlua::Function = module.get("encode")?;
    let out: mlua::String = encode.call((module.clone(), value))?;
    Ok(String::from_utf8_lossy(out.as_bytes().as_ref()).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json_table(lua: &Lua) -> Table {
        module(lua).unwrap()
    }

    #[test]
    fn encode_object_like_readme_sample() {
        let lua = Lua::new();
        let json = json_table(&lua);
        let t: Table = lua
            .load(r#"{key1 = "a", key2 = "b", key3 = "c"}"#)
            .eval()
            .unwrap();
        let out = encode_call(&json, Value::Table(t)).unwrap();
        let parsed: Json = serde_json::from_str(&out).unwrap();
        assert_eq!(
            parsed,
            serde_json::json!({"key1": "a", "key2": "b", "key3": "c"})
        );
    }

    #[test]
    fn arrays_numbers_nesting_and_null_roundtrip() {
        let lua = Lua::new();
        let globals = lua.globals();
        globals.set("JSON", json_table(&lua)).unwrap();
        let encoded: String = lua
            .load(r#"return JSON:encode({1, 2, 3.5, "x", nested = {ok = true, no = JSON.null}})"#)
            .eval()
            .unwrap();
        let parsed: Json = serde_json::from_str(&encoded).unwrap();
        // Mixed tables encode as objects with stringified keys (Friedl's rule).
        assert_eq!(
            parsed,
            serde_json::json!({"1": 1, "2": 2, "3": 3.5, "4": "x", "nested": {"ok": true, "no": null}})
        );
        let pure: Json = serde_json::from_str(
            &lua.load(r#"return JSON:encode({1, 2, 3})"#).eval::<String>().unwrap(),
        )
        .unwrap();
        assert_eq!(pure, serde_json::json!([1, 2, 3]));
        globals.set("encoded", encoded).unwrap();
        let back: String = lua
            .load(r#"local t = JSON:decode(encoded) return t["4"] .. tostring(t.nested.ok):upper()"#)
            .eval()
            .unwrap();
        assert_eq!(back, "xTRUE");
        let null_check: String = lua
            .load(r#"local t = JSON:decode(encoded) return tostring(t.nested.no == JSON.null)"#)
            .eval()
            .unwrap();
        assert_eq!(null_check, "true");
    }

    #[test]
    fn decode_rejects_garbage() {
        let lua = Lua::new();
        let globals = lua.globals();
        globals.set("JSON", json_table(&lua)).unwrap();
        let err = lua
            .load("return JSON:decode('{nope')")
            .exec()
            .unwrap_err();
        assert!(err.to_string().contains("decode failed"), "{err}");
    }
}
