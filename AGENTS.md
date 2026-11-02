# AGENTS.md

ug-lite is a low-memory drop-in binary that reimplements only `ug mcp-proxy` and `ug auth-token` in Rust. Everything it does not handle is forwarded to the original Python `ug`. [README.md](README.md) is the source of truth for behavior, exit codes, error codes, and what is not ported. Read it before making changes.

## Commands

Requires Rust 1.89 or later (`rust-version` in `Cargo.toml`; clippy flags uses of newer APIs).

```sh
cargo build                                # dev build
cargo build --release                      # release build (target/release/ug-lite)
cargo test                                 # unit tests + integration tests in tests/
cargo test <part of a name>                # run only tests whose name matches
cargo fmt --check                          # formatting
cargo clippy --all-targets -- -D warnings  # lint; warnings are errors
```

Before finishing a change, make `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` all pass. No CI runs tests on PRs, so check locally (`.github/workflows/pullfrog.yml` is for a manually started agent; `release.yml` runs the tests and then creates a release when a `vX.Y.Z` tag is pushed).

## Layout

- `src/main.rs`: the whole implementation (one file). To start reading, look at `main`, `parse_opts`, `fallback`, `auth_token`, `Auth`, `mcp_proxy`, `Proxy`, `client_message`, `rpc_kind`, and `read_sse`
- `mod tests` at the end of `src/main.rs`: unit tests. HTTP peers are local servers started with `mock` / `mock_held` / `mock_seen`. Each mock accepts only one connection
- `tests/`: integration tests that start the built binary

## Rules

- **Memory and startup time are the reason this exists.** Do not add dependencies. In particular, no async runtime (tokio and the like)
- **Do not use `eprintln!` / `println!`.** Writing to a pipe whose reader has closed panics in every build (the release build has `panic = "abort"`, so the whole process dies). Use `elog!` for stderr, and write to stdout in a way that does not turn write errors into panics
- **`mcp-proxy` stdout is for the MCP protocol.** Write nothing but JSON-RPC messages to it
- **Exit codes and error codes are a contract with callers.** When you change them, update both READMEs too. Also update them when you add something that is not ported
- **The README exists in English ([README.md](README.md)) and Japanese ([README.ja.md](README.ja.md)).** Every README change goes into both. README.md is the primary one; README.ja.md is its translation
- **Add tests.** Check new branches and parsers in `mod tests`, and process-level behavior such as exit codes and pipes in `tests/`. Do not write tests that go out to the network

## Running by hand

```sh
cargo build
(echo '{"jsonrpc":"2.0","id":1,"method":"ping"}'; sleep 1) |
  DATABRICKS_BEARER=dummy ./target/debug/ug-lite mcp-proxy --url http://127.0.0.1:9/mcp --host https://h
# stderr: ug mcp-proxy: upstream request failed: ...
# stdout: {"jsonrpc":"2.0","id":1,"error":{"code":-32000,...}}
```

Setting `DATABRICKS_BEARER` lets it start without calling the `databricks` CLI. The `sleep` works around a known bug: if stdin closes before the response, ug-lite exits without answering the in-flight request. Checking against a real workspace requires a `databricks` CLI login.
