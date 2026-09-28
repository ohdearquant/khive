//! Private stdio MCP fixture. The harness runs it only with a state-file argument.
use serde_json::{json, Value};
use std::{
    fs::{self, OpenOptions},
    io::{self, BufRead, Write},
    time::{Duration, Instant},
};

fn wait_for_release(path: &str, suffix: &str) {
    let marker = format!("{path}.{suffix}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !std::path::Path::new(&marker).exists() {
        assert!(
            Instant::now() < deadline,
            "fixture release marker missing: {suffix}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

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
            "tools/list" => {
                let mut catalogs = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(format!("{path}.catalogs"))
                    .unwrap();
                writeln!(catalogs, "{id}").unwrap();
                if state["exit_once_during_catalog"] == true
                    && OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .open(format!("{path}.exited_once"))
                        .is_ok()
                {
                    std::process::exit(3);
                }
                if let Some(delay_ms) = state["catalog_delay_ms"].as_u64() {
                    std::thread::sleep(Duration::from_millis(delay_ms));
                }
                if state["block_catalog"] == true {
                    wait_for_release(&path, "release_catalog");
                }
                json!({"tools": state["tools"]})
            }
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
                    Some("delay") => std::thread::sleep(Duration::from_millis(300)),
                    Some("block") => wait_for_release(&path, "release_call"),
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
