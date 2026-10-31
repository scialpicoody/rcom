//! The `rcom.ui` script API (issue #24): the sandboxed plot table installed
//! next to the other builtins (docs/design.md, "Lua layer": "`rcom.ui` (log,
//! plot taps)").
//!
//! Shipped surface (v1, this ticket):
//!
//! ```lua
//! rcom.ui.add_point(channel, value)              -- plot `value` now
//! rcom.ui.add_point_at(channel, t_ms, value)     -- plot with explicit epoch-ms t
//! rcom.ui.clear_channel(channel)                 -- drop one channel's points
//! rcom.ui.clear()                                -- drop every channel
//! ```
//!
//! `channel` is any string ("temperature", "ch0", ...); values are numbers.
//! Points land in the process-global [`PlotStore`](super::store::PlotStore)
//! directly — one call per point, no bus hop — and the plot page samples the
//! store on its own tick, which is why this API is safe at wire data rates.
//!
//! A **decoder script** is any script defining a global
//! `decode(bytes, timestamp_ms)` function (see [`super::decoder`]); inside it,
//! `rcom.ui.add_point` is the tap. The table is process-global: a plain script
//! (script panel) may plot too — `rcom.ui` never touches the wire.

use mlua::Lua;

use super::store::global_store;

/// Installs the `rcom` global with its `ui` table (idempotent per VM; merges
/// into an existing `rcom` table — `rcom.net` installs the same global).
pub fn install(lua: &Lua) -> mlua::Result<()> {
    let globals = lua.globals();
    let rcom = match globals.get::<mlua::Table>("rcom") {
        Ok(table) => table,
        Err(_) => {
            let table = lua.create_table()?;
            globals.set("rcom", table.clone())?;
            table
        }
    };

    let ui = lua.create_table()?;
    ui.set(
        "add_point",
        lua.create_function(|_, (channel, value): (String, f64)| {
            global_store().add_point(&channel, value);
            Ok(())
        })?,
    )?;
    ui.set(
        "add_point_at",
        lua.create_function(|_, (channel, t_ms, value): (String, f64, f64)| {
            global_store().add_point_at(&channel, t_ms, value);
            Ok(())
        })?,
    )?;
    ui.set(
        "clear_channel",
        lua.create_function(|_, channel: String| {
            global_store().clear_channel(&channel);
            Ok(())
        })?,
    )?;
    ui.set(
        "clear",
        lua.create_function(move |_, ()| {
            global_store().clear();
            Ok(())
        })?,
    )?;

    rcom.set("ui", ui)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::bus::EventBus;
    use crate::core::lua::LuaRuntime;

    /// The table is exactly the documented surface (the sandboxed API stays
    /// narrow — no IO, no wire access from the plot table).
    #[test]
    fn surface_is_exactly_the_four_functions() {
        let lua = Lua::new();
        install(&lua).unwrap();
        let script = r#"
            local names = {}
            for name, _ in pairs(rcom.ui) do table.insert(names, name) end
            table.sort(names)
            return table.concat(names, ",")
        "#;
        let surface: String = lua.load(script).eval().unwrap();
        assert_eq!(surface, "add_point,add_point_at,clear,clear_channel");
    }

    /// `add_point` from a script VM lands in the global store — THE tap the
    /// decoders use.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn script_add_point_reaches_the_store() {
        let store = global_store();
        store.clear_channel("rcom-ui-test");
        let runtime = LuaRuntime::start(
            EventBus::new(),
            None,
            None,
            r#"
            for i = 1, 5 do rcom.ui.add_point("rcom-ui-test", i * 0.5) end
            "#,
        )
        .expect("plot script must run");

        assert!(!runtime.is_stopped());
        let names = store.channel_names();
        let (_, count) = names
            .iter()
            .find(|(name, _)| name == "rcom-ui-test")
            .cloned()
            .expect("channel created by the script");
        assert_eq!(count, 5);
        let snapshot = store.sample(1_000_000.0, 100);
        let series = snapshot
            .series
            .iter()
            .find(|series| series.name == "rcom-ui-test")
            .expect("series in snapshot");
        assert_eq!(series.points[4].1, 2.5);
        store.clear_channel("rcom-ui-test");
    }

    /// A decoder script's `decode(bytes, ts)` is drivable through the runtime
    /// helper and buffers state across calls (two chunks, one frame).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn decoder_contract_runs_across_chunk_boundaries() {
        let store = global_store();
        store.clear_channel("decoder-test");
        let runtime = LuaRuntime::start(
            EventBus::new(),
            None,
            None,
            r#"
            buf = ""
            function decode(bytes, timestamp_ms)
                buf = buf .. bytes
                while #buf >= 7 do
                    if buf:byte(1) == 0xAA and buf:byte(2) == 0x55 then
                        local value = (buf:byte(5) * 256 + buf:byte(6))
                        if value >= 32768 then value = value - 65536 end
                        rcom.ui.add_point_at("decoder-test", timestamp_ms, value)
                        buf = buf:sub(8)
                    else
                        buf = buf:sub(2)
                    end
                end
            end
            "#,
        )
        .expect("decoder script must run");
        assert!(runtime.has_function("decode"));

        let t0 = chrono::Utc::now().timestamp_millis() as f64;
        let frame = crate::core::plot::frames::build_frame(1, 1234);
        runtime.call_bytes_function("decode", &frame[..4], t0).expect("first half");
        // No point yet — the channel does not even exist (rings are created on
        // the first plotted point).
        assert!(
            !store.channel_names().iter().any(|(n, _)| n == "decoder-test"),
            "partial frame must not plot yet"
        );
        runtime.call_bytes_function("decode", &frame[4..], t0).expect("second half");
        let snapshot = store.sample(1_000_000.0, 10);
        let series = snapshot
            .series
            .iter()
            .find(|series| series.name == "decoder-test")
            .expect("frame completed across chunks");
        assert_eq!(series.points, vec![(t0, 1234.0)]);
        store.clear_channel("decoder-test");
    }

    /// A script error inside `decode` returns as an error (the host stops the
    /// decoder and reports once) instead of panicking the app.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn decoder_error_returns_not_panics() {
        let runtime = LuaRuntime::start(
            EventBus::new(),
            None,
            None,
            "function decode(bytes, ts) error('bad decoder', 0) end",
        )
        .expect("broken decoder must load");
        let error = runtime
            .call_bytes_function("decode", b"x", 1.0)
            .expect_err("decode must error");
        assert!(error.to_string().contains("bad decoder"), "{error}");
        runtime.stop();
    }

    /// `rcom.ui` merges into the same `rcom` global `rcom.net` installs (the
    /// install order must not clobber either).
    #[test]
    fn ui_merges_with_net_under_rcom() {
        let lua = Lua::new();
        crate::core::nettools::lua_api::install(&lua).unwrap();
        install(&lua).unwrap();
        let check = r#"
            return type(rcom.net.tcp_send) == "function"
               and type(rcom.ui.add_point) == "function"
        "#;
        let both: bool = lua.load(check).eval().unwrap();
        assert!(both, "rcom.net and rcom.ui must coexist");
    }
}
