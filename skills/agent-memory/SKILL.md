---
name: agent-memory
description: How to use the user's VTA-backed memory — what is worth saving, when to recall, and what must never go in. Load whenever the user says "remember", "you always forget", "what do you know about me", or asks you to keep/forget something across sessions; and before saving anything with the memory_* tools.
---

# Agent memory (VTA-backed)

The user's durable memory lives in their own Verifiable Trust Agent, not in this
tool and not in a file in the repo. You reach it through six MCP tools:
`memory_recall`, `memory_get`, `memory_save`, `memory_forget`, `memory_list`,
`memory_context`.

The whole store is scoped to one **VTA trust context**. That context is the
isolation boundary — an agent scoped to a different one cannot read, write, or
delete these memories, and the VTA enforces that, not this tool. So "the
memories" always means "the memories in this context". If the user seems to be
talking about memories you cannot see, run `memory_context` and say which
context you are looking at rather than concluding they have none.

## The recall pattern

`memory_recall` is the cheap call and `memory_get` is the expensive one. Recall
returns ranked *summaries* — key, name, type, one-line description. Read those,
decide which one or two actually matter, and `memory_get` only those.

Do not call `memory_get` on every result. A recall that returns eight summaries
and then eight full bodies has spent the context window you were trying to save.

Recall when:

- The user references a decision, preference, or project you have no record of
  in this session.
- You are about to do something the user has opinions about — how to name
  things, how to open PRs, what to run before committing.
- The user asks what you know or remember.

Ranking is on **name and description**, not body. A memory whose description is
vague is a memory that will not be found. This is the single most important
thing to get right when saving.

## What to save

Four types. Pick the one that fits; the type is part of the key.

| Type | For |
|---|---|
| `user` | Who the person is — role, expertise, standing preferences |
| `feedback` | Guidance on how you should work: corrections *and* confirmed approaches |
| `project` | Ongoing work, goals, constraints not derivable from the code |
| `reference` | Pointers to external resources — URLs, dashboards, tickets |

For `feedback` and `project`, the body should say **why**, and how to apply it.
"Use PRs, not direct pushes" is a rule someone will eventually override; "use
PRs, not direct pushes — main is protected and direct pushes fail CI" is a rule
they can reason about.

Convert relative dates to absolute ones. "Last week" is wrong the moment it is
stored.

Link related memories by name in the `links` field. A link to a memory that does
not exist yet is fine — it marks something worth writing later.

## What not to save

- **Anything the repository already records.** Code structure, past fixes, git
  history, what is in `CLAUDE.md`. If someone could learn it by reading the
  repo, it does not belong here.
- **Anything that only matters to this conversation.** A file path you are
  mid-edit on, a test you are about to run.
- **Secrets.** Tokens, passwords, private keys. The VTA has a secrets vault and
  a credential vault for those, and they are gated differently on purpose. If
  the user asks you to remember a credential, say that memory is the wrong store
  and point at `pnm vault`.
- **Durable application state.** Anything whose loss would break something — an
  account id, a membership, a record another system depends on. "Forget
  everything" has to stay a safe thing for a user to ask, which it stops being
  the moment state like that lives here. The VTA's `app-state` family exists for
  it.

If the user asks you to remember something in one of those categories, say so in
a sentence and offer the alternative rather than saving it anyway.

## Before saving

Recall first. If a memory already covers it, update that one — `memory_save`
with the same name replaces it — rather than creating a near-duplicate. Two
memories that disagree are worse than neither.

Write the description as the answer to: *would I want this loaded at the start of
a session?* If the honest answer is no, do not save it.

## Forgetting

`memory_forget` is permanent. There is no undo and no grace window.

Only forget when the user asks, or when you have just saved a memory that
supersedes the old one. If a memory turns out to be wrong, forgetting it is
right — a wrong memory is worse than a missing one.

If the user asks you to forget *everything*, confirm the scope first, list what
is there with `memory_list`, and only then delete. It is a small number of calls
and it is not reversible.

## Setup problems

If a memory tool fails with a permission error, the agent identity has lost
access to the trust context. Point the user at:

```
vta-agent-memory doctor
```

which reports the context, the identity, the transport, and whether it can
actually read. If they have never set it up:

```
vta-agent-memory setup --vta <their-pnm-slug>
```
