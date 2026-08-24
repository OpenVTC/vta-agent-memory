---
description: Permanently delete a memory
argument-hint: [memory key or description]
---

Forget this memory: $ARGUMENTS

This is permanent — there is no undo and no grace window.

If the argument is already a key (`type/name`), confirm what it is with
`memory_get` and show the user before deleting. If it is a description, use
`memory_recall` to find candidates and confirm which one they mean — do not
guess when more than one matches.

If they have asked to forget everything, run `memory_list` first, show them what
is there, and confirm before deleting anything.
