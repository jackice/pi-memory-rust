# @jackice/pi-memory-rust

A Rust memory extension for Pi: Turso storage + Tantivy FTS + turbovec vectors + local ONNX embedding.

See the repository root `README.md` for full documentation (this package is the npm distribution entry).

## Layout

- `index.ts` — Pi extension entry (event bindings + /memory commands + LLM proxy)
- `src/client.ts` — JSON-RPC client + persistent process lifecycle management
- The native binary ships in per-platform leaf packages: `@jackice/pi-memory-rust-<platform>-<arch>` (declared in `optionalDependencies`)

## Environment variables

| Variable | Description |
|---|---|
| `PI_MEMORY_RUST_BINARY` | Explicit path to the Rust binary (overrides leaf package / bundled binary) |
| `PI_MEMORY_DATA_DIR` | Data directory (default `~/.local/share/pi-memory-rust`) |
| `PI_MEMORY_MODEL_PATH` | Embedding model directory (default `~/.cache/pi-memory-rust/model`) |

## Install

```bash
pi install npm:@jackice/pi-memory-rust
```
