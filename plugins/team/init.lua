-- Agent teams: several sessions working at once, addressable by name, with a
-- shared board they pull work from.
--
-- A teammate is a top-level session, not a subagent. Subagents die with the
-- turn that created them and cannot be addressed or messaged; sessions have a
-- mailbox, survive a reload, and appear in `/sessions`. Everything here is
-- built on `maki.session.*`.
--
-- All state below is module-level, which is shared across every session
-- because the host runs one Lua VM per process. That is what makes a shared
-- board possible without files or locks. Two consequences worth knowing: the
-- VM is single-threaded and cooperative, so a read-modify-write is atomic only
-- while it does not await; and the state dies on `/reload`.

local MAX_NAME = 32
local NAME_PATTERN = "^[%w][%w_-]*$"
local LEAD = "lead"

local opts = maki.api.register_options({
  max_teammates = { default = 8, min = 1, desc = "Most teammates that may be live at once." },
})

--- name -> session id. The plugin owns addressing rather than leaning on
--- session titles, which `/rename` can change out from under it.
local by_name = {}
--- session id -> { name, status }. Kept fresh by the SessionStatusChanged
--- autocmd, which is synchronous and so cannot ask the host for anything.
local roster = {}
--- task id -> task. Ordered by `order` because pairs() is unordered.
local board = {}
local next_task = 0

local ERR_NAME_TAKEN = "a teammate is already called"
local ERR_BAD_NAME = "name must be alphanumeric, with - and _"
local ERR_NO_PEER = "no teammate called"
local ERR_FULL = "team is full"
local ERR_NO_TASK = "no task"
local ERR_CLAIMED = "already claimed by"
local ERR_BLOCKED = "blocked by"
local ERR_NOT_YOURS = "not yours to finish, it is claimed by"
local ERR_SELF_SEND = "that is you"

local function trim(s)
  return (tostring(s or ""):gsub("^%s+", ""):gsub("%s+$", ""))
end

--- Who is calling. Every session shares this module, so a teammate finds its
--- own name in the same table the lead does.
local function whoami(ctx)
  local id = ctx and ctx.session_id and ctx:session_id()
  if not id then
    return LEAD, nil
  end
  local entry = roster[id]
  return entry and entry.name or LEAD, id
end

local function peers_of(id)
  local out = {}
  for peer_id, entry in pairs(roster) do
    if peer_id ~= id then
      out[#out + 1] = { id = peer_id, name = entry.name, status = entry.status or "idle" }
    end
  end
  table.sort(out, function(a, b)
    return a.name < b.name
  end)
  return out
end

local function live_count()
  local n = 0
  for _ in pairs(roster) do
    n = n + 1
  end
  return n
end

local function tasks_in_order()
  local out = {}
  for _, task in pairs(board) do
    out[#out + 1] = task
  end
  table.sort(out, function(a, b)
    return a.order < b.order
  end)
  return out
end

--- Unresolved blockers, read live rather than trusting the stored list.
--- Completing a task prunes it from its dependents, but that is bookkeeping:
--- this is the check that decides. It also means a task created after its
--- blocker already finished is claimable immediately.
local function pending_blockers(task)
  local out = {}
  for _, id in ipairs(task.blocked_by or {}) do
    local dep = board[id]
    if dep and dep.status ~= "done" then
      out[#out + 1] = id
    end
  end
  return out
end

--- Fire-and-forget. A send that fails means the peer is gone, which is worth
--- knowing but never worth failing the caller's own operation over.
local function tell(id, text, from, wake)
  local ok = maki.session.send(text, { session = id, from = from, wake = wake })
  return ok
end

local function tell_peers(id, text, from, wake)
  local reached = 0
  for _, peer in ipairs(peers_of(id)) do
    if tell(peer.id, text, from, wake) then
      reached = reached + 1
    end
  end
  return reached
end

-- ---------------------------------------------------------------------------
-- Ops
-- ---------------------------------------------------------------------------

local function op_spawn(input, ctx)
  local name = trim(input.name)
  if name == "" or #name > MAX_NAME or not name:match(NAME_PATTERN) then
    return { llm_output = ERR_BAD_NAME .. ", up to " .. MAX_NAME .. " characters", is_error = true }
  end
  if by_name[name] then
    return { llm_output = ERR_NAME_TAKEN .. " " .. name, is_error = true }
  end
  if live_count() >= opts.max_teammates then
    return {
      llm_output = string.format("%s (%d live). Ask one to finish first.", ERR_FULL, live_count()),
      is_error = true,
    }
  end

  local me = whoami(ctx)
  local prompt = string.format(
    "You are `%s`, a teammate on an agent team. `%s` started you.\n\n"
      .. "Use the `team` tool to coordinate: `list` shows your peers, `send` messages one by "
      .. "name, and the board ops share work. Claim a task before you start it, and report the "
      .. "outcome when you finish so peers waiting on it can move.\n\n"
      .. "Message a peer before editing a file they may already own — overlapping edits collide. "
      .. "Never read another session's files to work out what a peer is doing; ask them.\n\n%s",
    name,
    me,
    trim(input.prompt)
  )

  local id, err = maki.session.new({ prompt = prompt, focus = false, model = input.model })
  if not id then
    return { llm_output = "could not start a teammate: " .. tostring(err), is_error = true }
  end

  by_name[name] = id
  roster[id] = { name = name, status = "working" }
  maki.session.set_title({ id = id, title = name })

  return string.format(
    "Started `%s`%s. It is working on its prompt now; `team list` shows the roster and "
      .. "`team send` reaches it by name.",
    name,
    input.model and (" on " .. input.model) or ""
  )
end

local function op_list(_, ctx)
  local me, id = whoami(ctx)
  local peers = peers_of(id)
  if #peers == 0 then
    return "No teammates. Start one with `team spawn`."
  end
  local lines = { string.format("You are `%s`. Teammates:", me) }
  for _, peer in ipairs(peers) do
    lines[#lines + 1] = string.format("- `%s` (%s)", peer.name, peer.status)
  end
  return table.concat(lines, "\n")
end

local function op_send(input, ctx)
  local me, id = whoami(ctx)
  local to = trim(input.to)
  local target = by_name[to]
  if not target then
    return { llm_output = string.format("%s `%s`. Run `team list`.", ERR_NO_PEER, to), is_error = true }
  end
  if target == id then
    return { llm_output = ERR_SELF_SEND, is_error = true }
  end
  local message = trim(input.message)
  if message == "" then
    return { llm_output = "message must not be blank", is_error = true }
  end

  local how, err = maki.session.send(message, { session = target, from = me, wake = true })
  if not how then
    return { llm_output = string.format("could not reach `%s`: %s", to, tostring(err)), is_error = true }
  end
  local note = ({
    woken = "it was idle and has started a turn to read it",
    injected = "it is working; the message joins its next step",
    delivered = "it will read this when it next runs",
  })[how] or how
  return string.format("Sent to `%s` — %s.", to, note)
end

local function op_task_add(input, ctx)
  local me, id = whoami(ctx)
  local title = trim(input.title)
  if title == "" then
    return { llm_output = "title must not be blank", is_error = true }
  end
  for _, dep in ipairs(input.blocked_by or {}) do
    if not board[dep] then
      return { llm_output = string.format("%s `%s` to block on", ERR_NO_TASK, dep), is_error = true }
    end
  end

  next_task = next_task + 1
  local task_id = "t" .. next_task
  board[task_id] = {
    id = task_id,
    order = next_task,
    title = title,
    status = "pending",
    blocked_by = input.blocked_by or {},
    created_by = me,
  }

  -- Publishing work wakes peers. Skipping this is what forces a team to poll,
  -- or to grow a watchdog that notices idle agents next to claimable work.
  local blockers = pending_blockers(board[task_id])
  local reached = 0
  if #blockers == 0 then
    reached = tell_peers(id, string.format("new task `%s`: %s — claim it with `team task_claim`", task_id, title), me, true)
  end
  return string.format(
    "Published `%s`: %s%s. Told %d peer(s).",
    task_id,
    title,
    #blockers > 0 and (" (blocked by " .. table.concat(blockers, ", ") .. ")") or "",
    reached
  )
end

local function op_task_claim(input, ctx)
  local me = whoami(ctx)
  local task = board[trim(input.task_id)]
  if not task then
    return { llm_output = ERR_NO_TASK .. " `" .. trim(input.task_id) .. "`", is_error = true }
  end
  -- Check and write with nothing awaited in between. The VM is cooperative,
  -- so this is atomic; an await here would let a peer claim the same task.
  if task.status == "claimed" then
    return { llm_output = string.format("`%s` is %s `%s`", task.id, ERR_CLAIMED, task.claimed_by), is_error = true }
  end
  if task.status == "done" then
    return { llm_output = string.format("`%s` is already done", task.id), is_error = true }
  end
  local blockers = pending_blockers(task)
  if #blockers > 0 then
    return {
      llm_output = string.format("`%s` is %s %s", task.id, ERR_BLOCKED, table.concat(blockers, ", ")),
      is_error = true,
    }
  end
  task.status = "claimed"
  task.claimed_by = me

  -- Claiming is deliberately not broadcast. One message per claim to every
  -- idle peer is one wasted turn each, and nobody is waiting on the news.
  return string.format("Claimed `%s`: %s. Finish with `team task_done`, or hand it back with `team task_release`.", task.id, task.title)
end

local function op_task_release(input, ctx)
  local me = whoami(ctx)
  local task = board[trim(input.task_id)]
  if not task or task.status ~= "claimed" then
    return { llm_output = ERR_NO_TASK .. " of yours to release", is_error = true }
  end
  if task.claimed_by ~= me and me ~= LEAD then
    return { llm_output = string.format("`%s` is %s `%s`", task.id, ERR_CLAIMED, task.claimed_by), is_error = true }
  end
  task.status = "pending"
  task.claimed_by = nil
  return string.format("Released `%s`: %s. It is claimable again.", task.id, task.title)
end

local function op_task_done(input, ctx)
  local me, id = whoami(ctx)
  local task = board[trim(input.task_id)]
  if not task then
    return { llm_output = ERR_NO_TASK .. " `" .. trim(input.task_id) .. "`", is_error = true }
  end
  if task.status == "claimed" and task.claimed_by ~= me and me ~= LEAD then
    return { llm_output = string.format("`%s` is %s `%s`", task.id, ERR_NOT_YOURS, task.claimed_by), is_error = true }
  end
  task.status = "done"
  task.result = trim(input.result)
  task.claimed_by = task.claimed_by or me

  -- Prune the finished id from dependents. Bookkeeping only: `pending_blockers`
  -- reads live status, so a missed prune costs nothing.
  local unblocked = {}
  for _, other in ipairs(tasks_in_order()) do
    for i, dep in ipairs(other.blocked_by or {}) do
      if dep == task.id then
        table.remove(other.blocked_by, i)
        if other.status == "pending" and #pending_blockers(other) == 0 then
          unblocked[#unblocked + 1] = other
        end
        break
      end
    end
  end

  -- Told to everyone only because anyone may claim. This is the one broadcast
  -- worth its cost: work became available that was not before.
  if #unblocked > 0 then
    local names = {}
    for _, other in ipairs(unblocked) do
      names[#names + 1] = string.format("`%s` (%s)", other.id, other.title)
    end
    tell_peers(id, "now claimable: " .. table.concat(names, ", "), me, true)
  end

  local suffix = #unblocked > 0 and (" Unblocked " .. #unblocked .. " task(s).") or ""
  return string.format("Finished `%s`: %s.%s", task.id, task.title, suffix)
end

local function op_task_list(input, _)
  local want = input.status
  local lines = {}
  for _, task in ipairs(tasks_in_order()) do
    if not want or task.status == want then
      local blockers = pending_blockers(task)
      lines[#lines + 1] = string.format(
        "- `%s` [%s]%s %s%s",
        task.id,
        task.status,
        task.claimed_by and (" " .. task.claimed_by) or "",
        task.title,
        #blockers > 0 and (" (blocked by " .. table.concat(blockers, ", ") .. ")") or ""
      )
    end
  end
  if #lines == 0 then
    return "Board is empty. Publish work with `team task_add`."
  end
  return table.concat(lines, "\n")
end

local OPS = {
  spawn = op_spawn,
  list = op_list,
  send = op_send,
  task_add = op_task_add,
  task_claim = op_task_claim,
  task_release = op_task_release,
  task_done = op_task_done,
  task_list = op_task_list,
}

-- ---------------------------------------------------------------------------
-- Roster upkeep
-- ---------------------------------------------------------------------------

-- Synchronous, like every autocmd, so it may not ask the host anything. It
-- only records what the event already carries.
maki.api.create_autocmd("SessionStatusChanged", {
  callback = function(ev)
    local data = ev and ev.data
    if not data or not data.session_id then
      return
    end
    local entry = roster[data.session_id]
    if entry then
      entry.status = data.status
    end
  end,
})

local DESCRIPTION = [[Coordinate a team of agents working at once.

Each teammate is its own session with its own context, addressable by name.

- spawn: start a teammate with a name and a prompt. Optionally a cheaper model.
- list: who exists and what they are doing.
- send: message a teammate by name. Never blocks; the reply says how it landed.
- task_add / task_claim / task_release / task_done / task_list: a shared board
  peers pull work from. Claim before starting; report the outcome when done.

Message a peer before touching a file they may own. Never read another
session's files to work out what a peer is doing — ask them. Never use
messaging for something a tool can answer.]]

maki.api.register_tool({
  name = "team",
  description = DESCRIPTION,
  kind = "execute",
  audiences = { "main" },
  schema = {
    type = "object",
    required = { "op" },
    additionalProperties = false,
    properties = {
      op = {
        type = "string",
        enum = { "spawn", "list", "send", "task_add", "task_claim", "task_release", "task_done", "task_list" },
        description = "What to do.",
      },
      name = { type = "string", description = "spawn: what to call the teammate." },
      prompt = { type = "string", description = "spawn: what it should work on." },
      model = { type = "string", description = 'spawn: a model spec, or a tier ("weak", "medium", "strong").' },
      to = { type = "string", description = "send: teammate name from `list`." },
      message = { type = "string", description = "send: plain prose. Keep it short." },
      title = { type = "string", description = "task_add: what the task is." },
      blocked_by = {
        type = "array",
        items = { type = "string" },
        description = "task_add: task ids that must finish first.",
      },
      task_id = { type = "string", description = "task_claim/release/done: which task." },
      result = { type = "string", description = "task_done: what came of it." },
      status = {
        type = "string",
        enum = { "pending", "claimed", "done" },
        description = "task_list: only show these.",
      },
    },
  },

  -- Rendered per request, so the model sees the live roster without anyone
  -- asking for it. Synchronous: reads the cache, never the host.
  describe = function()
    local peers = peers_of(nil)
    if #peers == 0 then
      return DESCRIPTION
    end
    local lines = { DESCRIPTION, "", "Live teammates:" }
    for _, peer in ipairs(peers) do
      lines[#lines + 1] = string.format("- `%s` (%s)", peer.name, peer.status)
    end
    lines[#lines + 1] = "Idle teammates are not gone: messaging one wakes it."
    return table.concat(lines, "\n")
  end,

  header = function(input)
    local op = input.op or "?"
    if op == "spawn" then
      return "team spawn " .. tostring(input.name)
    elseif op == "send" then
      return "team send " .. tostring(input.to)
    end
    return "team " .. op
  end,

  handler = function(input, ctx)
    local run = OPS[input.op]
    if not run then
      return { llm_output = "unknown op: " .. tostring(input.op), is_error = true }
    end
    return run(input, ctx)
  end,
})
