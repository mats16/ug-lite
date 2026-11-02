# ug-lite

[Unity Gateway](https://github.com/databricks/unity-gateway) の `ug` コマンドのうち、コーディングエージェントが毎回起動する 2 つのサブコマンドだけを Rust で再実装した、メモリ使用量の小さい互換バイナリです。

| サブコマンド | 用途 |
|---|---|
| `ug mcp-proxy --url <URL> [--host] [--profile] [--use-pat]` | stdio MCP ⇔ Databricks streamable-HTTP MCP のブリッジ。リクエストごとにトークンを注入 |
| `ug auth-token [--host] [--profile] [--use-pat] [--force-refresh] [--browser-login]` | Claude Code の `apiKeyHelper` や Codex の auth command 向けにトークンを出力 |

それ以外のサブコマンドやフラグ（`configure`、`--help`、`--client-id` など）は、元の Python 版 `ug` にそのまま `exec` で引き渡します。

## 計測 (macOS / Apple Silicon, `system.ai.dbsql` に対して initialize + tools/list)

| | Physical footprint | RSS | initialize までの時間 |
|---|---|---|---|
| `ug` (Python) | 68 MB | 90 MB | 0.84 s |
| `ug-lite` | 3.9 MB | 9.7 MB | 0.2 s |

`auth-token` は 0.28 s から 0.07 s（ほぼ `databricks auth token` 自体の時間）になります。

## インストール

最新リリースのバイナリ（macOS arm64、Linux x86_64 / arm64）を `~/.local/bin/ug-lite` にインストールします。

```sh
curl -fsSL https://raw.githubusercontent.com/mats16/ug-lite/main/install.sh | sh
```

`/usr/local/bin/ug-lite` に置く場合は `UG_LITE_INSTALL_DIR` で指定します（書き込めないディレクトリでは `sudo` を使います）。

```sh
curl -fsSL https://raw.githubusercontent.com/mats16/ug-lite/main/install.sh | UG_LITE_INSTALL_DIR=/usr/local/bin sh
```

インストールが終わると、`ug` という名前で使うためのリンクを作るコマンドが表示されます。それを実行し、`~/.ug-lite/bin` を元の `ug`（`~/.local/bin/ug`）より前の PATH に置きます（`~/.zshrc` などに書いておきます）。

```sh
export PATH="$HOME/.ug-lite/bin:$PATH"
```

`ug configure` や `ug claude` などはエージェント設定に `shutil.which("ug")` の絶対パスを書き込むため、PATH の先頭に置いておけば次回の設定更新から ug-lite が使われます。既存の設定ファイル（例: `~/.codex/config.toml`）の `command = ".../ug"` を書き換えても構いません。

フォールバック先は `$UG_LITE_FALLBACK`（元の `ug` の絶対パス）です。未設定なら、PATH 上で ug-lite 以外の最初の実行可能な `ug` を使います。ug-lite 同士が呼び合うことはありません。

## 元の実装と同じ挙動

- `~/.ucode/state.json` の現在のワークスペース、`profile`、`use_pat` を既定値として使います
- `DATABRICKS_BEARER` と `DATABRICKS_BEARER_COMMAND` を優先し、`--use-pat` の場合は `~/.databrickscfg` の PAT を使います
- トークンは `databricks auth token --output json` で取得し、期限の 5 分前までプロセス内で再利用します
- トークンキャッシュのロック競合はリトライします。`--browser-login` の場合は、ロックファイルで直列化したうえでブラウザログインします
- 起動時にトークンを取得できなければ stderr にメッセージを出し、exit 2 で終了します（エージェントの起動タイムアウトを待たせないため）。起動後の取得失敗はそのリクエストへのエラー応答にとどめ、再ログイン後はそのまま使えます
- MCP セッション（`mcp-session-id`、`mcp-protocol-version`）を扱い、JSON 応答と SSE 応答の両方に対応します。stdin が閉じたらセッションを DELETE します
- どのリクエストにも、正しい形の応答を必ず 1 つ返します。上流から届いたメッセージは、MCP スキーマの JSON-RPC 形式（`result` はオブジェクト、`error` は整数の `code` と文字列の `message` を持つ）であることを確認してから転送します。SSE は WHATWG の仕様どおりに解析し、改行は LF・CRLF・CR のどれでも受け付けます。stdin も同じです
- 転送に失敗したときのエラーコードは ug-lite 独自の取り決めで、MCP SDK と完全に同じであるとは保証しません。ただし、SDK と同じ値を選んでいます

  | 状況 | コード |
  |---|---|
  | 上流に接続できない、SSE が応答の前に切れた | -32000 |
  | リクエストへの 202、3xx、想定外の content-type | -32600 |
  | セッションがある状態での 404（`Session terminated`） / セッションがない状態での 404 | -32600 / -32601 |
  | 応答が JSON-RPC として解釈できない | -32700 |
  | その他の HTTP エラー（本文が正しい JSON-RPC エラーなら、そのコードのまま転送） | -32603 |
  | セッション中にトークンを取得できない（プロセスは終了しない） | -32603 |
- TLS は MCP SDK（httpx2 + truststore）と同じく OS のトラストストアを使い、`SSL_CERT_FILE` か `SSL_CERT_DIR` があればそれを使います

## 移植していないもの

- **connection-backed サービス（例: `system.ai.atlassian`）の接続サインイン**: 元の実装では、401 や `-32006` を受けると `databricks auth login --resource` を実行し、`authenticate` ツールを差し込みます。ug-lite は行いません。未サインインのサービスは、一度だけ次を実行してください。
  ```sh
  databricks auth login --host <workspace> --profile <profile> --resource <MCP URL>
  ```
  （または元の `ug mcp-proxy` で一度接続してください）
- リクエスト外でサーバーから届くメッセージ用の GET SSE ストリーム
- 複数の `databricks` CLI から最新版を選ぶ処理（PATH 上の最初のものを使います）
- `auth login --no-browser` による再認証（現行の CLI はこのフラグを受け付けません）
- Windows 対応（未検証です。たとえば `DATABRICKS_BEARER_COMMAND` を POSIX のシェル規則で分割するため、`C:\...` のようなパスは扱えません）
- SSE の再開（`Last-Event-ID`）。応答の前にストリームが切れたら、そのリクエストはエラーになります
