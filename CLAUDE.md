# CLAUDE.md — vta-agent-memory

A Claude Code plugin that makes a Verifiable Trust Agent the user's memory
service. One Rust binary (MCP server + CLI) plus the plugin manifest, skill,
commands, and hook around it.

> This file is guidance for people **working on this repo**, and is deliberately
> not plugin context. `claude plugin validate` warns that a root `CLAUDE.md` is
> not loaded for users of the plugin — correct, and intended: what users need is
> in `skills/agent-memory/SKILL.md`, which is loaded.

## Layout

```
.claude-plugin/plugin.json   plugin manifest
.mcp.json                    launches bin/vta-agent-memory serve
hooks/hooks.json             SessionStart -> `recall --format json`
skills/agent-memory/         WHEN to save and recall — the policy layer
commands/                    /remember /recall /forget /memories
bin/vta-agent-memory         COMMITTED shim — execs the real cargo-installed binary
scripts/install.sh           cargo install
src/
  lib.rs       the crate is a library too, so tests/ can reach it
  enrol.rs     two-phase enrolment (init/connect) — the primary path
  pnm.rs       resolving which VTA, and where its pnm session actually lives
  lazy.rs      connect on first use, so the server exists when the VTA doesn't
  main.rs      CLI: serve | setup | recall | list | forget | export | import | rooms | doctor
  setup.rs     the online provisioning flow
  config.rs    ~/.config/vta-agent-memory/config.json (0600)
  store.rs     typed ops over vta/memory/* — the only VTA-facing code
  record.rs    the record shape, key encoding, and recall ranking
  authorship.rs  signing records on save, verifying author + location on read
  provenance.rs  basis / confidence / evidence: vocabulary, checks, rank weight
  lifecycle.rs   status, validUntil/reviewAfter, history keys, standing
  bundle.rs      the signed export/import bundle (docs/memory-bundle.md)
  rooms.rs       recall across Data Rooms: read, verify the in-body signature, rank
  server.rs    the nine MCP tools
tests/
  common/                    the in-process fake VTA + signers, shared
  memory_roundtrip.rs        Store end-to-end against an in-process fake VTA
  bundle.rs                  export -> import, and tampered bundles refused
  rooms_recall.rs            recall across memory + rooms; bad room signatures refused
  eval.rs, eval/scenarios/   memory quality evaluation — scored scenarios
  mcp_starts_without_a_vta.rs  the real binary, over stdio, with no VTA
  hook_contract.rs           the SessionStart hook's contract
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

**Ranking, provenance and lifecycle changes go through the evaluation.**
`cargo test --test eval` replays the scenarios in `tests/eval/scenarios/` and
scores them; a failed hard check fails the build, and CI posts the report to the
job summary. Change a weight and the numbers move — say in the PR how. A fix
for a listed *known gap* should make its probe hard. A new behaviour gets a
scenario, and an empty-`query` probe if it is about what must *not* surface:
with a query, a zero score hides things that the session-start hook would show.

## Non-negotiables

**Nothing here is transport-aware.** `VtaClient::memory_{put,list,delete}` drive
the generic Trust-Task dispatcher, which picks TSP > DIDComm > REST from what
both DID documents advertise. If you find yourself branching on transport
anywhere outside `doctor`'s diagnostic print, something has gone wrong.

**Never wrap a `VtaClient` call in a retry loop.** Retry has exactly one owner
per failure domain; for operation completion that owner is
`VtaClient::idempotent`. A hand-rolled loop cannot hold an idempotency key
stable across attempts, so it turns one retried operation into two operations.

**`serve` must never fail to start.** Claude Code launches it at session start
and takes what it gets: a process that exits before speaking MCP shows up as *no
memory tools*, not as a broken memory service, and the model then cannot say
what is wrong because the tool it would say it with is gone. Connection happens
on first use (`crate::lazy`), failures reach the model as tool errors, and
`memory_context` deliberately answers without connecting — it is what somebody
reaches for *because* memory is failing.

Note the connect future is **not `Send`** (the session rung boxes an error
across an await inside `SessionStore`), so it is driven on the blocking pool via
`Handle::block_on`. Awaiting it directly in a tool handler does not compile.

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

**A `pnm` session lives under the keyring key `vta:<slug>`, not `<slug>`.**
Neither session backend prefixes anything (`vta_keyring_key` in
`pnm-cli/src/config.rs` is where the prefix is added), so passing a bare slug
finds no session and reports "Not authenticated" to somebody who is
authenticated. `ResolvedVta::session_key` is the single place that applies it —
do not build the key anywhere else.

**A VTA is identified by its DID, not by a `pnm` name.** The name is a nickname
chosen on one machine and meaningless on any other; every user-facing message
and every stored config records the DID. `crate::pnm::PnmConfig::resolve`
accepts either and normalises.

**What `pnm` was configured with beats DID-document discovery.** `pnm` records
`mediator_did` / `url` exactly for VTAs that *cannot* advertise a service
endpoint, so preferring `resolve_vta_endpoint` would fail setup for precisely
the deployments the operator had already described.

**`bin/vta-agent-memory` is a committed shell shim, not a build artifact.**
`.mcp.json` and `hooks.json` invoke `${CLAUDE_PLUGIN_ROOT}/bin/vta-agent-memory`
because Claude Code's launch environment need not match a shell — but when the
plugin is installed from a marketplace that directory is a fresh clone with no
compiled binary in it. The shim resolves the real one (`$VTA_AGENT_MEMORY_BIN`,
then `~/.cargo/bin`, then a local `target/`, then `PATH`). Do not gitignore it,
and do not replace it with a copied binary — that is what was broken.

Its exit code differs by subcommand on purpose: `recall` exits 0 (the
SessionStart hook must never fail a session), everything else exits 1.

**A plugin-visible fix must bump `version` in `.claude-plugin/plugin.json`.**
`claude plugin update` compares that version, not the commit, so a fix pushed
without a bump is unreachable by every install that already exists — it fetches
the new commit into the marketplace clone, leaves the install pinned to the old
one, and reports "already at the latest version". The shim commit shipped that
way, so the fix for *"a marketplace install does not work"* could not be
installed by anyone whose install did not work. Uninstall + reinstall is the
only way out, and it is not something a user should have to discover. Keep
`Cargo.toml`'s version in step: same artifact, and `plugin.json`'s version names
the install directory.

**Enrolment goes through `vti_secrets::IntegrationOnboarding`, never a
hand-rolled key.** It is the shared ephemeral-`did:key` → ACL-grant →
auto-rotate-on-first-connect flow the mediator, PNM and DID-hosting use. Two
properties fall out and both are load-bearing: the DID that gets pasted into a
ticket stops authenticating after `connect`, and **no private key is ever
written to this crate's config** — it lives in the session store. If you find
yourself putting `privateKeyMultibase` in a config struct, stop.

**The machine holding memories must not need an operator credential.** That is
why `enrol` (init → out-of-band grant → connect) is the primary path and `setup`
is the same-machine convenience. It mirrors the rest of the stack: *"VTC
deliberately never holds a VTA admin credential (no self-grant), same as
mediator / did-hosting."* Phase 1 makes no authenticated call at all — a DID
document is a public read — so it works before anybody has granted anything.

**`pnm`'s config is at `dirs::config_dir()/pnm`, not `~/.config/pnm`.** On macOS
that is `~/Library/Application Support/pnm`. Hardcoding the XDG path told a
macOS user with a working `pnm` that no VTA was configured. `vta-sdk`'s
`agent_connect::default_sessions_dir` still has this bug.

**Setup writes nothing until it has connected as the new identity and listed
memories.** The probe is `memory/list/0.2` specifically, not a cheaper `whoami`:
listing is the operation gated on exactly what setup just arranged.

**Every record is signed by its writer, and nothing is returned as memory until
that is checked.** `Store::save` refuses without a `Signer`; `Store::check` is
the one place a stored value becomes an `Entry`, and a record that fails is a
`Refused`, never an `Entry`. Three rules in `authorship.rs` are load-bearing:
verify over the value **as stored** (a re-serialised struct drops fields a newer
writer signed); a `v >= 2` record with no `proof` is refused, not downgraded to
unattributed; and the resolver is `did:key`-only, so a stored record can never
make recall fetch a URL. The signer is loaded from the session store **after**
connecting — the first connect of an enrolled identity rotates its key.

**A save never overwrites blind.** `Store::save_versioned` reads, archives the
current version to `archive/<key>/v<n>`, then writes with `expectedVersion` =
the version it read; a `versionConflict` is a `SaveOutcome::Conflict` carrying the
(checked) winner, never an error to retry. Do not "fix" a conflict by re-saving
unconditionally — that is the lost update this exists to prevent. History keys
must stay outside every `<type>/` prefix, and `forget` must delete them: "forget"
has to mean forget. A status change is a new version signed by whoever made it;
only `retract` is allowed on a refused record, because anything else would
re-sign content nobody vouches for.

**An import keeps the original author, verifiably.** A record's signature names
the context it was written in, so an imported record is signed by the importer
for where it now is and carries the original verbatim in `importedFrom`;
`authorship::verify` checks both proofs, the key, and that the content is the
same, then attributes the memory to the original author. Do not "simplify" this
by relaxing the location check for imports, or by re-signing as the importer —
the first lets any record be relocated, the second loses who wrote it. Any
later save drops `importedFrom` (`Store::save_versioned`); only `Store::import`
writes it. Opening a bundle is all or nothing.

**Room records are checked here, not by the VTA.** `rooms/keys/read` reports
what the VTA checked about the *host*; the author's in-body signature is the
reader's (`rooms::verify_body`, the rules of `vtc_client`'s
`verify_record_body`). Verify the location at the *sealed* version
(`ext.org.openvtc.rooms.sealedAt`), not the record's own: a curation moves the
version past the body. Do the location and host-author checks before
resolving the author. Room records may be verified with a network resolver
(members are often `did:webvh`); personal records stay `did:key`-only. Recall
never writes to a room, and the session-start hook does not read rooms: a slow
host must not delay a session.

## The retrieval constraint

`vta/memory/list/0.2` pages a context in ascending key order and can narrow by
key prefix, but has no search: ranking happens here. (0.1, which this crate no
longer sends, returned the whole context with no prefix at all.) Everything in
`record.rs` and `store.rs` follows from that:

- Keys are `<type>/<slug>` so a type filter runs on the raw key, before decode
  (`MemoryKey::raw_key_has_type` / `Store::list_of_type`).
- Recall returns `summary()` (key, name, type, description), never `full()`.
  Bodies come from `memory_get` alone.
- `rank()` multiplies relevance by `provenance::weight(basis)`. Never rank on
  `confidence`: the rooms schema it comes from says a reader MUST NOT compare it
  across authors. Keep `basis`/`evidence` as strings/raw values in the record —
  a typed field would make a newer writer's record undecodable.
- `rank()` is weighted whole-word matching, plurals folded, query stopwords
  dropped. Not substring containment: that let "run" match "runner". Deliberately crude; see its doc
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
