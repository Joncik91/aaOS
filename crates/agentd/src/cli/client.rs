//! JSON-RPC transport over /run/agentd/agentd.sock.
//!
//! Two call shapes:
//!   * `call_sync` — single request, single response (list/status/stop).
//!   * `call_streaming` — single request, NDJSON stream (submit/logs).

use std::path::Path;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::cli::errors::CliError;

/// Send one JSON-RPC request, read one newline-delimited response, and return
/// the `result` field. Server-reported `error` objects become `CliError::ServerError`.
pub async fn call_sync(socket: &Path, method: &str, params: Value) -> Result<Value, CliError> {
    let mut stream = UnixStream::connect(socket)
        .await
        .map_err(|e| CliError::DaemonUnreachable(socket.display().to_string(), e.to_string()))?;

    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let body = req.to_string() + "\n";
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let n = reader.read_line(&mut line).await?;
    if n == 0 {
        return Err(CliError::BrokenPipe);
    }

    let resp: Value = serde_json::from_str(line.trim())
        .map_err(|_| CliError::Protocol("malformed JSON-RPC response".into()))?;

    if let Some(err) = resp.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown")
            .to_string();
        return Err(CliError::ServerError(msg));
    }

    resp.get("result")
        .cloned()
        .ok_or_else(|| CliError::Protocol("response missing 'result' field".into()))
}

/// Connect, send a streaming request, return a BufReader the caller reads
/// NDJSON frames from via `read_line`. The caller is responsible for the
/// frame-handling loop and for closing the connection on SIGINT or terminal frame.
pub async fn call_streaming(
    socket: &Path,
    method: &str,
    params: Value,
) -> Result<BufReader<UnixStream>, CliError> {
    let mut stream = UnixStream::connect(socket)
        .await
        .map_err(|e| CliError::DaemonUnreachable(socket.display().to_string(), e.to_string()))?;

    let req = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    });
    let body = req.to_string() + "\n";
    stream.write_all(body.as_bytes()).await?;
    stream.flush().await?;

    Ok(BufReader::new(stream))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::net::UnixListener;

    async fn temp_socket() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sock");
        (dir, path)
    }

    #[tokio::test]
    async fn call_sync_returns_result_on_success() {
        let (_dir, sock) = temp_socket().await;
        let listener = UnixListener::bind(&sock).unwrap();

        // Fake server: read one line, reply with a JSON-RPC success.
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let resp = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "result": { "ok": true, "n": 42 }
            });
            stream
                .write_all((resp.to_string() + "\n").as_bytes())
                .await
                .unwrap();
            stream.flush().await.unwrap();
        });

        let result = call_sync(&sock, "agent.list", serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(result["n"], 42);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn call_sync_surfaces_server_error() {
        let (_dir, sock) = temp_socket().await;
        let listener = UnixListener::bind(&sock).unwrap();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let resp = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "error": { "code": -32603, "message": "agent not found" }
            });
            stream
                .write_all((resp.to_string() + "\n").as_bytes())
                .await
                .unwrap();
            stream.flush().await.unwrap();
        });

        let err = call_sync(&sock, "agent.status", serde_json::json!({"agent_id": "x"}))
            .await
            .unwrap_err();
        match err {
            CliError::ServerError(msg) => assert!(msg.contains("agent not found")),
            other => panic!("expected ServerError, got {:?}", other),
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn call_sync_reports_unreachable_daemon() {
        let (_dir, sock) = temp_socket().await;
        // No listener bound; connect will fail.
        let err = call_sync(&sock, "agent.list", serde_json::json!({}))
            .await
            .unwrap_err();
        match err {
            CliError::DaemonUnreachable(path, _inner) => {
                assert!(path.contains("test.sock"));
            }
            other => panic!("expected DaemonUnreachable, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn call_streaming_returns_reader_for_ndjson() {
        let (_dir, sock) = temp_socket().await;
        let listener = UnixListener::bind(&sock).unwrap();

        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            // Emit 3 NDJSON lines then close.
            for i in 0..3 {
                let frame = serde_json::json!({"kind": "event", "i": i});
                stream
                    .write_all((frame.to_string() + "\n").as_bytes())
                    .await
                    .unwrap();
            }
            stream.flush().await.unwrap();
        });

        let mut reader = call_streaming(
            &sock,
            "agent.submit_streaming",
            serde_json::json!({"goal": "x"}),
        )
        .await
        .unwrap();
        let mut frames = Vec::new();
        let mut line = String::new();
        while reader.read_line(&mut line).await.unwrap() > 0 {
            if let Ok(v) = serde_json::from_str::<Value>(line.trim()) {
                frames.push(v);
            }
            line.clear();
        }
        assert_eq!(frames.len(), 3);
        assert_eq!(frames[0]["i"], 0);
        assert_eq!(frames[2]["i"], 2);
        server.await.unwrap();
    }
}
