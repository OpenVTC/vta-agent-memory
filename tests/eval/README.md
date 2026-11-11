# Memory quality evaluation

```bash
cargo test --test eval                  # every scenario, markdown report
cargo test --test eval -- poisoning     # by dimension or name substring
EVAL_REPORT_JSON=eval.json cargo test --test eval   # per-probe detail as JSON
```

Each file in `scenarios/` is one scripted, multi-session scenario run against the
real `Store` — signing, verification, ranking, lifecycle — over the in-process
fake VTA in `tests/common/`. Steps run in order; probes interleave with them, so
a scenario can probe before and after a repair.

A failed **hard** check fails the run, and so `cargo test` and CI. A probe
marked `"soft": true` is scored and listed under *Known gaps* but does not fail
anything: that is how a weakness is kept visible until it is fixed. When you fix
one, make its probe hard.

## Dimensions

| Dimension | Asks | LongMemEval analogue |
|---|---|---|
| `precision` | Does a task-shaped query surface the memory that answers it, first? | information extraction |
| `staleness` | Do superseded, expired and deprecated facts lose to current ones? | knowledge updates, temporal reasoning |
| `contradiction` | When memories disagree, does what the user said (or what is evidenced, or undisputed) win? | knowledge updates |
| `minimisation` | Does an unrelated question get nothing, and does recall stay within its limit? | abstention |
| `repairability` | How many operations does correcting a wrong memory take? | — |
| `poisoning` | Are tampered, relocated or stripped records refused, does the fence hold, and does keyword stuffing fail to outrank? | — |
| `sources` | Across personal memory and Data Rooms: one ranking, `sources` narrowing, room curation respected, bad room signatures refused | — |

## Metrics

Per probe, against its hand-labelled `relevant` keys (only where it has some):

- **precision** — the share of what was returned that is relevant. An empty
  result has precision 1.
- **recall** — the share of the relevant keys that were returned.
- **MRR** — 1 / rank of the first relevant key; 0 if none was returned.

Precision is deliberately strict: a contradiction scenario returns the losing,
demoted memory too (it should stay visible for review), and that counts against
precision. Read the hard checks for whether the right thing won; read the
metrics for how much else came with it.

**repair ops** is the number of steps flagged `"repair": true`; a scenario's
`maxRepairOps` fails it if the fix took more.

## Format

```jsonc
{
  "name": "staleness-superseded-fact",
  "dimension": "staleness",
  "description": "What this checks and why.",
  "writers": ["laptop", "phone"],      // optional; the first one recalls. Default ["agent"]
  "maxRepairOps": 1,                   // optional
  "rooms": ["deals"],                  // optional: Data Rooms recall reads, by label
  "steps": [
    { "save":   { "as": "phone", "type": "user", "name": "Editor",
                  "description": "…", "body": "…",
                  "basis": "declared", "confidence": 0.6, "evidence": [ … ],
                  "validUntil": "2020-01-01T00:00:00+00:00", "reviewAfter": "…",
                  "repair": false } },
    { "status": { "key": "user/editor", "status": "disputed", "reason": "…", "repair": true } },
    { "forget": { "key": "user/editor" } },
    { "tamper": { "key": "user/editor", "set": { "body": "…" }, "remove": ["proof"] } },
    { "plant":  { "key": "user/x", "value": "raw string or JSON object" } },
    { "copy":   { "from": "user/editor", "to": "user/other" } },
    { "room":   { "room": "deals", "as": "alice", "key": "r1",
                  "title": "…", "description": "…", "body": "…", "basis": "observed",
                  "status": "active",            // or deprecated, retracted
                  "validUntil": "…",
                  "open": false,                 // an open room: cleartext, unsigned
                  "alter": { "description": "…" },  // changed after signing
                  "servedAs": "r2",              // served under another key
                  "hostAuthor": "bob" } },       // host records someone else as author
    { "probe":  { "label": "…", "query": "…", "type": "user", "limit": 8,
                  "relevant": ["user/editor"],
                  "top": "user/editor",          // must rank first
                  "absent": ["user/old"],        // must not be returned
                  "expectEmpty": false,          // must return nothing
                  "refused": ["user/forged"],    // must be reported refused
                  "stale": ["user/editor"],      // must come back flagged stale
                  "describe": { "user/editor": "helix" },  // description must contain
                  "fence": false,                // rendered output keeps exactly one fence
                  "sources": ["personal"],       // or "rooms", or room labels; default all
                  "soft": false } }
  ]
}
```

Probes take the path `memory_recall` takes: personal memory and the
scenario's rooms, ranked together. A room record is named
`room:<label>/<key>` in `relevant`, `top`, `absent`, `refused` and
`describe`; personal memories keep their own keys. Room records are versioned
in step order and signed by a fresh `did:key` per member named in `as`.

Dates in `validUntil` / `reviewAfter` are stored as given, so write normalised
RFC 3339 (`2020-01-01T00:00:00+00:00`). Probe with an empty `query` too — that
is the session-start hook's path, and the one place a zero relevance score does
not hide a memory that should not be there.
