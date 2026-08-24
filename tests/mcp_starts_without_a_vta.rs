//! The MCP server must exist even when the VTA does not.
//!
//! Claude Code launches this server at session start and takes what it gets. A
//! process that exits before speaking MCP does not show up as a broken memory
//! service — it shows up as **no memory tools at all**, and the model then has
//! no way to tell anyone why, because the thing it would use to say so is the
//! thing that is missing.
//!
//! It used to do exactly that: `serve` connected before serving, so an
//! unreachable VTA, a laptop on a train, or a machine where `setup` had never
//! been run removed the whole plugin.
//!
//! These drive the real binary over stdio, because that is the only way to
//! observe the property — it is about process lifetime, not about any function.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

/// Speak `requests` to a fresh server that has no usable config, and return the
/// JSON-RPC responses it wrote to stdout.
fn talk_to_server(requests: &[&str]) -> Vec<serde_json::Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_vta-agent-memory"))
        .arg("serve")
        // A path that cannot exist, so the failure is "not configured" and does
        // not depend on the developer's own machine or network.
        .env(
            "VTA_AGENT_MEMORY_CONFIG",
            "/nonexistent/vta-agent-memory/config.json",
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawning the server");

    {
        let stdin = child.stdin.as_mut().expect("stdin");
        for r in requests {
            writeln!(stdin, "{r}").expect("writing a request");
        }
        stdin.flush().expect("flush");
    }
    // Dropping stdin is what lets the server finish; without it this hangs.
    drop(child.stdin.take());

    let out = BufReader::new(child.stdout.take().expect("stdout"));
    let responses: Vec<serde_json::Value> = out
        .lines()
        .map_while(Result::ok)
        .filter_map(|l| serde_json::from_str(&l).ok())
        .collect();
    let _ = child.wait();
    responses
}

fn by_id(responses: &[serde_json::Value], id: i64) -> Option<&serde_json::Value> {
    responses.iter().find(|r| r.get("id") == Some(&id.into()))
}

const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}"#;
const INITIALIZED: &str = r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#;

#[test]
fn the_server_completes_a_handshake_with_no_vta_reachable() {
    let responses = talk_to_server(&[INITIALIZE]);
    let init = by_id(&responses, 1).expect("the server must answer initialize");
    assert_eq!(
        init["result"]["serverInfo"]["name"], "vta-agent-memory",
        "got: {init}"
    );
}

#[test]
fn every_memory_tool_is_offered_with_no_vta_reachable() {
    // Absent tools are the failure this guards. If the model cannot see
    // `memory_context`, it cannot even explain what is wrong.
    let responses = talk_to_server(&[
        INITIALIZE,
        INITIALIZED,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
    ]);
    let listed = by_id(&responses, 2).expect("the server must answer tools/list");
    let names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();

    for expected in [
        "memory_save",
        "memory_recall",
        "memory_get",
        "memory_forget",
        "memory_list",
        "memory_context",
    ] {
        assert!(
            names.contains(&expected),
            "missing {expected}; have {names:?}"
        );
    }
}

#[test]
fn a_tool_call_returns_an_error_that_names_the_fix() {
    // The error reaches a person through the model, so it has to be worth
    // relaying — "run setup", not "os error 2".
    let responses = talk_to_server(&[
        INITIALIZE,
        INITIALIZED,
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"memory_recall","arguments":{"query":"anything"}}}"#,
    ]);
    let called = by_id(&responses, 3).expect("the server must answer the tool call");
    let message = called["error"]["message"]
        .as_str()
        .unwrap_or_else(|| panic!("expected a tool error, got: {called}"));
    assert!(
        message.contains("vta-agent-memory setup"),
        "the error must name the fix: {message}"
    );
}

#[test]
fn stdout_carries_only_json_rpc() {
    // stdout is the protocol channel. One stray `println!` — or one log line
    // routed to the wrong stream — corrupts every message on it.
    let responses = talk_to_server(&[INITIALIZE, INITIALIZED]);
    assert!(
        !responses.is_empty(),
        "nothing parsed as JSON-RPC, so something else is writing to stdout"
    );
    for r in &responses {
        assert_eq!(r["jsonrpc"], "2.0", "non-JSON-RPC line on stdout: {r}");
    }
}
