---
description: Show everything in your VTA-backed memory
---

Show the user what is in their VTA-backed memory.

Run `memory_context` and `memory_list`. Report the trust context the memories
live in, then the memories grouped by type, as name + description — not full
bodies.

`memory_list` returns one page (50 by default) and a `total`. If `nextOffset` is
set there are more: say how many in all, and fetch further pages with `offset`
only if the user wants to see them.

If the list is empty, say which context you looked in, since an empty result
usually means the wrong context rather than no memories.
