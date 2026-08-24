//! The `SessionStart` hook contract.
//!
//! `recall --format json` is invoked by a hook on every session start, which
//! gives it a different contract from a command a person typed. It must never
//! fail the session and must never inject noise. Those are easy properties to
//! regress by "improving" the error handling, so they are pinned here by
//! running the real binary.

use std::process::Command;

fn bin() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_vta-agent-memory"));
    // A path that cannot exist, so the failure is "not configured" rather than
    // anything depending on the developer's own machine.
    c.env(
        "VTA_AGENT_MEMORY_CONFIG",
        "/nonexistent/vta-agent-memory/config.json",
    );
    c
}

#[test]
fn an_unusable_config_does_not_fail_the_session() {
    let out = bin()
        .args(["recall", "--format", "json"])
        .output()
        .expect("running the binary");

    assert!(
        out.status.success(),
        "a hook must exit 0 whatever the state of the VTA — someone just opened a terminal"
    );
    assert!(
        out.stdout.is_empty(),
        "and must inject nothing, not an error message: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn the_same_failure_is_a_real_error_for_a_person() {
    let out = bin().arg("recall").output().expect("running the binary");

    assert!(
        !out.status.success(),
        "someone who typed `recall` wants to know it failed"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("vta-agent-memory setup"),
        "and wants to be told the fix, got: {stderr}"
    );
}

#[test]
fn hook_output_is_json_only_on_stdout() {
    // stdout carries the hook envelope; every log line must go to stderr, or a
    // single stray `println!` corrupts it. Nothing is configured here, so the
    // strongest available assertion is that the failure path stays silent on
    // stdout — the same guarantee `serve` relies on for its JSON-RPC channel.
    let out = bin()
        .args(["recall", "--format", "json"])
        .env("RUST_LOG", "debug")
        .output()
        .expect("running the binary");

    assert!(
        out.stdout.is_empty(),
        "debug logging leaked into the protocol channel: {}",
        String::from_utf8_lossy(&out.stdout)
    );
}
