//! Minimal MCP stdio server for testing.
//! Responds to initialize, tools/list, and tools/call("echo").
use std::io::{BufRead, Write};

fn main() {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());

    for line in stdin.lock().lines() {
        let line = line.unwrap();
        if line.is_empty() { continue; }
        let req: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // Pass id through as-is (could be number, string, or null per JSON-RPC 2.0)
        let id = req["id"].clone();
        let method = req["method"].as_str().unwrap_or("").to_string();

        let resp = match method.as_str() {
            "initialize" => serde_json::json!({
                "jsonrpc": "2.0", "id": id,
                "result": { "protocolVersion": "2024-11-05", "capabilities": {} }
            }),
            "tools/list" => serde_json::json!({
                "jsonrpc": "2.0", "id": id,
                "result": {
                    "tools": [{
                        "name": "echo",
                        "description": "echoes input",
                        "inputSchema": { "type": "object",
                            "properties": { "message": { "type": "string" } } }
                    }]
                }
            }),
            "tools/call" => {
                let args = req["params"]["arguments"].clone();
                serde_json::json!({
                    "jsonrpc": "2.0", "id": id,
                    "result": { "echoed": args }
                })
            }
            _ => serde_json::json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": -32601, "message": "method not found" }
            }),
        };

        writeln!(out, "{}", serde_json::to_string(&resp).unwrap()).unwrap();
        out.flush().unwrap();
    }
}
