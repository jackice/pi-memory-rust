# @jackice/pi-memory-rust

A Rust memory extension for Pi: Turso storage + Tantivy FTS + turbovec vectors + local ONNX embedding.

See the repository root `README.md` for full documentation (this package is the npm distribution entry).

## Layout

- `index.ts` — Pi extension entry (event bindings + /memory commands + LLM proxy)
- `src/client.ts` — JSON-RPC client + persistent process lifecycle management
- The native binary ships in per-platform leaf packages: `@jackice/pi-memory-rust-<platform>-<arch>` (declared in `optionalDependencies`)

The `0.1.18` npm release currently supports **macOS arm64 only**. npm will reject installation on other OS/CPU combinations; publish platform-matched leaf binaries before expanding this range.

## Environment variables

| Variable | Description |
|---|---|
| `PI_MEMORY_RUST_BINARY` | Explicit path to the Rust binary (overrides leaf package / bundled binary) |
| `PI_MEMORY_DATA_DIR` | Data directory (default `~/.local/share/pi-memory-rust`) |
| `PI_MEMORY_MODEL_PATH` | Embedding model directory (default `~/.cache/pi-memory-rust/model`) |
| `PI_MEMORY_MAX_OPEN_PROJECTS` | Soft limit for idle project runtimes (default `16`; only unshared runtimes are evicted, so active projects can temporarily exceed the limit) |

Long maintenance commands (`prepare-model`, `migrate`, `rebuild`, `rebuild-embeddings`, `clear`, and `purge`) start a server job and poll its status. If the command wait expires, `/memory job <id>` resumes polling while that server process is still running. Clearing every stored project still requires `/memory clear all confirm`; its per-project outcomes are retained in the job details.

## Install

```bash
pi install npm:@jackice/pi-memory-rust
```
