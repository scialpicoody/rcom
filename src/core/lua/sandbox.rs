//! The script sandbox: prunes the standard libraries mlua's *safe* mode still
//! loads (`ALL_SAFE` excludes only `debug`/FFI — `io` and the dangerous half of
//! `os` are present until this pass runs).
//!
//! Blocked, and why:
//! - `io` (whole library): filesystem access; llcom scripts don't need it.
//! - `os.execute`, `os.exit`, `os.remove`, `os.rename`, `os.tmpname`, `os.getenv`,
//!   `os.setlocale`: process/FS/environment escape hatches. `os.time`/`os.date`/
//!   `os.clock` stay (llcom scripts log timestamps with them).
//! - `debug`: not loaded at all (mlua safe mode). llcom's `head.lua` used
//!   `debug.sethook` for its timeout — rcom enforces stops from Rust instead.
//! - `require`: preload-only. `package.path`/`cpath` are emptied and the
//!   `searchers`/`preload` loaders beyond the preload one are removed, so a
//!   script can load only the modules the host embedded (strings/log/sys/JSON).
//! - `load`/`loadstring`: kept (llcom's `cmd` channel needs `load`), but binary
//!   chunks (bytecode, a known Lua-injection vector) are rejected by a wrapper.
//! - `_G` stays writable on purpose: llcom scripts assign globals freely
//!   (`uartReceive = ...`, `runMaxSeconds = ...`).

use mlua::Lua;

/// The bytecode signature Lua's `load` accepts behind the safe-mode wrapper.
const LUA_SIGNATURE: &[u8] = b"\x1bLua";

pub fn apply(lua: &Lua) -> mlua::Result<()> {
    let globals = lua.globals();

    // io: gone entirely.
    globals.set("io", mlua::Value::Nil)?;

    // os: keep the clock/date/time trio only.
    if let Ok(os) = globals.get::<mlua::Table>("os") {
        for name in [
            "execute",
            "exit",
            "remove",
            "rename",
            "tmpname",
            "getenv",
            "setlocale",
            "difftime",
        ] {
            os.set(name, mlua::Value::Nil)?;
        }
    }

    // debug: mlua safe mode never loads it; make sure it cannot reappear.
    globals.set("debug", mlua::Value::Nil)?;

    // require: preload-only. Keep package.preload, drop every on-disk searcher.
    let package: mlua::Table = globals.get("package")?;
    package.set("path", "")?;
    package.set("cpath", "")?;
    let searchers: mlua::Table = package.get("searchers")?;
    // searchers[1] is the preload loader; everything else touches the filesystem
    // (or C modules) — gone.
    for i in (2..=searchers.raw_len()).rev() {
        searchers.set(i, mlua::Value::Nil)?;
    }
    searchers.set("n", 1)?;

    // load: reject binary chunks (bytecode), keep text sources.
    let load_wrapper = lua.create_function(|lua, (source, chunkname): (mlua::String, Option<String>)| {
        if source.as_bytes().starts_with(LUA_SIGNATURE) {
            return Err(mlua::Error::runtime(
                "loading bytecode is not allowed in the sandbox",
            ));
        }
        let mut chunk = lua.load(source.as_bytes().to_vec());
        if let Some(name) = chunkname {
            chunk = chunk.set_name(name);
        }
        chunk.into_function()
    })?;
    globals.set("load", load_wrapper)?;
    // dofile/require of files can no longer work; drop the dangling references.
    globals.set("dofile", mlua::Value::Nil)?;
    globals.set("loadstring", mlua::Value::Nil)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sandboxed() -> Lua {
        let lua = Lua::new();
        apply(&lua).unwrap();
        lua
    }

    #[test]
    fn blocks_io_library() {
        let lua = sandboxed();
        assert!(lua.globals().get::<Option<mlua::Table>>("io").unwrap().is_none());
        assert!(
            lua.load("io.open").exec().is_err(),
            "io must be unreachable"
        );
    }

    #[test]
    fn blocks_os_exit_and_friends_keeps_time() {
        let lua = sandboxed();
        assert!(lua.load("os.execute").exec().is_err());
        assert!(lua.load("os.exit").exec().is_err());
        assert!(lua.load("os.remove").exec().is_err());
        assert!(lua.load("os.getenv").exec().is_err());
        // the ones llcom scripts use stay
        let now: i64 = lua.load("os.time()").eval().unwrap();
        assert!(now > 1_700_000_000);
        let _: f64 = lua.load("os.clock()").eval().unwrap();
        let _d: String = lua.load(r#"os.date("%Y")"#).eval().unwrap();
    }

    #[test]
    fn blocks_debug_library() {
        let lua = sandboxed();
        assert!(lua.load("debug.sethook").exec().is_err());
        assert!(lua.load("debug.getinfo").exec().is_err());
    }

    #[test]
    fn require_is_preload_only() {
        let lua = sandboxed();
        let preload: mlua::Table = lua
            .globals()
            .get::<mlua::Table>("package")
            .unwrap()
            .get("preload")
            .unwrap();
        preload
            .set(
                "mine",
                lua.create_function(|_, ()| Ok("works"))
                    .unwrap(),
            )
            .unwrap();
        let ok: String = lua.load("require('mine')").eval().unwrap();
        assert_eq!(ok, "works");
        // Anything else has no on-disk fallback.
        assert!(lua.load("require('does_not_exist')").exec().is_err());
    }

    #[test]
    fn load_rejects_bytecode_but_keeps_text() {
        let lua = sandboxed();
        let out: String = lua
            .load("return load('return 1+1')()")
            .eval()
            .unwrap();
        assert_eq!(out, "2");
        // A minimal bytecode-header source must be refused at load time.
        let err = lua
            .load("return load('\\27Lua40\\0\\0\\0')")
            .exec()
            .unwrap_err();
        assert!(err.to_string().contains("bytecode"), "{err}");
    }
}
