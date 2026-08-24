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
scripts/install.sh          # builds and places bin/vta-agent-memory
```

### Enrol this machine

Two phases, because **the machine holding your memories should not also hold an
operator credential**. Phase 1 mints an identity and asks for access; somebody
with admin grants it, from anywhere.

```bash
bin/vta-agent-memory init --vta-did did:webvh:… --context agent-memory
```

```
This machine's memory agent is ready to be granted access.

  agent DID   did:key:z6MkqVfVXEHNu2TZDZX4nV3oGxsZGMAMYhWKNryGVKWniCDA
  vta         did:webvh:…
  context     agent-memory
  mediator    did:webvh:…

Run this wherever you hold VTA admin — it does not have to be this machine:

  pnm acl create --did did:key:z6Mkq… --role application --contexts agent-memory --label vta-agent-memory

Then, back here:

  vta-agent-memory connect
```

`init` needs **no credential and no `pnm`**: it mints a key locally and reads
the mediator out of the VTA's DID document, which is a public lookup. The grant
line is meant to be pasted into a ticket or a chat message and run by whoever
holds admin — on another laptop, in CI, in another timezone.

`connect` then authenticates as that key, proves it can actually read the
context, and writes the config. It refuses to write anything until that works.

### Or, if you hold admin on this machine

```bash
bin/vta-agent-memory setup              # your default `pnm` VTA
bin/vta-agent-memory setup --vta did:webvh:…   # a specific one, by DID
```

`setup` does both phases at once, making the grant itself through your `pnm`
login. Convenient — and it needs an operator credential *here*, so prefer
`init` + `connect` anywhere that matters. `--vta` also accepts the local `pnm`
name, but prefer the DID: a `pnm` name is a nickname chosen on one machine and
means nothing on any other.

Then add the plugin to Claude Code (from a marketplace that lists this repo, or
by pointing at the checkout).

Check it any time:

```bash
bin/vta-agent-memory doctor
```

```
config      ~/.config/vta-agent-memory/config.json
vta         did:webvh:abc:vta.example.com:mine
context     my-project
identity    dedicated agent did:key
transport   Didcomm
memories    12
```

### Prerequisites

For `init` + `connect`: the VTA's DID, a trust context, and somebody who can run
one `pnm acl create`. Nothing else — no `pnm` on this machine.

For `setup`: a VTA you have already logged into with `pnm` **here**. It reads
`pnm`'s own config (`dirs::config_dir()/pnm` — `~/Library/Application Support/pnm`
on macOS, not `~/.config`) and reuses that login's keyring session. If
`pnm auth status` works, so will this.

## What setup does

Everything rides a flow the VTI stack already has:

| Step | Existing asset |
|---|---|
| Mint an agent identity | `EphemeralSetupKey` — the same helper every VTI two-phase setup uses |
| Find its way in | `resolve_vta_endpoint` against the VTA's DID document |
| Authorize it | `acl/create`, role `application`, scoped to one context |
| Prove it | a real `vta/memory/list/0.1` **as the new identity** |

`setup` additionally uses `pnm`'s config and keyring session to find the VTA and
make the grant without a second person.

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
revoking your login.

Reach for it when the VTA advertises no DIDComm mediator — a REST-only VTA — the
one case where a `did:key` agent has no way in. If that VTA has a mediator that
simply isn't in its DID document (a `did:key` VTA, an airgapped one), the better
fix is to tell `pnm` about it once:

```toml
# ~/.config/pnm/config.toml
[vtas.mine]
mediator_did = "did:web:mediator.example.com"
```

Setup prefers what `pnm` was configured with over DID-document discovery,
precisely because discovery cannot find the deployments that need it.

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
vta-agent-memory setup [--vta <did:… | pnm-name>] [--context <id>] [--use-session] [--force]
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
    "did": "did:key:z…",          // the agent; pnmSession has sessionKey instead
    "privateKeyMultibase": "z…",
    "vtaDid": "did:webvh:…",
    "mediatorDid": "did:web:…",
    "restUrl": "https://…"
  },
  "recallLimit": 8
}
```

## When the VTA is unreachable

The plugin does not disappear. The MCP server starts without contacting the VTA
and connects on the first memory call, so:

- The six tools are always present, and a failure comes back as a tool error the
  model can relay — "run `vta-agent-memory setup`" rather than silence.
- `memory_context` answers *without* connecting, so you can still ask which VTA
  and context you are pointed at while it is down.
- Failures are not latched: a VTA that comes back is picked up on the next call,
  and `setup` run mid-session takes effect without a restart.
- The `SessionStart` hook stays quiet and exits 0, so a session never fails to
  start because memory was unavailable.

A session that never touches memory never opens a mediator socket at all.

## Tests

```bash
cargo test
```

`tests/mcp_starts_without_a_vta.rs` drives the real binary over stdio with no
config at all, asserting it completes an MCP handshake, offers all six tools, and
returns an actionable error from a tool call. That is a property about process
lifetime, so it can only be observed by running the thing.

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
