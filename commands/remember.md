---
description: Save something to your VTA-backed memory
argument-hint: [what to remember]
---

Save this to the user's VTA-backed memory: $ARGUMENTS

Follow the `agent-memory` skill. In particular:

1. `memory_recall` first — if an existing memory covers this, update that one
   (same name) rather than creating a near-duplicate.
2. Pick the type deliberately: `user`, `feedback`, `project`, or `reference`.
3. Write the description as the answer to "would I want this loaded at the start
   of a session?" — recall ranks on it.
4. For `feedback` and `project`, put the **why** in the body.
5. Convert relative dates to absolute ones.

If the argument is empty, look at what has happened in this session and propose
what is worth remembering — then ask before saving. If the thing to remember is
a secret, or state something else depends on, say so instead of saving it.

Report back with the key you saved and the description, in one or two lines.
