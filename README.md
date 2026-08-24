# vta-agent-memory

Durable memory for Claude Code, stored in **your own Verifiable Trust Agent**
rather than in the tool.

Memories are ordinary `vta/memory/{put,list,delete}/0.1` Trust Tasks against a
VTA you control. They are scoped to one trust context, audited on the VTA, and
included in its encrypted backups. Revoking this machine's access is one
`pnm acl delete`.

> **Name note:** `vta-agent-memory` is descriptive, not a product name. If this
> ships under the Affinidi `Agent[Capability]` house style, the name is the last
> thing to decide, not the first.

## What you get

A Claude Code plugin with:

- **Six MCP tools** — `memory_recall`, `memory_get`, `memory_save`,
  `memory_forget`, `memory_list`, `memory_context`.
- **A skill** (`agent-memory`) that says *when* to save and recall, what is
  worth keeping, and what must never go in. This is most of the value; the tools
  are the easy part.
- **Four slash commands** — `/remember`, `/recall`, `/forget`, `/memories`.
- **A `SessionStart` hook** that loads your memories into a new session before
  you type anything, so the model does not have to remember to ask.

## Install

Needs Rust 1.95+, and a VTA you are already logged into with
[`pnm`](https://github.com/OpenVTC/verifiable-trust-infrastructure).

```bash
git clone https://github.com/OpenVTC/vta-agent-memory
cd vta-agent-memory
scripts/install.sh                       # builds and places bin/vta-agent-memory
bin/vta-agent-memory setup --vta <your-pnm-slug>
```

Then add the plugin to Claude Code (from a marketplace that lists this repo, or
by pointing at the checkout).

Check it any time:

```bash
bin/vta-agent-memory doctor
```

## What setup does

Everything rides a flow the VTI stack already has:

| Step | Existing asset |
|---|---|
| Authenticate as you | your `pnm` keyring session |
| Find the trust context | `contexts/list/1.0` |
| Mint an agent identity | `EphemeralSetupKey` (the same helper every VTI two-phase setup uses) |
| Authorize it | `acl/create`, role `application`, scoped to one context |
| Find its way back in | `resolve_vta_endpoint` against the VTA's DID document |
| Prove it | a real `vta/memory/list/0.1` **as the new identity** |

Nothing is written to disk until that last step passes. A config that has never
been exercised is a config that fails later, in a hook, with nobody watching.

By default setup mints a **dedicated `did:key`** whose ACL entry names exactly
one context and the least-privileged role that can reach memory. That is the
point: the memory service is not you. Revoke it with

```bash
pnm acl delete --did <the agent DID setup printed>
```

`--use-session` skips the minting and reuses your own login instead. It stores no
key, but the memory service then inherits your whole reach, and revoking it means
revoking your login. Use it when the VTA advertises no DIDComm mediator (a
REST-only VTA), which is the one case where a `did:key` agent has no way to
connect.

**Online only.** There is no offline/air-gapped setup path.

## Transports

None of this code is transport-aware. `VtaClient::memory_{put,list,delete}` go
through the generic Trust-Task dispatcher, which selects **TSP > DIDComm > REST**
from what your DID document and the VTA's both advertise. `doctor` prints which
one you ended up on.

## Design notes worth knowing

**Retrieval happens on this side of the wire.** `memory/list/0.1` returns *every
entry in the context, in ascending key order* — no prefix, no cursor, no search.
So:

- Keys are structured `<type>/<slug>`, which lets a type filter run on the raw
  key before any record is decoded.
- Records carry a one-line `description`, and recall ranks and returns
  **descriptions, not bodies**. Bodies come back only from `memory_get`. A memory
  service that pastes every body into the context window has made the session
  worse.
- Ranking is weighted token containment (name 4, description 2, body 1). Crude
  and predictable, chosen over embeddings because the VTA offers no vector index
  and a scoring function nobody can predict is worse than one everybody can.

Cost is a full read of the context per recall. Fine at the hundreds of memories a
person accumulates. If that stops being true, the fix is upstream — a
`vta/memory/query` with a prefix and a cursor — and it starts with a spec PR in
`trustoverip/dtgwg-trust-tasks-tf`, because the VTA's dispatcher refuses URIs the
published registry has no schema for.

**Memory is not application state.** "Forget everything" has to stay a safe thing
for a user to ask, which it stops being the moment account state lives here. The
VTA's `vta/app-state/*/1.0` family — versioned, namespaced, with a change feed —
is where that belongs.

**Memory is not a secret store.** The VTA has a secrets vault and a credential
vault, gated on different capabilities on purpose.

## Commands

```
vta-agent-memory setup --vta <slug> [--context <id>] [--use-session] [--force]
vta-agent-memory serve                       # MCP over stdio (the default)
vta-agent-memory recall [--query Q] [--type T] [--limit N] [--full] [--format text|json]
vta-agent-memory list [--type T]
vta-agent-memory forget <key>
vta-agent-memory doctor
```

`recall --format json` emits the `hookSpecificOutput.additionalContext` envelope
Claude Code reads from a `SessionStart` hook. That is why this is a CLI as well
as an MCP server: hooks are shell commands, not MCP calls.

## Configuration

`~/.config/vta-agent-memory/config.json` (override with
`$VTA_AGENT_MEMORY_CONFIG`), written `0600` because the dedicated-identity form
holds a private key.

```jsonc
{
  "version": 1,
  "contextId": "my-project",
  "identity": {
    "kind": "agent",              // or "pnmSession"
    "did": "did:key:z…",
    "privateKeyMultibase": "z…",
    "vtaDid": "did:webvh:…",
    "mediatorDid": "did:web:…",
    "restUrl": "https://…"
  },
  "recallLimit": 8
}
```

## Tests

```bash
cargo test
```

`tests/memory_roundtrip.rs` runs the real `VtaClient::memory_*` bodies through
the SDK's in-process loopback transport into a fake keyspace with the VTA's own
storage semantics — upsert on `(contextId, key)`, whole-context `list`,
`not found` on an absent delete. No VTA, no mediator, no socket, but the
payloads asserted on are the ones that ship. Framing (TSP sealing, DIDComm
authcrypt, mediator routing) sits below that point and needs a live VTA.

## Relationship to `vta-mcp`

`vta-mcp` (in the VTI workspace) bridges the VTA's whole management console to
MCP, and its generic `vta_call` tool can already reach `vta/memory/*`. This is
not a smaller copy of it — it is the memory product: tools shaped like
remembering rather than like a key/value store, plus the retrieval, skill, and
hook layers that the raw Trust Tasks have no opinion about.

Both share one connect ladder, `vta_sdk::agent_connect::AgentConnect` — did:webvh
bundle, scoped did:key, bearer token, `pnm` session — so the two agree about how
an agent-side bridge gets in.

## Licence

Apache-2.0.
