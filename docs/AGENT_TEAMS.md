# Agent teams in maki — a design sketch

A sketch of what it would take to give maki something like [Claude Code's agent
teams](https://code.claude.com/docs/en/agent-teams.md): several agents working at
once, addressable by name, messaging each other, coordinating through a shared
task list.

Nothing here is implemented. This is a design record written after reading the
code, so that whoever picks it up starts from what is actually there rather than
from what seems likely. Line references are to the commit this was written
against and will drift; the type and function names are the durable part.

## The short version

**maki already has the runtime and the panel.** It is missing names, a shared
task list, and the tools to drive them. Almost all of the gap can be closed in
Lua.

The one design decision that matters: **a teammate should be a top-level session,
not a persistent subagent.** Everything else follows from that.

## What already exists

`maki-ui/src/event_loop.rs` opens with a description that is close to a
specification for this feature:

> Multi-session supervisor: every session owns an `App` + `AgentHandles` and
> keeps draining agent events while backgrounded; only the focused session
> renders and receives input.

Backgrounded sessions are not parked. `next_wake` builds a `flume::Selector` over
*every* runtime's channels, `handle_agent` routes by index rather than by focus,
and `checkpoint_all` persists every session on change each frame.

| Claude Code concept | maki today | gap |
|---|---|---|
| Team | `EventLoop.sessions: Vec<SessionRuntime>` | none — it *is* the team |
| Teammate | a top-level session: own `MakiId`, `App`, `AgentHandles`, model, history | not addressable by name |
| Spawn a teammate | `SessionRequest::New { prompt, focus }` — spawns, submits a prompt, focuses only if asked | not exposed to the model |
| Mailbox (a JSON file per agent) | `SessionMailbox` + a process-global `MAILBOXES` registry, in memory | no sender field |
| Automatic delivery | `start_mailbox_runs` → `claim_idle_wake`, every loop pass | works |
| Idle notification | `SessionStatusChanged` autocmd, `{session_id, title, status, focused}` | already emitted |
| Agent panel | `plugins/sessions/init.lua` | already ships |
| Shared task list | `todo_write`, per session | **missing** |
| File locking for task claims | not needed — one process, one event loop | — |

`/sessions` is worth looking at before designing anything new. It already renders
one row per session with a status icon (`◆` needs input, a spinning `·` for
working, `●` focused, `○` live), host-animated spinners, a footer tally of
`◆ N needs input · ● N working`, ranks frozen while the picker is open so rows do
not jump under the cursor as background agents progress, and a
`SessionStatusChanged` autocmd that flashes `◆ <title> needs input · /sessions`
for sessions you are not looking at.

That is an agent panel. It exists.

## Why a teammate is a session, not a persistent subagent

The instinct is to make `task`'s subagents long-lived. The code says no, and the
reasons are worth recording because none of them are visible from Lua.

**A subagent is cancelled when the turn that created it ends.** Its
`child_cancel` descends from the per-run token via `agent_ctx.cancel.child()`.
`CancelTrigger` fires on `Drop`, and `AgentLoop` drops it unconditionally after
every run through `clear_cancel_trigger` → `CancelMap::remove`. So a
`maki.agent.session` handle stashed in a module-level table survives perfectly
well — one Lua VM, upvalues live for the process — and then returns `Cancelled`
on its second `:prompt()`. This is the single most surprising fact in this
document.

**It would also be invisible.** `sub_event_tx` carries the creating run's
`run_id`, and the UI drops any envelope whose `run_id` does not match the current
one. A survivor's `TurnComplete`s and its final `SubagentHistory` are discarded
silently.

**It cannot be addressed.** A subagent's only identity is `ui_id`, which is the
parent's `tool_use_id` and is *deliberately* non-unique — sibling sessions from
one tool call share it, and `CancelMap` is built to keep them under one key.
Meanwhile `notify` takes a `MakiId`.

**And the obvious fix is a trap.** `SessionMailbox::register` is idempotent by id,
and a subagent inherits its *parent's* `SessionRef`. Changing `mailbox: None` to
`SessionMailbox::register(agent_ctx.session_id.id())` would upgrade the parent's
live entry and hand the subagent a shared inbox, so it would quietly consume
observations meant for the parent. A silent correctness bug, not a missing
feature.

**Nothing would ever wake it.** The only claimant for `claim_wake` in the process
is `EventLoop::start_mailbox_runs`, which iterates top-level runtimes.

**And it would not survive a reload.** Restoring a session always marks subagent
chats finished; nothing in-flight comes back. Top-level sessions restore fully
through `ShutdownReport.tabs`. This asymmetry is the strongest single argument: a
long-lived agent that must outlive a reload can be a session and cannot be a
subagent chat.

To be fair to the alternative: a persistent subagent is not architecturally
impossible. It is three couplings deep — cancel parentage, `run_id` stamping,
mailbox identity — each a local change. The accurate objection is not "impossible"
but "it would have to re-acquire run-independent identity, cancellation and event
routing, which is what a top-level session already has."

Related, in case it looks like an escape hatch: the `plugin-owned jobs` feature
(`maki.fn.jobstart({ owner = "plugin" })`) *is* the only thing in the codebase
that outlives its tool call, but a job is an OS subprocess (`bash -c`, its own
process group), not an agent. It is the right primitive for a long-lived
*external* process, and no help here.

## The design

A Lua plugin, `plugins/team/init.lua`. This matches maki's own split — the task
plugin's header says "this plugin owns structured output and subagent
concurrency; Rust exposes primitives only".

```lua
-- audiences = { "main" }
--
-- Because teammates ARE top-level sessions, they run under MAIN and so get
-- these tools too. Teammates messaging each other -- the one property that
-- separates a team from a fan-out -- falls out for free.

team_spawn { name, prompt }   -- maki.session.new{ prompt, focus = false }
                              -- + maki.session.set_title(id, name)
team_send  { to, message }    -- resolve name -> id from maki.session.live()
                              -- + maki.session.notify(text, {session=id, wake=true})
team_list  {}                 -- maki.session.live() -> {id,title,status,updated_at,focused}
task_add / task_claim / task_done / task_list
```

Build on `maki.session.*`, not `maki.agent.*`. `maki.agent.*` requires
`Caps::Handler` and is therefore unreachable from slash commands, autocmds,
keybinds and job callbacks. `maki.session.*` needs no `ctx` and works everywhere.

**The coordinator belongs in a `/team` slash command.** Slash-command handlers run
through `run_detached` with no deadline and no cancel token, which is exactly why
`plugins/sessions/init.lua` can hold a `board.win:recv(TICK_MS)` loop open for the
life of the process. Note that autocmd and job callbacks are *synchronous* and
cannot do the async `live()` round-trip; `sessions/init.lua` handles this by
having the autocmd set a dirty flag the loop picks up.

**Idle notifications need no polling.** `SessionStatusChanged` already fires on
every transition with `working | needs_input | idle`.

### The shared task list is a plain table

There is one Lua VM per process — `PluginHost` is constructed once in
`build_stack`, and `Stack` is documented as "one generation of the app:
everything torn down and rebuilt on `/reload`". Every session gets a clone of the
same `EventHandle`; there is no per-session `PluginHost` anywhere. So a
module-level table is shared mutable state across every session. `task`'s
process-wide concurrency semaphore already relies on this.

Two consequences:

- **No files, no locking.** Claude Code needs `~/.claude/tasks/{team}/` with file
  locking because its teammates are separate OS processes. maki's are coroutines
  in one VM.
- **Claiming is atomic if it does not await.** The VM is single-threaded and
  cooperative; coroutines interleave *only* at await points. A synchronous
  read-check-write claim needs no lock. A claim that spans an `await` can
  interleave, and must be written knowing that.

Caveat: module state dies on `/reload`, which rebuilds the `Stack` and joins the
Lua thread. If tasks must survive that, they need `maki.fs.atomic_write` — and
then the race question returns, because crash recovery reintroduces it.

## What needs Rust

For a TUI-only prototype: **nothing**. That is the headline finding.

Four things worth doing anyway, in rough priority order:

1. **Sender attribution on `notify`.** See below — the most important one.
2. **A user bubble on the idle-wake turn.** `start_mailbox_run` starts a turn with
   empty message text *and* an empty display string, so a teammate receiving a
   message appears to start a turn from nothing.
3. **Mailbox overflow is silent.** At `MAILBOX_CAPACITY` (100) the oldest message
   is dropped with no signal.
4. **A non-TUI session supervisor**, if teams should work outside the TUI. This is
   the only genuinely architectural item, and it is optional.

### Sender attribution, in detail

`notify` pushes `Message::observation(text)`, and `MessageKind::Observation` is
**host-side only**. It hides the message from the TUI transcript and from ACP
user-message updates, and it is persisted in maki's session JSON — but it never
leaves the host. The Anthropic wire type is `WireMessage { role, content }`. On
the wire the model receives a plain `role: "user"` text block containing exactly
the string the plugin passed, indistinguishable from the human typing it.

The intent is stated in the `types.rs` module doc — "an observation comes from
outside, belongs in model context, and must never be mistaken for the user
talking" — but enforcement is display-side, not model-side.

A plugin can get most of the way there without Rust: a tool handler can call
`ctx:session_id()`, which returns the id of the session that *actually* invoked
it. Its own doc comment is on point: "the session that called this tool, which
under concurrent sessions is not always the focused one `maki.session.current()`
reports." So `team_send` can stamp the true sender rather than trusting a `from`
field in the tool input, and a teammate cannot lie about who it is through the
tool interface.

The residual gap is narrow but real: the stamp lands in the message *text*, and a
model can write a convincing lookalike inside its own message body, so a receiver
cannot distinguish a plugin stamp from a forged one. A structural field on
`notify` is the difference between "hard to forge" and "impossible", and it
matters because the failure mode is a teammate claiming the user approved
something.

## Limitations to decide about, not discover

**Teams would be TUI-only.** `EventLoop` exists only in the interactive UI. Under
headless, ACP and the SDK, every `maki.session.*` call returns "no interactive UI
attached" — except `notify`, which bypasses the loop and calls
`SessionMailbox::notify` inline.

**Permissions differ from Claude Code, deliberately.** Each runtime *forks* its
`PermissionManager` (commented "Prototype only: every runtime forks its own
manager so session rules stay per-session"), so a teammate does not inherit the
lead's rules, and its prompts surface in its *own* session — appearing as
`◆ needs input` in the panel rather than bubbling to the lead. That is arguably
better than the alternative, but it means answering a prompt requires switching
to that agent. Pick deliberately.

**A wake never interrupts a running turn.** `claim_idle_wake` returns nothing
unless status is exactly `Idle`, so a message to a busy teammate queues until its
turn ends. This is the desirable behaviour; it is recorded here so nobody
"fixes" it.

**Nested teams work naturally**, where Claude Code forbids them — teammates are
sessions with the same tools. Decide whether to cap depth.

**A top-level session is heavyweight.** `spawn_runtime` builds a full `App` —
chats, float manager, input box, pickers, status bar. There is no lightweight
`SessionRuntime`.

## Side quests found on the way

Small, unrelated to the design, worth sweeping up:

- `SessionStatusChanged` is fired by the event loop but missing from the
  documented event list in `maki-lua/src/api/autocmd.rs`.
- `register_tool`'s docstring lists an audience `"sub"` that does not exist. The
  real set is `main`, `research_sub`, `general_sub`, `interpreter`, `workflow`.
- `register_tool`'s `restore` docstring gives the wrong argument list.
- The tool-result docstring claims `content` is a legacy alias for `llm_output`;
  `coerce_tool_result` reads only `llm_output`.

## If you only remember three things

1. A teammate is a **top-level session**. Subagents die with the turn that made
   them, in four independent ways.
2. The shared task list is a **plain Lua table** — one VM per process — and
   claiming needs no lock as long as it does not await.
3. `MessageKind::Observation` **never reaches the model**, so cross-agent messages
   look like the user talking. Fix that before anyone builds on this.
