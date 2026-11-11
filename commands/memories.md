---
description: Show everything in your VTA-backed memory
argument-hint: [--stale]
---

Show the user what is in their VTA-backed memory.

Run `memory_context` and `memory_list`. Report the trust context the memories
live in, then the memories grouped by type, as name + description — not full
bodies.

`memory_list` returns one page (50 by default) and a `total`. If `nextOffset` is
set there are more: say how many in all, and fetch further pages with `offset`
only if the user wants to see them.

Show each memory's `status` when it is not `active`, and mark ones that are
`stale` or `expired`.

If the arguments include `--stale`, call `memory_list` with `stale: true`
instead, and for each memory it returns — due for review, or past its
`validUntil` — ask the user whether it still holds. Then, per their answer: save
it again (optionally with a new `reviewAfter`), `memory_deprecate` it, or
`memory_retract` it. Do not change any of them without asking.

If the list is empty, say which context you looked in, since an empty result
usually means the wrong context rather than no memories.
