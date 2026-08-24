# CLAUDE.md — vta-agent-memory

A Claude Code plugin that makes a Verifiable Trust Agent the user's memory
service. One Rust binary (MCP server + CLI) plus the plugin manifest, skill,
commands, and hook around it.

## Layout

```
.claude-plugin/plugin.json   plugin manifest
.mcp.json                    launches bin/vta-agent-memory serve
hooks/hooks.json             SessionStart -> `recall --format json`
skills/agent-memory/         WHEN to save and recall — the policy layer
commands/                    /remember /recall /forget /memories
scripts/install.sh           build + place bin/vta-agent-memory
src/
  lib.rs       the crate is a library too, so tests/ can reach it
  main.rs      CLI: serve | setup | recall | list | forget | doctor
  setup.rs     the online provisioning flow
  config.rs    ~/.config/vta-agent-memory/config.json (0600)
  store.rs     typed ops over vta/memory/* — the only VTA-facing code
  record.rs    the record shape, key encoding, and recall ranking
  server.rs    the six MCP tools
tests/
  memory_roundtrip.rs   Store end-to-end against an in-process fake VTA
```

## Testing

`tests/memory_roundtrip.rs` drives the real `VtaClient::memory_*` bodies through
the SDK's `test-loopback` transport into a `FakeMemoryKeyspace` that implements
the VTA's own storage semantics (upsert on `(contextId, key)`, whole-context
`list` in key order, `not found` on absent delete). No VTA, no mediator, no
socket — but the payloads on the wire are the ones that ship, which is the layer
where this crate's defects would live.

It does **not** cover framing: TSP sealing, DIDComm authcrypt, and mediator
routing all sit below the loopback point. Those need a live VTA.

Add a test there — not a unit test — whenever you change what goes on the wire
or what `Store` does with what comes back.

## Non-negotiables

**Nothing here is transport-aware.** `VtaClient::memory_{put,list,delete}` drive
the generic Trust-Task dispatcher, which picks TSP > DIDComm > REST from what
both DID documents advertise. If you find yourself branching on transport
anywhere outside `doctor`'s diagnostic print, something has gone wrong.

**Never wrap a `VtaClient` call in a retry loop.** Retry has exactly one owner
per failure domain; for operation completion that owner is
`VtaClient::idempotent`. A hand-rolled loop cannot hold an idempotency key
stable across attempts, so it turns one retried operation into two operations.

**Always shut the client down.** A DIDComm or TSP `VtaClient` owns a live,
auto-reconnecting mediator socket that `Drop` cannot close. The mediator permits
one websocket per DID, so a leaked client duels the next connection for that
slot. Every command routes its result through `finish()` (or an explicit
`shutdown().await`) *before* propagating an error — a bare `?` on the happy path
is the bug to look for.

**The role for a memory agent is `application`, never `admin`.** An empty
`allowed_contexts` list means *unrestricted* for `admin` and *authorized
nowhere* for every other role. A memory agent must not be one bad edit away from
reaching every context on the VTA.

**Setup writes nothing until it has connected as the new identity and listed
memories.** The probe is `memory/list/0.1` specifically, not a cheaper `whoami`:
listing is the operation gated on exactly what setup just arranged.

## The retrieval constraint

`vta/memory/list/0.1` returns every entry in the context, ascending key order.
No prefix, no cursor, no search. Everything in `record.rs` and `store.rs`
follows from that:

- Keys are `<type>/<slug>` so a type filter runs on the raw key, before decode
  (`MemoryKey::raw_key_has_type` / `Store::list_of_type`).
- Recall returns `summary()` (key, name, type, description), never `full()`.
  Bodies come from `memory_get` alone.
- `rank()` is weighted token containment. Deliberately crude; see its doc
  comment before replacing it.

`Store::list` is the single place to change if the upstream task ever grows a
prefix or a cursor. Growing one means a spec PR in
`trustoverip/dtgwg-trust-tasks-tf` first — the VTA's dispatcher refuses URIs the
published registry has no schema for.

## Boundaries with the other VTA stores

- **Secrets** → the secrets vault (`vault/*`), gated on `VaultRead`/`VaultWrite`.
- **Verifiable credentials** → the credential vault (`vault/credentials/*`).
- **Durable application state** → `vta/app-state/*/1.0` — versioned, namespaced,
  with a change feed.

Memory is none of those. The settling argument: *"forget everything" must stay a
safe thing for a user to ask*, which it cannot be if account state lives here.

## Upstream dependency

`vta_sdk::agent_connect::AgentConnect` is the shared connect ladder (did:webvh
bundle → scoped did:key → bearer token → `pnm` session). It lives in
OpenVTC/verifiable-trust-infrastructure and is pulled via a `[patch.crates-io]`
git dependency until it ships in a released `vta-sdk`. Removing that patch block
is the only change needed once it does.

Do not add a fifth way to authenticate here. If a new one is needed, it belongs
in `agent_connect` where `vta-mcp` gets it too.

## Conventions

- Run `cargo fmt` and `cargo clippy --all-targets` before committing.
- Conventional-commit titles; DCO sign-off (`git commit -s`).
- Tests live beside the code. The ones that matter are the behavioural ones —
  what recall does with a query that matches nothing, what `list` does with an
  entry another tool wrote — not round-trip serde.
