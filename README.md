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
scripts/install.sh          # cargo install, into ~/.cargo/bin
```

Then add it to Claude Code. **Two steps** — `claude plugin install <name>`
resolves from *configured marketplaces*, so on its own it reports
"not found in any configured marketplace":

```bash
claude plugin marketplace add OpenVTC/vta-agent-memory
claude plugin install vta-agent-memory@vta-agent-memory
```

The plugin ships Rust source, not a compiled binary, so `bin/vta-agent-memory`
in the repo is a **shim**: `.mcp.json` and the hook invoke it, and it execs
whichever real binary `cargo install` produced. That is what makes a
marketplace install work, where the plugin directory is a fresh clone with no
build artifacts in it. Override the lookup with `$VTA_AGENT_MEMORY_BIN`.

### Enrol this machine

Two phases, because **the machine holding your memories should not also hold an
operator credential** — and because the identity you paste into a ticket should
stop working once it has been used.

```bash
bin/vta-agent-memory init --vta-did did:webvh:… --context agent-memory
```

```
This machine minted a temporary identity and needs it authorized.

  temp DID    did:key:z6Mkoqb2y3jbu6zoPDBoYGzA4j34JwsnToSUNqBgh9W7VebT
  vta         did:webvh:…
  context     agent-memory

Run these wherever you hold VTA admin — it does not have to be this machine:

  pnm contexts create --id agent-memory --name agent-memory    # skip if it exists
  pnm acl create --did did:key:z6Mkoqb… --role application --contexts agent-memory --label vta-agent-memory

Then, back here:

  vta-agent-memory connect

The temporary DID above is rotated away on that first connect, so it stops
being an authenticator once it has done its job.
```

That last line is the point. This is the same
**ephemeral `did:key` → ACL grant → auto-rotate on first connect** flow the
mediator, PNM and the DID-hosting services use, driven through the shared
`vti_secrets::IntegrationOnboarding` helper rather than reinvented:

- **`init` needs no credential.** It mints a throwaway key, parks it in the
  session store, and prints the grant. Nothing authenticated is called.
- **The grant runs anywhere** — another laptop, CI, a colleague reading it off a
  ticket.
- **`connect` rotates.** On the first successful authentication the session
  store atomically swaps the throwaway key for a fresh one, mirrors the ACL
  entry onto the new DID, and drops the temp one. The DID that travelled through
  a low-trust channel is no longer live.
- **No private key is ever written to this tool's config.** It lives in the
  session backend — the OS keyring, normally — and the config records only which
  session to open.

`connect` then proves the rotated identity can actually read the context before
writing anything.

### Or, if you hold admin on this machine

```bash
bin/vta-agent-memory setup              # your default `pnm` VTA
bin/vta-agent-memory setup --vta did:webvh:…   # a specific one, by DID
```

`setup` does both phases at once, making the grant itself through your `pnm`
login. It runs the **same** onboarding and the same rotation — the convenience
path is not the less safe one. It does need an operator credential *here*,
though, so prefer `init` + `connect` anywhere that matters. `--vta` also accepts the local `pnm`
name, but prefer the DID: a `pnm` name is a nickname chosen on one machine and
means nothing on any other.

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
| Mint, park, rotate | `vti_secrets::IntegrationOnboarding` — the flow the mediator, PNM and DID-hosting share |
| Find its way in | `resolve_vta_endpoint` against the VTA's DID document |
| Authorize it | `acl/create`, role `application`, scoped to one context |
| Hold the key | the SDK session store (OS keyring), never this tool's config |
| Prove it | a real `vta/memory/list/0.1` **as the rotated identity** |

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

`dirs::config_dir()/vta-agent-memory/config.json` (override with
`$VTA_AGENT_MEMORY_CONFIG`), written `0600`. It holds **no key material** — the
key lives in the session store — only which session to open:

```jsonc
{
  "version": 1,
  "contextId": "agent-memory",
  "identity": {
    "serviceName": "vta-agent-memory",
    "sessionKey": "agent:agent-memory",
    "sessionsDir": "…/vta-agent-memory",
    "vtaDid": "did:webvh:…",
    "operatorLogin": false
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
