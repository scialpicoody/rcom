# Licenses of the embedded Lua sources

`sys.lua`, `strings.lua` and `log.lua` are taken unmodified from llcom
(https://github.com/chenxuuu/llcom), `llcom/DefaultFiles/core_script/`, which
carries them from the openLuat (Luat) project:

- `sys.lua`, `strings.lua`, `log.lua`: **MIT**, © openLuat (headers inside the files).
- llcom itself is **Apache-2.0**; `head.lua` here is a Rust-runtime-adapted port
  of llcom's `core_script/head.lua` (differences documented in the file header).

`JSON` is **not** embedded: rcom implements the same `JSON:encode`/`JSON:decode`
surface natively over `serde_json` (see `../json.rs`).
