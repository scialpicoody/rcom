//! The `rcom.ports` script API (ticket #35): cross-session enumeration and
//! sends — the beyond-llcom axis (docs/design.md, "Lua layer": "`rcom.ports`
//! (enumerate, cross-session send)").
//!
//! Shipped surface (v1, this ticket):
//!
//! ```lua
//! local list, err = rcom.ports.list()            -- array of {name, session_id, open}
//! local ok = rcom.ports.send(session_id, data)   -- the target's TX path, hook applies
//! local ok = rcom.ports.send_raw(session_id, data) -- the target's TX path, hooks bypassed
//! ```
//!
//! Semantics (the part worth writing down):
//!
//! - `list()` enumerates the session table: one entry per session — `name`
//!   (the port), `session_id` (the address every other call takes), `open`
//!   (whether the session is live; a dead session lingers until the manager
//!   replaces or closes it). On failure (no manager, or the session table is
//!   momentarily locked by an async operation) it returns `(nil, reason)`.
//! - **Hooks belong to sessions, not scripts.** `send` drives the *target
//!   session's* TX path — exactly the path the UI's send bar uses for that
//!   session — so the target's own pre-send hook applies to the bytes. The
//!   sender's hook is irrelevant: the sender is a VM, not a session. A script
//!   that wants to transform first converts its own bytes (or calls its own
//!   session's `send` — cross-sending to yourself is allowed).
//! - `send_raw` is the llcom `apiSendUartData` parity path for cross-sends:
//!   the bytes are already final, so no hook rewrites them.
//! - Both sends are fire-and-forget by the `api*` contract (they never block
//!   the Lua VM on the port): `true` means the bytes were dispatched onto the
//!   target's TX path; the outcome (a `TX` data row, or an error row) lands in
//!   the *target* session's log — design-ui §5.2 `CrossSend`: cross-sends
//!   render as normal TX rows on the target session, never on the sender's.
//!   `false` means nothing was dispatched (no manager, unknown session, or no
//!   runtime to dispatch on).
//!
//! The table installs next to `rcom.net`/`rcom.ui` (merges into the same
//! `rcom` global, idempotent per VM); it resolves the process-global
//! [`PortManager`](crate::core::port::manager::PortManager) lazily, so VMs
//! without a running manager still load (the calls then report failure).

use std::sync::Arc;

use mlua::{Lua, Value};

use crate::core::port::manager::PortManager;

/// Installs the `rcom` global's `ports` table (idempotent per VM).
pub fn install(lua: &Lua) -> mlua::Result<()> {
    let ports = lua.create_table()?;

    ports.set(
        "list",
        lua.create_function(
            |lua, ()| -> mlua::Result<(Value, Option<String>)> {
                let Some(manager) = PortManager::global() else {
                    return Ok((Value::Nil, Some("no port manager is running".into())));
                };
                match manager.try_sessions_snapshot() {
                    Some(sessions) => {
                        let table = lua.create_table()?;
                        for (index, session) in sessions.iter().enumerate() {
                            let entry = lua.create_table()?;
                            entry.set("name", session.port.clone())?;
                            entry.set("session_id", session.session_id)?;
                            entry.set("open", session.open)?;
                            table.set(index + 1, entry)?;
                        }
                        Ok((Value::Table(table), None))
                    }
                    None => Ok((
                        Value::Nil,
                        Some("the session table is busy; retry shortly".into()),
                    )),
                }
            },
        )?,
    )?;

    ports.set(
        "send",
        lua.create_function(|_, (session_id, data): (i64, mlua::String)| {
            let bytes = data.as_bytes().as_ref().to_vec();
            Ok(dispatch(session_id, bytes, false))
        })?,
    )?;

    ports.set(
        "send_raw",
        lua.create_function(|_, (session_id, data): (i64, mlua::String)| {
            let bytes = data.as_bytes().as_ref().to_vec();
            Ok(dispatch(session_id, bytes, true))
        })?,
    )?;

    let globals = lua.globals();
    let rcom = match globals.get::<mlua::Table>("rcom") {
        Ok(table) => table,
        Err(_) => {
            let table = lua.create_table()?;
            globals.set("rcom", table.clone())?;
            table
        }
    };
    rcom.set("ports", ports)?;
    Ok(())
}

/// Dispatches a cross-session send (fire-and-forget): `true` when the bytes
/// were handed to the target session's TX path. `raw` bypasses the target's
/// pre-send hook (already-final bytes); otherwise the hook applies — hooks
/// belong to sessions, not scripts (see the module docs).
fn dispatch(session_id: i64, bytes: Vec<u8>, raw: bool) -> bool {
    let Some(manager) = PortManager::global() else {
        return false;
    };
    if session_id <= 0 {
        return false;
    }
    // Unknown sessions fail before dispatching (an honest `false` for a stale
    // id); an unavailable snapshot dispatches anyway — the outcome row will
    // tell (best effort beats a lock-tied Lua VM).
    if let Some(sessions) = manager.try_sessions_snapshot() {
        if !sessions.iter().any(|session| session.session_id as i64 == session_id) {
            return false;
        }
    }
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn(async move {
                let manager: Arc<PortManager> = manager;
                let outcome = if raw {
                    manager.write_raw_for(session_id as u64, bytes, false).await
                } else {
                    manager.write_for(session_id as u64, bytes).await
                };
                if let Err(error) = outcome {
                    tracing::warn!(session = session_id, "rcom.ports send failed: {error}");
                }
            });
            true
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::bus::{Direction, EventBus};
    use crate::core::lua::LuaRuntime;
    use crate::core::port::factory::{BackendFactory, MockMode};
    use crate::core::port::testing::EventTap;
    use crate::core::port::PortConfig;

    /// The global manager is process state: the tests that re-point it hold
    /// this for their whole body (a VM's dispatch lands up to ~100 ms after
    /// `init_global`, so overlapping tests would race on whose manager serves).
    static GLOBAL_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn global_guard() -> std::sync::MutexGuard<'static, ()> {
        GLOBAL_SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// THE cross-session AC (ticket #35), at the API level: a script on
    /// session A enumerates the session table and sends through session B —
    /// the bytes leave on **B's** TX path (a TX row on B, none on A), and B's
    /// own hook applies to them.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn a_session_script_enumerates_and_cross_sends() {
        let _serial = global_guard();
        let bus = EventBus::new();
        let tap = EventTap::new(&bus);
        let backend = BackendFactory::with_mode(MockMode::All);
        let manager = Arc::new(PortManager::new(Arc::new(backend), bus.clone()));
        manager.init_global();

        let a = manager.open("MOCK0", PortConfig::new(115_200)).await.expect("open echo");
        let b = manager.open("MOCK1", PortConfig::new(115_200)).await.expect("open noisy");

        // Session A's script: list, find MOCK1, cross-send through it. B's own
        // hook rewrites hex text to bytes — proving the TARGET's hook applies.
        manager.set_hook_for(
            b,
            Some(crate::core::lua::TxHook::new("return uartData:fromHex()")),
        );
        let script = r#"
            sys.taskInit(function()
                sys.wait(100)
                local list, err = rcom.ports.list()
                assert(list, err or "list failed")
                assert(#list == 2, "expected two sessions, got " .. #list)
                local target
                for _, p in ipairs(list) do
                    log.info("ports", p.name, p.session_id, tostring(p.open))
                    if p.name == "MOCK1" and p.open then target = p.session_id end
                end
                assert(target, "no MOCK1 session in the list")
                local ok = rcom.ports.send(target, "43524f5353")  -- "CROSS" as hex
                assert(ok, "cross send was not dispatched")
            end)
        "#;
        let vm = LuaRuntime::start(bus.clone(), Some(a), None, script)
            .expect("cross script must start");
        // The bytes on the wire of B are the hook-converted payload: the
        // target session's hook applied (§5.2 CrossSend) — and B's TX row is
        // the only one the script produced (A's TX path stayed silent).
        let tx = tap.next_data(b, Direction::Tx, std::time::Duration::from_secs(10));
        assert_eq!(tx.bytes.as_slice(), b"CROSS", "the target's hook must apply to cross-sends");
        assert!(tx.hooked, "the rewritten cross-send carries the hooked flag");

        let _ = vm;
        manager.close_all().await;
    }

    /// `send_raw` bypasses the target's hook (llcom `apiSendUartData` parity
    /// for cross-session sends).
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cross_send_raw_bypasses_the_target_hook() {
        let _serial = global_guard();
        let bus = EventBus::new();
        let tap = EventTap::new(&bus);
        let backend = BackendFactory::with_mode(MockMode::All);
        let manager = Arc::new(PortManager::new(Arc::new(backend), bus.clone()));
        manager.init_global();

        let a = manager.open("MOCK0", PortConfig::new(115_200)).await.expect("open echo");
        let b = manager.open("MOCK1", PortConfig::new(115_200)).await.expect("open noisy");
        manager.set_hook_for(
            b,
            Some(crate::core::lua::TxHook::new("return 'REWRITTEN'")),
        );

        let script = r#"
            sys.taskInit(function()
                sys.wait(100)
                assert(rcom.ports.send_raw(_TARGET_, "raw-bytes"), "raw cross send failed")
            end)
        "#
        .replace("_TARGET_", &b.to_string());
        let vm = LuaRuntime::start(bus.clone(), Some(a), None, &script)
            .expect("raw cross script must start");
        let tx = tap.next_data(b, Direction::Tx, std::time::Duration::from_secs(10));
        assert_eq!(tx.bytes.as_slice(), b"raw-bytes", "send_raw must bypass the hook");

        let _ = vm;
        manager.close_all().await;
    }

    /// VM isolation (per-session VMs): two VMs bound to different sessions
    /// each see the session table and send through their own session; a
    /// script's writes land only on its own session's TX path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_session_vms_are_isolated_and_self_addressed() {
        let _serial = global_guard();
        let bus = EventBus::new();
        let tap = EventTap::new(&bus);
        let backend = BackendFactory::with_mode(MockMode::All);
        let manager = Arc::new(PortManager::new(Arc::new(backend), bus.clone()));
        manager.init_global();

        let a = manager.open("MOCK0", PortConfig::new(115_200)).await.expect("open echo");
        let b = manager.open("MOCK1", PortConfig::new(115_200)).await.expect("open noisy");

        // Each VM sends on its OWN session via apiSendUartData (the sink is
        // session-bound) and stamps its session id.
        let script = r#"
            sys.taskInit(function()
                sys.wait(100)
                apiSendUartData("from-" .. "_TAG_")
            end)
        "#;
        let script_a = script.replace("_TAG_", "a");
        let script_b = script.replace("_TAG_", "b");
        let sink = |session: u64| {
            Some(Arc::new(SessionSinkStub(session, manager.clone())) as Arc<dyn crate::core::lua::UartSink>)
        };
        let vm_a = LuaRuntime::start(bus.clone(), Some(a), sink(a), &script_a).expect("vm a");
        let vm_b = LuaRuntime::start(bus.clone(), Some(b), sink(b), &script_b).expect("vm b");
        // Both sends race (independent VM timers), so the waits must not be
        // sequential — b's Tx can land while waiting for a's and be skipped.
        // One drain loop collects both.
        let (mut got_a, mut got_b) = (false, false);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline && !(got_a && got_b) {
            match tap.rx.recv_timeout(std::time::Duration::from_millis(200)) {
                Ok(crate::core::bus::Event::Data(data)) if data.direction == Direction::Tx => {
                    got_a |= data.session_id == a && data.bytes.as_slice() == b"from-a";
                    got_b |= data.session_id == b && data.bytes.as_slice() == b"from-b";
                }
                Ok(_) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                Err(error) => panic!("event tap died: {error}"),
            }
        }
        assert!(got_a, "VM a's send never landed on session a's TX path");
        assert!(got_b, "VM b's send never landed on session b's TX path");

        manager.close_all().await;
    }

    /// A session-bound sink stub (the driver's `ScriptSink` shape): raw writes
    /// to one fixed session.
    struct SessionSinkStub(u64, Arc<PortManager>);

    impl crate::core::lua::UartSink for SessionSinkStub {
        fn send_uart(&self, bytes: Vec<u8>) -> bool {
            let manager = Arc::clone(&self.1);
            let session = self.0;
            tokio::spawn(async move {
                let _ = manager.write_raw_for(session, bytes, false).await;
            });
            true
        }
    }

    /// Sends into unknown or non-address sessions report `false` instead of
    /// throwing; `list()` fails soft (either a table or `(nil, reason)`) —
    /// deterministic even when another test's manager holds the process
    /// global (session ids are unique per process, so 999999 never exists).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn list_and_send_fail_soft() {
        let bus = EventBus::new();
        let vm = LuaRuntime::start(
            bus,
            None,
            None,
            r#"
                local list, err = rcom.ports.list()
                if list == nil then
                    assert(type(err) == "string", "list must fail soft with a reason")
                else
                    assert(type(list) == "table", "list must return a table")
                end
                assert(rcom.ports.send(999999, "x") == false, "unknown sessions refuse")
                assert(rcom.ports.send_raw(999999, "x") == false, "unknown sessions refuse")
                assert(rcom.ports.send(0, "x") == false, "session 0 is not an address")
                assert(rcom.ports.send_raw(-1, "x") == false, "negative ids refuse")
            "#,
        )
        .expect("soft-fail script must start");
        let _ = vm;
    }

    /// The snapshot shape: sorted by session id, alive entries marked open,
    /// dead entries (killed mock sessions) marked not-open until replaced.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn snapshot_lists_sessions_with_liveness() {
        let bus = EventBus::new();
        let _tap = EventTap::new(&bus);
        let backend = BackendFactory::with_mode(MockMode::All);
        let manager = Arc::new(PortManager::new(Arc::new(backend), bus.clone()));

        let a = manager.open("MOCK0", PortConfig::new(115_200)).await.expect("open");
        let b = manager.open("MOCK1", PortConfig::new(115_200)).await.expect("open");
        let sessions = manager.try_sessions_snapshot().expect("snapshot");
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_id, a.min(b));
        assert!(sessions.iter().all(|session| session.open), "both live");
        assert_eq!(
            sessions.iter().map(|session| session.port.as_str()).collect::<Vec<_>>(),
            vec!["MOCK0", "MOCK1"],
        );

        manager.close_session(a).await;
        let sessions = manager.try_sessions_snapshot().expect("snapshot");
        assert_eq!(sessions.len(), 1, "closed sessions leave the table");
        assert_eq!(sessions[0].session_id, b);

        manager.close_all().await;
    }
}
