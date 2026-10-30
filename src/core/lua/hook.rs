//! The TX pre-send hook seam (docs/design.md: "TX path always goes through the
//! session's Lua pre-send hook") — the part of llcom's send conversion this
//! ticket provides; ticket #19 wires the editor and the session write path.
//!
//! Mechanism (llcom parity): a send conversion runs in its own one-shot VM with
//! `runType = "send"`, the bytes to send in the `uartData` global, and whatever
//! the chunk `return`s goes on the wire (`return uartData`, `return
//! uartData:fromHex()`, ...). A 3 s instruction watchdog (llcom's `runMaxSeconds`)
//! bounds runaway conversion scripts. [`TxHook`] is the object #19 installs on a
//! session; until then nothing in the app calls it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use mlua::{Lua, Value};

use crate::core::bus::EventBus;

use super::runtime::{SEND_VM_TIMEOUT, arm_stop_hook_on};
use super::{json, sandbox, scripts};

/// The outcome of a send conversion: the transformed bytes (empty when the
/// script returned nothing — llcom sends nothing in that case). Runs anywhere
/// (no async context required), bounded by the 3 s watchdog.
pub fn run_send_convert(script: &str, input: &[u8]) -> mlua::Result<Vec<u8>> {
    let lua = Lua::new();
    sandbox::apply(&lua)?;
    install_send_preloads(&lua)?;

    // The llcom watchdog: `runMaxSeconds = 3` for send scripts, enforced by a
    // stop flag the instruction hook checks (head.lua's debug.sethook role).
    // A plain parked thread keeps this usable from the sync TX path; it exits
    // early via `done` when the conversion finishes first (never a busy wait:
    // 25 ms granularity, only while a conversion is actually running).
    let stop = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let watchdog_stop = Arc::clone(&stop);
    let watchdog_done = Arc::clone(&done);
    let watchdog = std::thread::spawn(move || {
        let deadline = std::time::Instant::now() + SEND_VM_TIMEOUT;
        while std::time::Instant::now() < deadline {
            if watchdog_done.load(Ordering::Relaxed) {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        watchdog_stop.store(true, Ordering::Relaxed);
    });
    arm_stop_hook_on(&lua.current_thread(), Arc::clone(&stop));

    let result = (|| -> mlua::Result<Vec<u8>> {
        lua.globals().set("runType", "send")?;
        lua.load(scripts::HEAD_LUA).set_name("=head").exec()?;
        lua.globals().set("uartData", lua.create_string(input)?)?;
        let value = lua.load(script).set_name("=send_convert").eval::<Value>()?;
        Ok(match value {
            Value::Nil => Vec::new(),
            Value::String(s) => s.as_bytes().to_vec(),
            Value::Integer(i) => i.to_string().into_bytes(),
            Value::Number(f) => format!("{f}").into_bytes(),
            other => {
                return Err(mlua::Error::runtime(format!(
                    "send script must return a string, got {}",
                    other.type_name()
                )))
            }
        })
    })();
    done.store(true, Ordering::Relaxed);
    let _ = watchdog.join();
    result
}

/// A pre-send hook installed on a session: transforms every outgoing chunk
/// (`return uartData:fromHex()` shape). Stub for #19 to adopt.
pub struct TxHook {
    script: String,
}

impl TxHook {
    pub fn new(script: impl Into<String>) -> Self {
        Self {
            script: script.into(),
        }
    }

    /// Transforms `input` through the hook script. Errors carry the Lua failure
    /// for the caller to surface (contract §2.4 `SessionError::Hook`).
    pub fn apply(&self, input: &[u8], _bus: &EventBus) -> mlua::Result<Vec<u8>> {
        run_send_convert(&self.script, input)
    }
}

fn install_send_preloads(lua: &Lua) -> mlua::Result<()> {
    // Send VMs only get the string extensions (llcom's head.lua returns before
    // loading log/sys for runType == "send").
    let preload: mlua::Table = lua
        .globals()
        .get::<mlua::Table>("package")?
        .get("preload")?;
    preload.set(
        "strings",
        lua.load(scripts::STRINGS_LUA).set_name("=strings").into_function()?,
    )?;
    // package.preload holds loader FUNCTIONS: wrap the ready-made JSON table.
    let json_module = json::module(lua)?;
    preload.set(
        "JSON",
        lua.create_function(move |_lua, ()| Ok(json_module.clone()))?,
    )?;
    Ok(())
}

/// How long a conversion may run before the watchdog errors it. Exposed for the
/// timeout test (real value: llcom's 3 s; see [`SEND_VM_TIMEOUT`]).
pub fn send_vm_timeout() -> Duration {
    SEND_VM_TIMEOUT
}

#[cfg(test)]
mod tests {
    use super::*;

    /// llcom README send conversion #2: hex input becomes raw bytes.
    #[test]
    fn send_convert_from_hex() {
        let out = run_send_convert(scripts::SEND_CONVERT_HEX, b"313233").unwrap();
        assert_eq!(out, b"123");
    }

    /// The default conversion passes bytes through unchanged.
    #[test]
    fn send_convert_default_passthrough() {
        let out = run_send_convert(scripts::SEND_CONVERT_DEFAULT, b"abc\x01").unwrap();
        assert_eq!(out, b"abc\x01");
    }

    /// The README's JSON send conversion (require("JSON") + uartData:split(",")).
    #[test]
    fn send_convert_readme_json_sample() {
        let script = r#"
            json = require("JSON")
            t = uartData:split(",")
            return json:encode({
                key1 = t[1],
                key2 = t[2],
                key3 = t[3],
            })
        "#;
        let out = run_send_convert(script, b"a,b,c").unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            parsed,
            serde_json::json!({"key1": "a", "key2": "b", "key3": "c"})
        );
    }

    /// The TxHook stub #19 will install: transform path proven end to end.
    #[tokio::test]
    async fn tx_hook_transforms_bytes() {
        let bus = EventBus::new();
        let hook = TxHook::new("return uartData:fromHex()");
        let out = hook.apply(b"30313233", &bus).unwrap();
        assert_eq!(out, b"0123");

        // A broken hook reports the Lua error to the caller (contract §2.4
        // SessionError::Hook — send rejected), never panics.
        let bad = TxHook::new("error('bad hook', 0)");
        let err = bad.apply(b"x", &bus).unwrap_err();
        assert!(err.to_string().contains("bad hook"), "{err}");
    }

    /// A runaway conversion dies at the llcom 3 s watchdog, not forever.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn send_convert_watchdog_bounds_runaway() {
        let t0 = std::time::Instant::now();
        let result = tokio::task::spawn_blocking(|| {
            run_send_convert("while true do end", b"").map(|_| ())
        })
        .await
        .unwrap();
        assert!(result.is_err(), "watchdog must kill the runaway script");
        assert!(
            t0.elapsed() >= send_vm_timeout() && t0.elapsed() < send_vm_timeout() * 2,
            "watchdog fired at {:?}",
            t0.elapsed()
        );
    }

    /// Conversion scripts have no uart/sys/log — llcom's head returns before
    /// loading them for runType == "send".
    #[test]
    fn send_convert_vm_is_minimal() {
        let out = run_send_convert("return type(sys)", b"").unwrap();
        assert_eq!(out, b"nil", "sys must be absent in send VMs");
        let out = run_send_convert("return type(log)", b"").unwrap();
        assert_eq!(out, b"nil", "log must be absent in send VMs");
        // But strings (fromHex/split) and JSON are there.
        let out = run_send_convert("return type(uartData.fromHex)", b"31").unwrap();
        assert_eq!(out, b"function");
    }
}
