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

---

# Prior art: oh-my-pi

[oh-my-pi](https://github.com/can1357/oh-my-pi) is a TypeScript coding agent that
ships a working version of all of this. Its teams feature is worth reading before
building maki's, and this section records what to take, what to leave, and why.

Read from the fork at `/Users/me/aaa/github/oh-my-pi`, under
`packages/coding-agent/`. Everything below is TypeScript; there is no Rust side.

Structurally it is **three subsystems, not one**, and the split is worth copying
into how maki thinks about it:

| Subsystem | File | Job |
|---|---|---|
| `AgentRegistry` | `src/registry/agent-registry.ts` | the roster — who exists |
| `IrcBus` | `src/irc/bus.ts` | the mailbox — how they talk |
| `TeamBoard` | `src/teams/board.ts` | the task board — how they divide work |

All three are process-global singletons in one process, which is the same shape
as maki's single event loop and single Lua VM. The design sketched above is not
fighting the grain.

## The principle that shows up three times

**Retarget the existing renderer; never build a second one.**

- Live session sharing writes the remote session to a replica file and loads it
  through the ordinary resume path, so "theming, ctrl+o, and transcript behaviour
  are native by construction".
- The agent hub's `Enter` does not open a viewer — it points the *main* view at
  that agent: "the transcript then renders through the regular session pipeline —
  exact parity by construction".
- A guest's typed prompt is not a special channel; it enters as a normal message
  with `streamingBehavior: "steer"`, so "there is no separate queue to reconcile".

maki already has a version of this in `resolve_render_chat`, where the task
picker previews the selected chat through the normal render path. Whatever a
multi-agent view becomes, it should extend that rather than grow a parallel one.

## What to steal

**Branch delivery on the recipient's run state.** Their bridge has three cases,
and maki currently has one and a half:

| Recipient | Delivery |
|---|---|
| busy, sender is the parent | steer the live turn, with a dedicated prompt template |
| busy, sender is a peer | queue as a non-interrupting aside, drained at the **next step boundary** |
| idle | wake it — a real turn |
| idle, plan mode | append and persist, no wake |

maki's `claim_idle_wake` fires only when status is exactly `Idle`, so the wake
path alone cannot reach a busy teammate until its turn *ends*. Not interrupting
is right; waiting for turn end rather than the next step boundary is a
limitation.

**But maki already has the step-boundary hook, and the mailbox just misses it.**
`Agent::turn` polls for queued work mid-turn at `run.rs:355`:

```rust
if self.try_auto_compact().await? || self.handle_queued_command().await? {
    return Ok(TurnOutcome::Continue);
}
```

`handle_queued_command` calls `push_input_context`, which drains the mailbox
(`run.rs:224-233`). So mailbox messages *are* injected at a step boundary — but
only when a queued **user** command arrives at the same time, because the handler
returns early unless `interrupt_source.poll()` yields one:

```rust
let Some(cmd) = source.poll() else { return Ok(false); };
```

A peer message alone never reaches it and waits for the next `Agent::run`. So
this is not a missing mechanism, it is an unwired one: checking the mailbox
alongside the interrupt source at that call site moves peer delivery from
turn-end to step-boundary. Note the asymmetry to preserve while doing it — a
queued user message is wrapped in `<user-interrupt>` and told to be addressed
now; a peer aside should not borrow that framing.

**Return a delivery outcome, not a boolean.** `injected | woken | revived |
failed` is returned to the sending model and rendered with distinct colours. It
tells the sender *how* the message landed, which is exactly what it needs to
decide whether to keep working or block. maki's `notify` returns `(bool, err)`
and already has the states to distinguish these.

**Never buffer a message you successfully delivered.** The recipient's context
*is* the inbox; buffering alongside injection double-delivers on the next drain
and corrupts unread counts. Only a failed hand-off is queued.

**Every coordination failure is a typed result, never a throw.**
`already_claimed`, `blocked` (carrying `pendingBlockers` so the model knows which
prerequisite to check), `not_owner`, `invalid_state`. Losing a claim race is
normal control flow for a teammate, not an error.

**State safety properties as guarantees, not rules.** Their whole board prompt is
eleven lines and reads like a spec — "atomically claim a pending task for
yourself. Fails with a conflict if another agent already claimed it". Enforcement
lives in the board; the prompt only describes what is true.

**Fence the channel from both directions.** "NEVER use shell tools, grep, or read
other sessions' files to figure out what a peer is doing. Message them directly"
*and* "NEVER use hub messaging for something a tool can answer." Without the
second line agents chat instead of working; without the first they snoop instead
of asking.

**Render a work-aware peer roster into the system prompt**, and register a
teammate's identity *before* building that prompt so a batch of siblings spawned
together see each other from their first token. The roster carries a one-line
activity gist per peer, which is what makes "message the peer who owns that file
before you edit it" a realistic instruction.

**Coordination tools are force-injected regardless of a role's tool list.**
Claude Code says the same thing about `SendMessage` and the task tools. Two
systems converging on it independently suggests it is load-bearing.

**A terminal tombstone plus compare-and-swap revival.** `aborted` is one-way and
revival must present the exact ref it was authorized against. This kills the
class of "late async work resurrects a killed agent" bugs, which an in-process
design will hit. maki's `/sessions` merges live rows with a stored disk scan and
has the same latent shape.

**Broadcasts reach live peers only; direct sends revive parked ones.** Sending is
the universal wake primitive, but fan-out must not be, or one broadcast
resurrects every agent you ever had.

**One `wait` that races peer messages, job completion, timeout and abort — with a
liveness abort.** If no running peer remains, fail immediately rather than
burning the full timeout. Kills the everyone-waiting-on-a-dead-teammate hang.

**Sanitize on write *and* on read.** Any same-user process can write the shared
store directly, bypassing the tool's input cleaning, and the raw strings flow
into list output and broadcasts — including dependency ids, which appear verbatim
in error messages. Their tests smuggle an OSC 52 clipboard-write through a
*model-chosen display name*. Escape-injection appears four separate times in this
codebase; they clearly learned it the hard way. maki renders cross-agent text in
a TUI the same way.

**A `useless` flag on tool results.** Zero-match searches, timed-out waits and
empty inbox drains are marked droppable so compaction can discard them.
Coordination polling generates a lot of these.

## The observer, which is the best single idea

A **deterministic, zero-LLM** team health monitor, process-level and keyed on the
global registry "so it sees every agent regardless of spawner, is not gated on
any lead turn, and cannot be killed or parked by the lead."

Seven detectors: stuck, error-loop, parked-stall, board-deadlock, cost-runaway,
token-runaway, all-idle, orphan. Each carries a `fix` string containing the
literal command to type.

Three rungs and nothing else: a dim status notice, then one DM to the agent, then
one OS notification to the human. And the invariant that makes it safe:

> The observer NEVER parks, aborts, releases, or mutates the board — kill/release
> authority stays with the user. Test-enforced invariant.

The test byte-compares the entire board directory before and after a scan.

Details worth copying wholesale: flags clear only after two consecutive absent
scans, because "a flapping pathology re-fires the L1 notice and re-sends the L2
DM on every flap". The board is *polled* rather than subscribed, deliberately —
"no board event feed exists; polling is robust against missed broadcasts". And
`stuck` is skipped entirely while a provider retry is in progress, because that
is an intentional sleep rather than a hang.

## What not to copy

- **Publishing work wakes nobody.** Only claim and complete broadcast; `create`
  does not. Idle peers find out about new work only when someone else claims
  something — and two of the seven observer detectors exist largely to paper over
  that. Broadcast on create.
- **Claim broadcast amplification.** Every successful claim fans out to all live
  peers, and each idle peer that receives it burns a full model turn. N idle
  teammates means N turns per claim.
- **No board GC.** Tasks and team directories accumulate forever; completed tasks
  never leave the list output. Add expiry.
- **No cycle check when creating a dependency** — caught only after the fact, by
  the observer.
- **Disk and file locks for a single-process data structure**, justified only by
  cross-restart resume. Their own reviewer's note: if maki does not need that, an
  in-memory table with a Lua-side mutex drops roughly 250 lines of locking.
- **A documented single-source-of-truth that is not one.** Their row formatter
  claims to be shared between the hub and the panel; the hub kept its own copy
  and the two have diverged.
- **A prompt that disagrees with the code** — the board prompt promises "newest
  context first" while the code sorts oldest-first.

## Two things worth having regardless of teams

**A second model watching the first.** Their advisor gets only the transcript
delta since its last update, with its own already-injected notes filtered out to
prevent it reviewing its own advice. Three severities with genuinely different
delivery: a nit batches in at the next step boundary, a concern steers, a blocker
steers and is not satisfied by a terminal answer. Advice renders as
`<advisory severity="concern" guidance="weigh, don't blindly obey">`.

Its emission guard is what makes it tolerable rather than a nuisance: normalize,
drop content-free phrases (`stop`, `lgtm`, `nothing to add`), dedupe against
everything this advisor has already said, and allow **at most one note per
cycle**. And the rule that matters most: **if the user deliberately interrupted,
it stops auto-resuming** — a late note becomes a card that re-enters context on
the next resume rather than a surprise turn.

**Modal key ownership, stated plainly.** "Once the reader starts typing a
message, the editor owns every key" — scroll keys work only while the input is
empty, and Esc clears a dirty input before it closes. That is the principled form
of the bug that made `e` unusable in maki's `/model` fuzzy search.

## For maki's session picker specifically

`plugins/sessions/init.lua` is already most of an agent panel. These are the
concrete deltas, and one of them is a correction to something maki already does.

**There is no animated spinner in their hub.** The running glyph is a static `⟳`,
and liveness is carried by *numbers that change* — a 5-second repaint tick
advances the elapsed clock and the age column. maki uses host-animated spinners,
which is prettier and strictly less informative: **a spinner keeps spinning for a
hung agent.** If maki keeps them, it needs the next item to compensate.

**Show silence.** Past a five-second threshold they append a dim `quiet 12s`
segment, computed from the last observed update. The comment says it plainly:
"surface the silence so a quietly-stuck agent is glanceable instead of
indistinguishable from a healthy one."

**Measure elapsed from run start, not from registration.** Their note: "age-since
activity is pinned at 'just now' by running heartbeats, so it carries no
information here. Prefer the executor-reported run start (fresh after a revive or
follow-up turn) over the ref's registration time." This applies directly to
maki's new activity line, which measures from `start_run` — correct today, and
worth keeping correct if sessions ever revive.

**Lead the detail line with the agent's own current objective**, not a static
task title. They carry `lastIntent` — what the model itself wrote about the step
it is on — falling back to the task description. Much higher information per
column than a title that never changes.

**Show measurement provenance rather than a confident wrong total.** Their
aggregate reads `2/3 timed · 3/7 measured`, and a `durationKind` of
`active | span | unknown` decides whether a duration is even summable.

**Keep the unread badge orthogonal to status.** `⧉ 3` is its own column rather
than folding "has messages waiting" into the status enum.

**Dim idle rows while any sibling is running** — "with nothing in flight every
row renders at full brightness." Finished work recedes behind live work with no
extra chrome.

**Bound the paint cost.** A selection-anchored window grows outward while it
fits, then trims the farthest neighbours so the `… N more` markers themselves fit
before painting. "Only visible entries are rendered, so the 5,000-agent Hub
retains bounded paint cost." maki's `/sessions` merges a live list with a
background disk scan and will meet the same wall.

**Read footer key labels from the keybinding registry**, with a test asserting
it, "so a rebind can't leave the footer lying". maki hand-writes footer hints in
its pickers and in the prompt editor; those drift the moment anything is rebound.

**Where maki already agrees:** `/sessions` freezes row rank for the picker's
lifetime so rows never jump under the cursor while background agents work — the
same fix, arrived at independently, and their top-ranked UI recommendation.
