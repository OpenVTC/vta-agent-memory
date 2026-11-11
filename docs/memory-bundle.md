# Memory bundle format

A **memory bundle** is a signed JSON file holding some or all of the memories in
one trust context. `vta-agent-memory export` writes one, and
`vta-agent-memory import` reads one. This page describes the format so that
software other than this crate can read and check a bundle. You don't need a VTA
or this crate to do that.

## Example

```json
{
  "type": "VtaAgentMemoryBundle",
  "bundleVersion": 1,
  "exportedBy": "did:key:z6Mk…",
  "exportedAt": "2026-11-11T09:30:00+00:00",
  "sourceContext": "home",
  "filter": { "basis": ["declared", "verified"] },
  "records": [
    {
      "key": "feedback/tabs",
      "version": 7,
      "record": {
        "v": 2,
        "name": "Tabs",
        "type": "feedback",
        "description": "indent with tabs",
        "body": "…",
        "links": [],
        "updatedAt": "2026-10-02T08:00:00+00:00",
        "basis": "declared",
        "confidence": 0.9,
        "reviewAfter": "2030-01-01T00:00:00+00:00",
        "author": "did:key:z6Mk…",
        "authorKind": "operator",
        "contextId": "home",
        "key": "feedback/tabs",
        "proof": { "type": "DataIntegrityProof", "cryptosuite": "eddsa-jcs-2022", "…": "…" }
      }
    }
  ],
  "proof": { "type": "DataIntegrityProof", "cryptosuite": "eddsa-jcs-2022", "…": "…" }
}
```

## Top-level fields

| field | meaning |
|---|---|
| `type` | Always `VtaAgentMemoryBundle`. |
| `bundleVersion` | `1`. A reader must refuse a version it doesn't know. |
| `exportedBy` | The DID that signed the bundle. Must be the signer of `proof`. |
| `exportedAt` | When the bundle was made (RFC 3339). |
| `sourceContext` | The trust context the memories came from. Every record's own `contextId` must equal it. |
| `filter` | What the export was narrowed to: `types`, `basis` and/or `status` lists. `{}` means everything that could be exported. A non-empty filter tells the reader the bundle is a subset. |
| `records` | One entry per memory: `key` (`<type>/<slug>`), `version` (the source VTA's version number, for information only) and `record`. |
| `proof` | The exporter's Data Integrity proof over everything above it. |

## The record

`record` is the memory **exactly as it was stored**, including its author's own
proof. Exporting never re-signs a record. Its fields are:

- **Content**: `name`, `type` (`user`, `feedback`, `project` or `reference`),
  `description` (one line), `body`, `links` and `updatedAt`.
- **Provenance**: `basis` (`declared`, `observed`, `verified` or `inferred`),
  `confidence` (0 to 1, the author's own estimate; never compare it across
  authors) and `evidence` (citations, each with a `kind` of `credential`,
  `memory` or `record`). This is the same vocabulary as Data Rooms records.
- **Lifecycle**: `status` (absent means `active`; otherwise `deprecated`,
  `disputed` or `retracted`), `statusReason`, `validUntil` and `reviewAfter`.
  The last two are RFC 3339 dates.
- **Authorship**: `v` (`2` for signed records), `author`, `authorKind`
  (`agent` or `operator`), `contextId`, `key` and `proof`.
- **`supersedes`**: where the version this one replaced is kept in the source
  context's history. History itself is not exported.
- **`importedFrom`**: present only if the record was itself imported into the
  source context. See below.

A reader should ignore fields it doesn't know about.

## Checking a bundle

A bundle is accepted only if all of the following hold. If any check fails,
reject the whole bundle. `import` writes nothing from a bundle that fails a
check.

1. `type` and `bundleVersion` are as above.
2. The bundle's `proof` verifies over the bundle with `proofPurpose`
   `assertionMethod`, and its signer is `exportedBy`. This check catches a
   record that was added, removed, edited or moved between bundles.
3. For each record:
   - Its `proof` verifies over the record as it appears in the bundle, and
     `author` is the signer.
   - `contextId` equals `sourceContext`, and `key` equals the entry's `key`.
   - The key is `<type>/<slug>` and matches the record's own `type` and
     slugified `name`.
   - Unsigned records are not accepted.
4. No key appears twice.

Proofs are W3C Data Integrity proofs using `eddsa-jcs-2022` (or
`ecdsa-jcs-2019`), canonicalised with JCS (RFC 8785). That means a bundle can be
re-indented or have its keys reordered without breaking any signature. This
crate resolves only `did:key` verification methods, so checking a bundle needs
no network access.

Steps 2 and 3 do different jobs. The record proofs say who wrote each memory;
the bundle proof says which memories belong together. Each check on its own
leaves a gap: a forger can re-sign a bundle around edited records, or present
genuine records in a bundle with some of them taken out.

## Imported records

A record's signature names the context it was written in, so an imported record
cannot keep that signature as its own: in the new context it would read as
having been moved there. `import` therefore writes a new record signed by the
importer for its new location, and keeps the original, verbatim, in
`importedFrom`. To accept a record that carries `importedFrom`, a reader checks
that:

- The outer record's proof holds for where it was found. The outer record is
  the importer's.
- `importedFrom` is a signed record whose `author` is its signer, whose `key`
  matches, and which does not itself contain an `importedFrom`.
- The content of the two records is the same. Content here is everything except
  `v`, `author`, `authorKind`, `contextId`, `key`, `proof`, `supersedes` and
  `importedFrom`.

If all of that holds, the memory is attributed to the **original** author, and
the importer and the original context are shown alongside. An importer can't
change what a memory says without breaking that attribution.

Importing a memory that was itself imported carries the first original rather
than nesting another layer. The original author stays the author however many
times a memory moves. Saving or changing the status of an imported memory drops
`importedFrom`: from then on the person who saved it is the author, and the
imported version is kept in history.

## What is left out of a bundle

- **Refused records**, whose signature doesn't hold, and **unattributed
  records**, written before records were signed. Neither has an author a
  receiver could check. `export` lists them. Saving an unattributed memory again
  signs it.
- **History.** A bundle holds the current version of each memory.

## Commands

```
vta-agent-memory export [--out FILE] [--type T]… [--basis B]… [--status S]…
vta-agent-memory import FILE|- [--replace] [--dry-run] [--type T]… [--basis B]… [--status S]…
```

Each filter option narrows the set. You can repeat an option or give it a
comma-separated list. `unstated` selects records with no `basis`. `--out`
writes the file readable by its owner only. A bundle holds your memories, so
treat it like any other private file.

`import` checks the whole bundle before it connects to a VTA. Without
`--replace`, a memory that already exists under the same key is left alone.
With `--replace`, the import becomes a new version of that memory and the
replaced version goes to history.
