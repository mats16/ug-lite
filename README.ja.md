# ug-lite

[English](README.md) | 日本語

[Unity Gateway](https://github.com/databricks/unity-gateway) の `ug` コマンドのうち、コーディングエージェントが毎回起動する 2 つのサブコマンドだけを Rust で再実装した、メモリ使用量の小さい互換バイナリです。

| サブコマンド | 用途 |
|---|---|
| `ug mcp-proxy --url <URL> [--host] [--profile] [--use-pat]` | stdio MCP ⇔ Databricks streamable-HTTP MCP のブリッジ。リクエストごとにトークンを注入 |
| `ug auth-token [--host] [--profile] [--use-pat] [--force-refresh] [--browser-login]` | Claude Code の `apiKeyHelper` や Codex の auth command 向けにトークンを出力 |

それ以外のサブコマンドやフラグ（`configure`、`--version`、`--client-id` など）は、元の Python 版 `ug` にそのまま `exec` で引き渡します。

`ug-lite --help` だけは、元の `ug` のヘルプの前に、ug-lite のバージョン、ug-lite が処理するコマンド、引き渡し先の `ug` のパスを stdout に表示します（`ug-lite configure --help` などサブコマンドのヘルプは元の `ug` のままです）。`ug-lite --version`（`-V`）は、ランチャーが起動のたびに読む出力なので、何も足さずに元の `ug` に引き渡します。

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

## 計測 (macOS / Apple Silicon, `system.ai.dbsql` に対して initialize + tools/list)

| | Physical footprint | RSS | initialize までの時間 |
|---|---|---|---|
| `ug` (Python) | 68 MB | 90 MB | 0.84 s |
| `ug-lite` | 3.9 MB | 9.7 MB | 0.2 s |

`auth-token` は 0.28 s から 0.07 s（ほぼ `databricks auth token` 自体の時間）になります。

## インストール

最新リリースのバイナリ（macOS arm64、Linux x86_64 / arm64）を `~/.local/bin/ug-lite` にインストールします。元の `ug`（`~/.local/bin/ug`）には手を触れません。

```sh
curl -fsSL https://raw.githubusercontent.com/mats16/ug-lite/main/install.sh | sh
```

`/usr/local/bin/ug-lite` に置く場合は `UG_LITE_INSTALL_DIR` で指定します（書き込めないディレクトリでは、インストーラーが自分で `sudo` を使います）。

```sh
curl -fsSL https://raw.githubusercontent.com/mats16/ug-lite/main/install.sh | UG_LITE_INSTALL_DIR=/usr/local/bin sh
```

## 使い方

エージェント設定で、`ug` の代わりに `ug-lite` を指定します。引数は `ug` と同じです。例: Codex の `~/.codex/config.toml`

```toml
[mcp_servers.slack]
command = "/Users/you/.local/bin/ug-lite"
args = ["mcp-proxy", "--url", "https://<workspace>/ai-gateway/mcp-services/system.ai.slack", "--host", "https://<workspace>"]
```

`ug configure` や `ug claude` などは設定に `ug` の絶対パスを書き込むので、実行したあとは `ug-lite` に書き換え直してください。

ug-lite が扱わないコマンドは、`$UG_LITE_FALLBACK`（元の `ug` の絶対パス。`~` は展開されません）に引き渡します。未設定なら `~/.local/bin/ug`（`ug` 自身のインストール先）を使います。PATH は探さないので、起動元（GUI アプリなど）の PATH に `~/.local/bin` がなくても動きます。`ug` を別の場所に入れた場合は `UG_LITE_FALLBACK` を設定してください。ug-lite は起動されるたびにこの変数を読むので、インストール時ではなく、ug-lite を起動する環境に設定します。エージェントから起動する場合は、そのエージェント設定の `env` に書くのが確実です（GUI アプリはシェルの設定ファイルを読みません）。例: Codex なら `[mcp_servers.slack]` に `env = { UG_LITE_FALLBACK = "/opt/ug/bin/ug" }`。引き渡し先がない、実行できない、ug-lite 自身を指す場合は、理由を stderr に出して exit 2 で終わります。ug-lite 同士が呼び合うことはありません。

### v0.1.0 からの更新

インストーラーを、v0.1.0 を入れたときと同じ `UG_LITE_INSTALL_DIR`（指定しなかった場合はなし）で再実行すると、ug-lite が新しいバージョンに置き換わります。

引き渡し先の探し方が、「PATH 上の次の `ug`」から「`$UG_LITE_FALLBACK`、なければ `~/.local/bin/ug`」に変わりました。元の `ug` が `~/.local/bin/ug` 以外にある場合は、上のとおり `UG_LITE_FALLBACK` を設定してください。設定しないと、引き渡すコマンドが exit 2 で終わります。

v0.1.0 の案内どおり `~/.ug-lite/bin/ug` のリンクを PATH の先頭に置いている場合は、`ug` も引き続き ug-lite になります。`ug` を元の `ug` に戻したい場合は、そのリンクと PATH の設定を消し、エージェント設定で `ug-lite` を直接指定してください。

## 元の実装と同じ挙動

- `~/.ucode/state.json` の現在のワークスペース、`profile`、`use_pat` を既定値として使います
- `DATABRICKS_BEARER` と `DATABRICKS_BEARER_COMMAND` を優先し、`--use-pat` の場合は `~/.databrickscfg` の PAT を使います
- トークンは `databricks auth token --output json` で取得し、期限の 5 分前までプロセス内で再利用します
- トークンキャッシュのロック競合はリトライします。`--browser-login` の場合は、ロックファイルで直列化したうえでブラウザログインします
- 起動時にトークンを取得できなければ stderr にメッセージを出し、exit 2 で終了します（エージェントの起動タイムアウトを待たせないため）。起動後の取得失敗はそのリクエストへのエラー応答にとどめ、再ログイン後はそのまま使えます
- MCP セッション（`mcp-session-id`、`mcp-protocol-version`）を扱い、JSON 応答と SSE 応答の両方に対応します。stdin が閉じたらセッションを DELETE します
- どのリクエストにも、正しい形の応答を必ず 1 つ返します。上流から届いたメッセージは、MCP スキーマの JSON-RPC 形式（`result` はオブジェクト、`error` は整数の `code` と文字列の `message` を持つ）であることを確認してから転送します。SSE は WHATWG の仕様どおりに解析し、改行は LF・CRLF・CR のどれでも受け付けます。stdin の改行も同じです
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
