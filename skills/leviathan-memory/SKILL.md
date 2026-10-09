---
name: leviathan-memory
description: Long-term memory across sessions with the Leviathan memory store. Use at the start of a session to recall what is known about the user, the project and past decisions; before acting on anything you may have learned before (preferences, conventions, owners, deploy steps, past failures); and whenever you learn something durable that a later session should know, or the user says "remember", "from now on", "I prefer", "we decided", or corrects an earlier fact.
---

# Leviathan memory: remember once, recall in a few tokens

The `leviathan memory` CLI is a small database of claims, one per row. A
recall returns only what fits a token budget, current values first, so it
stays cheap however much has been remembered. If the `recall` and
`remember` MCP tools are available, use them instead; they take the same
arguments.

## Start of a session

```bash
leviathan memory recall                     # the briefing: pinned and important memories across subjects
```

## Before acting

```bash
leviathan memory recall <words as the user said them>
leviathan memory recall -s <subject>        # everything current about a person, project or service
leviathan memory recall --kind decision,lesson deploy
```

An empty answer means *not remembered*, not *untrue*. Try other words or a
subject, or ask the user.

## Remember

```bash
leviathan memory remember "Prefers pnpm over npm" --kind preference -s josh -k package_manager
leviathan memory remember "Deploys go through staging first, after the March outage" --kind decision -s deploys -k path -i 4
leviathan memory remember "The flaky auth test fails when run before the cache warms" --kind lesson -s auth-tests
```

- One claim per call, in one sentence, in words a later question would use.
- Anything that can change gets a subject (`-s`) and a key (`-k`). Writing the same subject and key again **replaces** the old value; that is how corrections work. Don't write a second, contradicting memory without the key.
- Decisions carry their reason. Lessons say what to do differently.
- Remember durable things: preferences, conventions, decisions, owners, environments, recurring failures. Not the conversation, not things the code or git history already shows, not one-off details of the current task.
- `--importance 5` or `--pin` only for what every session must know.
- **Never store secrets.** They are refused (exit 2). Remember where a secret lives, not its value.

The reply says whether the memory was new, merged with a restatement, or
replaced an older value, and lists related memories. If a related one is
now wrong, forget it or overwrite its key.

## Correct and forget

```bash
leviathan memory forget <id> --reason "moved to yarn"
leviathan memory forget -s josh -k package_manager
leviathan memory history -s josh -k package_manager    # every value the slot has had
leviathan memory recall --as-of 2025-06 deploy          # what was true then
```

## Store

The default store is `~/.leviathan/memory.db`; `--memory=PATH` or
`LEVIATHAN_MEMORY` picks another, and `--ns` separates projects. Add `--json`
for structured output. Exit codes: `0` ok, `1` error, `2` refused or bad
request.
