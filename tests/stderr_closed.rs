//! A launcher that closes its stderr reader must not take the proxy down with it.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

#[test]
fn proxy_keeps_answering_when_stderr_reader_is_closed() {
    let (reader, writer) = std::io::pipe().unwrap();
    drop(reader); // every stderr write now fails with EPIPE
    let mut child = Command::new(env!("CARGO_BIN_EXE_ug-lite"))
        .args([
            "mcp-proxy",
            "--url",
            "http://127.0.0.1:9/mcp",
            "--host",
            "https://h",
        ])
        .env("DATABRICKS_BEARER", "t")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(writer)
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    // Each line makes the proxy log to stderr before replying.
    stdin.write_all(b"{oops\n").unwrap();
    stdin
        .write_all(br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#)
        .unwrap();
    stdin.write_all(b"\n").unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let first = lines.next().unwrap().unwrap();
    let second = lines.next().unwrap().unwrap();
    assert!(
        first.contains("-32700") && first.contains(r#""id":null"#),
        "{first}"
    );
    assert!(
        second.contains("-32000") && second.contains(r#""id":1"#),
        "{second}"
    );
    drop(stdin);
    assert!(child.wait().unwrap().success());
}
