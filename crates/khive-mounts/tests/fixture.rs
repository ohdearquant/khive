//! Private stdio MCP fixture. The harness runs it only with a state-file argument.
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::{self, BufRead, Write},
    time::Duration,
};

fn main() {
    let Some(path) = std::env::args().nth(1).filter(|arg| !arg.starts_with('-')) else {
        return;
    };
    if !std::path::Path::new(&path).is_file() {
        return;
    }
    let mut starts = OpenOptions::new()
        .create(true)
        .append(true)
        .open(format!("{path}.starts"))
        .unwrap();
    writeln!(starts, "{}", std::process::id()).unwrap();
    for line in io::stdin().lock().lines() {
        let request: Value = serde_json::from_str(&line.unwrap()).unwrap();
        let Some(id) = request.get("id") else {
            continue;
        };
        let state: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        if state["exit"] == true {
            std::process::exit(3);
        }
        let method = request["method"].as_str().unwrap();
        let result = match method {
            "initialize" => {
                json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {}}, "serverInfo": {"name": "fixture", "version": "1"}})
            }
            "tools/list" => json!({"tools": state["tools"]}),
            "tools/call" => {
                let mut calls = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(format!("{path}.calls"))
                    .unwrap();
                writeln!(calls, "{}", request["params"]["name"]).unwrap();
                match request["params"]["arguments"]["mode"].as_str() {
                    Some("error") => {
                        println!(
                            "{}",
                            json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000, "message": "RAW_SECRET_SENTINEL"}})
                        );
                        io::stdout().flush().unwrap();
                        continue;
                    }
                    Some("timeout") => std::thread::sleep(Duration::from_secs(10)),
                    Some("malformed") => {
                        println!(
                            "{}",
                            json!({"jsonrpc": "2.0", "id": id, "result": {"content": "RAW_SECRET_SENTINEL"}})
                        );
                        io::stdout().flush().unwrap();
                        continue;
                    }
                    Some("is_error") => {
                        println!(
                            "{}",
                            json!({"jsonrpc": "2.0", "id": id, "result": {"content": [{"type": "text", "text": "RAW_SECRET_SENTINEL"}], "isError": true}})
                        );
                        io::stdout().flush().unwrap();
                        continue;
                    }
                    _ => {}
                }
                json!({"content": [{"type": "text", "text": "ok"}], "structuredContent": request["params"]["arguments"]})
            }
            _ => panic!("unsupported fixture method"),
        };
        println!("{}", json!({"jsonrpc": "2.0", "id": id, "result": result}));
        io::stdout().flush().unwrap();
    }
}
