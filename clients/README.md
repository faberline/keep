# keep — generated clients

`keep spec gen` uses the shared `core/openapi-codegen` generator from faberline/core;
there is no Makefile, external generator, `node`, or `npx` dependency.

| Command | Output |
|---|---|
| `cargo run -q -p keep --bin keep -- spec gen --lang ts --out clients/ts` | TypeScript client |
| `cargo run -q -p keep --bin keep -- spec gen --lang py --out clients/py` | Python client |
| `cargo run -q -p keep --bin keep -- spec gen --lang rust --out clients/rust` | Rust client |

`codegen.toml` pins the default TypeScript, Python, and Rust contracts. Each
generated directory includes `.openapi-codegen.json`; pass `--target
<profile>` only for a deliberate compatibility override.
