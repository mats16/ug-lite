# AGENTS.md

ug-lite は、`ug mcp-proxy` と `ug auth-token` だけを Rust で再実装した、メモリ使用量の小さい互換バイナリです。扱わないものは元の Python 版 `ug` に引き渡します。挙動の仕様、終了コード、エラーコード、移植していないものは [README.md](README.md) が正です。変更の前に読んでください。

## コマンド

Rust 1.89 以降が必要です（`Cargo.toml` の `rust-version`。clippy がこれより新しい API の使用を検出します）。

```sh
cargo build                                # 開発ビルド
cargo build --release                      # 配布ビルド (target/release/ug-lite)
cargo test                                 # ユニットテスト + tests/ の結合テスト
cargo test <名前の一部>                     # 名前に一致するテストだけ実行
cargo fmt --check                          # 整形
cargo clippy --all-targets -- -D warnings  # lint。警告もエラーにする
```

変更を終える前に `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings`、`cargo test` をすべて通してください。PR でテストを走らせる CI はないので、手元で確認します（`.github/workflows/pullfrog.yml` は手動起動のエージェント用です）。

## 構成

- `src/main.rs`: 実装のすべて（1 ファイル）。読み始めるなら `main`、`parse_opts`、`fallback`、`auth_token`、`Auth`、`mcp_proxy`、`Proxy`、`client_message`、`rpc_kind`、`read_sse` です
- `src/main.rs` 末尾の `mod tests`: ユニットテスト。HTTP の相手は `mock` / `mock_held` / `mock_seen` で立てるローカルサーバーです。1 つの mock が受け付ける接続は 1 つだけです
- `tests/`: ビルド済みバイナリを起動する結合テスト

## 守ること

- **メモリと起動時間が存在理由です。** 依存は増やさないでください。特に async ランタイム（tokio など）は入れません
- **`eprintln!` / `println!` は使いません。** 読み手が閉じたパイプに書くと、どのビルドでも panic します（release ビルドは `panic = "abort"` なのでプロセスごと落ちます）。stderr には `elog!` を使い、stdout へは書き込みエラーを panic にしない方法で書きます
- **`mcp-proxy` の stdout は MCP のプロトコル用です。** JSON-RPC メッセージ以外は書きません
- **終了コードとエラーコードは呼び出し側との取り決めです。** 変えるときは README も合わせて直します。移植しない機能を増やしたときも README を更新します
- **テストを足します。** 新しい分岐やパーサーは `mod tests` で、終了コードやパイプなどプロセスとしての挙動は `tests/` で確かめます。ネットワークに出るテストは書きません

## 手で動かす

```sh
cargo build
(echo '{"jsonrpc":"2.0","id":1,"method":"ping"}'; sleep 1) |
  DATABRICKS_BEARER=dummy ./target/debug/ug-lite mcp-proxy --url http://127.0.0.1:9/mcp --host https://h
# stderr: ug mcp-proxy: upstream request failed: ...
# stdout: {"jsonrpc":"2.0","id":1,"error":{"code":-32000,...}}
```

`DATABRICKS_BEARER` を設定すると `databricks` CLI を呼ばずに起動できます。`sleep` は、応答の前に stdin が閉じると、処理中のリクエストに応答しないまま終了する既知の不具合を避けるためです。実際のワークスペースに対する確認には `databricks` CLI のログインが必要です。
