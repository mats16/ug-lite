# ug-lite

English | [日本語](README.ja.md)

A low-memory drop-in binary that reimplements, in Rust, only the two subcommands of the [Unity Gateway](https://github.com/databricks/unity-gateway) `ug` command that coding agents start every time.

| Subcommand | Purpose |
|---|---|
| `ug mcp-proxy --url <URL> [--host] [--profile] [--use-pat]` | Bridges stdio MCP to a Databricks streamable-HTTP MCP server, injecting a token into every request |
| `ug auth-token [--host] [--profile] [--use-pat] [--force-refresh] [--browser-login]` | Prints a token for Claude Code's `apiKeyHelper` or Codex's auth command |

Every other subcommand or flag (`configure`, `--version`, `--client-id`, and so on) is handed to the original Python `ug` unchanged with `exec`.

For `ug-lite --help` alone, ug-lite first prints its version, the commands it handles, and the path of the `ug` it forwards to on stdout, then the original `ug` help (subcommand help such as `ug-lite configure --help` stays the original `ug` help). `ug-lite --version` (`-V`) is forwarded to the original `ug` with nothing added, because launchers read its output on every start.

```text
$ ug-lite --help
ug-lite 0.1.0 runs these commands itself:
  mcp-proxy   Bridge stdio MCP to a Databricks MCP server
  auth-token  Print a Databricks token for agent auth helpers
Everything else goes to the original ug:
  /Users/you/.local/bin/ug

 Usage: ug [OPTIONS] COMMAND [ARGS]...
 ...
```

## Measurements (macOS / Apple Silicon, initialize + tools/list against `system.ai.dbsql`)

| | Physical footprint | RSS | Time to initialize |
|---|---|---|---|
| `ug` (Python) | 68 MB | 90 MB | 0.84 s |
| `ug-lite` | 3.9 MB | 9.7 MB | 0.2 s |

`auth-token` goes from 0.28 s to 0.07 s (almost all of it is `databricks auth token` itself).

## Install

Installs the latest release binary (macOS arm64, Linux x86_64 / arm64) to `~/.local/bin/ug-lite`. The original `ug` (`~/.local/bin/ug`) is left untouched.

```sh
curl -fsSL https://raw.githubusercontent.com/mats16/ug-lite/main/install.sh | sh
```

To install to `/usr/local/bin/ug-lite`, set `UG_LITE_INSTALL_DIR` (the installer uses `sudo` itself when the directory is not writable).

```sh
curl -fsSL https://raw.githubusercontent.com/mats16/ug-lite/main/install.sh | UG_LITE_INSTALL_DIR=/usr/local/bin sh
```

## Usage

In your agent settings, use `ug-lite` instead of `ug`. The arguments are the same as `ug`. Example: Codex `~/.codex/config.toml`

```toml
[mcp_servers.slack]
command = "/Users/you/.local/bin/ug-lite"
args = ["mcp-proxy", "--url", "https://<workspace>/ai-gateway/mcp-services/system.ai.slack", "--host", "https://<workspace>"]
```

Commands such as `ug configure` and `ug claude` write the absolute path of `ug` into the settings, so change it back to `ug-lite` after running them.

Commands ug-lite does not handle are forwarded to `$UG_LITE_FALLBACK` (the absolute path of the original `ug`; `~` is not expanded). If it is unset, ug-lite uses `~/.local/bin/ug` (where `ug` itself installs). It does not search PATH, so it works even when the launcher (a GUI app, for example) has no `~/.local/bin` on its PATH. If you installed `ug` somewhere else, set `UG_LITE_FALLBACK`. ug-lite reads this variable every time it starts, so set it in the environment that starts ug-lite, not at install time. When an agent starts it, the reliable place is the `env` of that agent's settings (GUI apps do not read shell startup files). Example for Codex: `env = { UG_LITE_FALLBACK = "/opt/ug/bin/ug" }` in `[mcp_servers.slack]`. If the target is missing, not executable, or ug-lite itself, ug-lite prints the reason to stderr and exits 2. ug-lite binaries never end up calling each other in a loop.

### Upgrading from v0.1.0

Rerun the installer with the same `UG_LITE_INSTALL_DIR` you used for v0.1.0 (none if you did not set one), and ug-lite is replaced with the new version.

The forwarding target changed from "the next `ug` on PATH" to "`$UG_LITE_FALLBACK`, or else `~/.local/bin/ug`". If the original `ug` is somewhere other than `~/.local/bin/ug`, set `UG_LITE_FALLBACK` as described above. Without it, forwarded commands exit 2.

If you followed the v0.1.0 instructions and put the `~/.ug-lite/bin/ug` link first on PATH, `ug` still runs ug-lite. To make `ug` the original `ug` again, remove that link and the PATH setting, and use `ug-lite` directly in your agent settings.

## Same behavior as the original

- Uses the current workspace, `profile`, and `use_pat` in `~/.ucode/state.json` as defaults
- Prefers `DATABRICKS_BEARER` and `DATABRICKS_BEARER_COMMAND`; with `--use-pat`, uses the PAT in `~/.databrickscfg`
- Gets tokens with `databricks auth token --output json` and reuses them in the process until 5 minutes before they expire
- Retries on token cache lock contention. With `--browser-login`, serializes on a lock file and then logs in through the browser
- If it cannot get a token at startup, it prints a message to stderr and exits 2 (so the agent does not wait for its startup timeout). A failure after startup only becomes an error response to that request, and it keeps working after you log in again
- Handles the MCP session (`mcp-session-id`, `mcp-protocol-version`) and both JSON and SSE responses. Sends DELETE for the session when stdin closes
- Always returns exactly one well-formed response to every request. Messages from upstream are forwarded only after checking that they are JSON-RPC as the MCP schema defines it (`result` is an object, `error` has an integer `code` and a string `message`). SSE is parsed as the WHATWG spec defines it, and LF, CRLF, and CR line endings are all accepted. stdin accepts the same line endings
- The error codes for forwarding failures are ug-lite's own contract and are not guaranteed to match the MCP SDK exactly. They are chosen to be the same values as the SDK, though

  | Situation | Code |
  |---|---|
  | Cannot connect upstream, SSE ended before the response | -32000 |
  | 202, 3xx, or an unexpected content-type for a request | -32600 |
  | 404 with a session (`Session terminated`) / 404 without a session | -32600 / -32601 |
  | Response cannot be read as JSON-RPC | -32700 |
  | Other HTTP errors (if the body is a valid JSON-RPC error, its code is forwarded as is) | -32603 |
  | Cannot get a token during a session (the process does not exit) | -32603 |
- TLS uses the OS trust store, as the MCP SDK (httpx2 + truststore) does, or `SSL_CERT_FILE` or `SSL_CERT_DIR` if set

## Not ported

- **Connection sign-in for connection-backed services (for example `system.ai.atlassian`)**: on a 401 or `-32006`, the original runs `databricks auth login --resource` and injects an `authenticate` tool. ug-lite does not. For a service you have not signed in to, run this once:
  ```sh
  databricks auth login --host <workspace> --profile <profile> --resource <MCP URL>
  ```
  (or connect once with the original `ug mcp-proxy`)
- The GET SSE stream for messages the server sends outside a request
- Picking the newest of several `databricks` CLIs (ug-lite uses the first one on PATH)
- Re-authentication with `auth login --no-browser` (current CLIs do not accept this flag)
- Windows support (untested; for example, `DATABRICKS_BEARER_COMMAND` is split with POSIX shell rules, so paths like `C:\...` do not work)
- SSE resumption (`Last-Event-ID`). If the stream ends before the response, that request fails
