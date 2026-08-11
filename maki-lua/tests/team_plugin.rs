//! Tests the team plugin end-to-end: real plugin source, real schema parsing
//! and tool dispatch, with `maki.session.*` replaced by a Lua stub that records
//! what the plugin asked the host to do.
//!
//! The stub is the whole point. Everything the plugin does to the outside world
//! goes through four session calls, so recording them is enough to assert the
//! coordination behaviour without a UI, a second agent, or a model.

use std::sync::Arc;

use maki_agent::tools::ToolRegistry;
use maki_agent::tools::test_support::stub_ctx;
use maki_agent::{AgentMode, ToolOutput};
use maki_lua::PluginHost;
use serde_json::{Value, json};

const TEAM_PLUGIN_SRC: &str = include_str!("../../plugins/team/init.lua");
const TEAM_TOOL: &str = "team";
const PROBE_TOOL: &str = "probe";

/// Mirrors of the plugin's contracts.
const ERR_NAME_TAKEN: &str = "a teammate is already called";
const ERR_BAD_NAME: &str = "name must be alphanumeric";
const ERR_NO_PEER: &str = "no teammate called";
const ERR_CLAIMED: &str = "already claimed by";
const ERR_BLOCKED: &str = "blocked by";
const ERR_FULL: &str = "team is full";
const MAX_TEAMMATES: u64 = 8;

/// `maki.session.*` recorded rather than performed. `new` hands back a
/// predictable id so tests can address the teammate afterwards.
const STUB_PRELUDE: &str = r#"
recorder = { new = {}, sent = {}, titles = {} }
local next_id = 0

maki.session = {
  new = function(o)
    next_id = next_id + 1
    local id = "sess-" .. next_id
    recorder.new[#recorder.new + 1] = { id = id, prompt = o.prompt, model = o.model, focus = o.focus }
    return id, nil
  end,
  set_title = function(o)
    recorder.titles[#recorder.titles + 1] = { id = o.id, title = o.title }
    return true, nil
  end,
  send = function(text, o)
    recorder.sent[#recorder.sent + 1] = { text = text, session = o.session, from = o.from, wake = o.wake }
    return recorder.send_outcome or "delivered", nil
  end,
}

maki.api.register_tool({
  name = "probe",
  description = "dump recorder",
  schema = { type = "object", properties = {} },
  handler = function() return maki.json.encode(recorder) end,
})
"#;

fn load_team_host() -> (Arc<ToolRegistry>, PluginHost) {
    let reg = Arc::new(ToolRegistry::new());
    let host = PluginHost::new(Arc::clone(&reg)).unwrap();
    host.load_source("team_plugin", &format!("{STUB_PRELUDE}\n{TEAM_PLUGIN_SRC}"))
        .unwrap();
    (reg, host)
}

fn exec(reg: &ToolRegistry, input: Value) -> Result<String, String> {
    let entry = reg.get(TEAM_TOOL).expect("team tool not registered");
    let inv = entry.tool.parse(&input).expect("parse failed");
    let ctx = stub_ctx(&AgentMode::Build);
    smol::block_on(async { inv.execute(&ctx).await })
        .output
        .map(|out| match out {
            ToolOutput::Plain(s) | ToolOutput::Markdown(s) => s.text,
            other => panic!("unexpected output: {other:?}"),
        })
}

fn ok(reg: &ToolRegistry, input: Value) -> String {
    exec(reg, input).expect("op failed")
}

fn probe(reg: &ToolRegistry) -> Value {
    let entry = reg.get(PROBE_TOOL).expect("probe not registered");
    let inv = entry.tool.parse(&json!({})).expect("parse failed");
    let ctx = stub_ctx(&AgentMode::Build);
    let out = smol::block_on(async { inv.execute(&ctx).await })
        .output
        .expect("probe failed");
    let text = match out {
        ToolOutput::Plain(s) | ToolOutput::Markdown(s) => s.text,
        other => panic!("unexpected probe output: {other:?}"),
    };
    serde_json::from_str(&text).expect("probe returned invalid json")
}

/// What the model actually sees, rebuilt the way a real request rebuilds it.
fn described(reg: &ToolRegistry) -> String {
    let vars = maki_agent::template::env_vars();
    let filter = maki_agent::tools::ToolFilter::All;
    let ctx = maki_agent::tools::DescriptionContext {
        filter: &filter,
        audience: maki_agent::tools::ToolAudience::MAIN,
        workflow: false,
    };
    let defs = reg.definitions(&vars, &ctx, false);
    let defs: Value = serde_json::to_value(defs).expect("definitions serialize");
    defs.as_array()
        .expect("definitions is an array")
        .iter()
        .find(|d| d["name"] == json!(TEAM_TOOL))
        .unwrap_or_else(|| panic!("team tool not in definitions: {defs}"))["description"]
        .as_str()
        .expect("description is a string")
        .to_string()
}

fn spawn(reg: &ToolRegistry, name: &str) -> String {
    ok(
        reg,
        json!({ "op": "spawn", "name": name, "prompt": "do the thing" }),
    )
}

fn add_task(reg: &ToolRegistry, title: &str) -> String {
    ok(reg, json!({ "op": "task_add", "title": title }))
}

// ---------------------------------------------------------------------------
// Spawning
// ---------------------------------------------------------------------------

#[test]
fn spawning_starts_a_background_session_and_names_it() {
    let (reg, _host) = load_team_host();
    spawn(&reg, "reviewer");

    let snap = probe(&reg);
    let new = &snap["new"][0];
    assert_eq!(
        new["focus"],
        json!(false),
        "a teammate must not steal focus"
    );
    assert!(new["prompt"].as_str().unwrap().contains("reviewer"));
    assert_eq!(snap["titles"][0]["title"], json!("reviewer"));
}

/// The spawn prompt is the teammate's only briefing: it never sees the lead's
/// conversation, so what it needs to know has to be in there.
#[test]
fn the_spawn_prompt_teaches_the_teammate_how_to_coordinate() {
    let (reg, _host) = load_team_host();
    spawn(&reg, "reviewer");

    let snap = probe(&reg);
    let prompt = snap["new"][0]["prompt"].as_str().unwrap().to_lowercase();
    assert!(
        prompt.contains("do the thing"),
        "the task itself is missing"
    );
    for expected in ["team", "claim a task", "before editing a file", "ask them"] {
        assert!(
            prompt.contains(expected),
            "prompt missing {expected:?}: {prompt}"
        );
    }
}

#[test]
fn a_model_is_forwarded_so_a_teammate_can_be_cheaper_than_the_lead() {
    let (reg, _host) = load_team_host();
    ok(
        &reg,
        json!({ "op": "spawn", "name": "scout", "prompt": "look around", "model": "weak" }),
    );
    assert_eq!(probe(&reg)["new"][0]["model"], json!("weak"));
}

#[test]
fn names_are_unique_because_they_are_the_address() {
    let (reg, _host) = load_team_host();
    spawn(&reg, "reviewer");
    let err = exec(
        &reg,
        json!({ "op": "spawn", "name": "reviewer", "prompt": "again" }),
    )
    .expect_err("duplicate name should fail");
    assert!(err.contains(ERR_NAME_TAKEN), "{err}");
}

#[test]
fn a_name_that_cannot_be_addressed_is_refused() {
    let (reg, _host) = load_team_host();
    for bad in ["", "has space", "-leading", "no/slash"] {
        let err =
            exec(&reg, json!({ "op": "spawn", "name": bad, "prompt": "x" })).unwrap_or_else(|e| e);
        assert!(err.contains(ERR_BAD_NAME), "{bad:?} was accepted: {err}");
    }
}

#[test]
fn the_team_has_a_ceiling() {
    let (reg, _host) = load_team_host();
    for i in 0..MAX_TEAMMATES {
        spawn(&reg, &format!("peer{i}"));
    }
    let err = exec(
        &reg,
        json!({ "op": "spawn", "name": "onemore", "prompt": "x" }),
    )
    .expect_err("should refuse past the cap");
    assert!(err.contains(ERR_FULL), "{err}");
}

// ---------------------------------------------------------------------------
// Messaging
// ---------------------------------------------------------------------------

#[test]
fn sending_addresses_by_name_and_attributes_the_sender() {
    let (reg, _host) = load_team_host();
    spawn(&reg, "reviewer");
    ok(
        &reg,
        json!({ "op": "send", "to": "reviewer", "message": "how is it going" }),
    );

    let snap = probe(&reg);
    let sent = snap["sent"].as_array().unwrap().last().unwrap();
    assert_eq!(sent["text"], json!("how is it going"));
    assert_eq!(sent["session"], json!("sess-1"));
    assert_eq!(sent["from"], json!("lead"), "the host names the sender");
    assert_eq!(sent["wake"], json!(true));
}

#[test]
fn sending_to_a_stranger_says_so_rather_than_failing_silently() {
    let (reg, _host) = load_team_host();
    let err = exec(
        &reg,
        json!({ "op": "send", "to": "nobody", "message": "hi" }),
    )
    .expect_err("unknown peer should fail");
    assert!(err.contains(ERR_NO_PEER), "{err}");
}

/// The outcome is the one fact a sender needs to decide whether to wait, so it
/// is reported in words rather than swallowed.
#[test]
fn the_reply_says_how_the_message_landed() {
    let (reg, _host) = load_team_host();
    spawn(&reg, "reviewer");
    let out = ok(
        &reg,
        json!({ "op": "send", "to": "reviewer", "message": "hi" }),
    );
    assert!(out.contains("read this when it next runs"), "{out}");
}

// ---------------------------------------------------------------------------
// The board
// ---------------------------------------------------------------------------

#[test]
fn publishing_work_tells_the_peers_who_could_take_it() {
    let (reg, _host) = load_team_host();
    spawn(&reg, "a");
    spawn(&reg, "b");
    add_task(&reg, "write the migration");

    let snap = probe(&reg);
    let told: Vec<_> = snap["sent"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["text"].as_str().unwrap().contains("new task"))
        .collect();
    assert_eq!(told.len(), 2, "both peers should hear about claimable work");
    assert!(told.iter().all(|s| s["wake"] == json!(true)));
}

/// One message per claim to every idle peer is one wasted turn each, and
/// nobody is waiting on the news.
#[test]
fn claiming_tells_nobody() {
    let (reg, _host) = load_team_host();
    spawn(&reg, "a");
    add_task(&reg, "write the migration");
    let before = probe(&reg)["sent"].as_array().unwrap().len();

    ok(&reg, json!({ "op": "task_claim", "task_id": "t1" }));

    assert_eq!(probe(&reg)["sent"].as_array().unwrap().len(), before);
}

#[test]
fn a_task_can_only_be_claimed_once() {
    let (reg, _host) = load_team_host();
    add_task(&reg, "write the migration");
    ok(&reg, json!({ "op": "task_claim", "task_id": "t1" }));

    let err = exec(&reg, json!({ "op": "task_claim", "task_id": "t1" }))
        .expect_err("second claim should fail");
    assert!(err.contains(ERR_CLAIMED), "{err}");
}

#[test]
fn a_blocked_task_cannot_be_claimed_until_its_blocker_is_done() {
    let (reg, _host) = load_team_host();
    add_task(&reg, "first");
    ok(
        &reg,
        json!({ "op": "task_add", "title": "second", "blocked_by": ["t1"] }),
    );

    let err = exec(&reg, json!({ "op": "task_claim", "task_id": "t2" }))
        .expect_err("blocked task should refuse");
    assert!(err.contains(ERR_BLOCKED), "{err}");

    ok(&reg, json!({ "op": "task_done", "task_id": "t1" }));
    let out = ok(&reg, json!({ "op": "task_claim", "task_id": "t2" }));
    assert!(out.contains("Claimed"), "{out}");
}

/// Blockers are read live rather than trusted, so a task published after its
/// blocker already finished is claimable straight away.
#[test]
fn a_task_blocked_on_finished_work_is_claimable_immediately() {
    let (reg, _host) = load_team_host();
    add_task(&reg, "first");
    ok(&reg, json!({ "op": "task_done", "task_id": "t1" }));
    ok(
        &reg,
        json!({ "op": "task_add", "title": "second", "blocked_by": ["t1"] }),
    );

    let out = ok(&reg, json!({ "op": "task_claim", "task_id": "t2" }));
    assert!(out.contains("Claimed"), "{out}");
}

#[test]
fn finishing_work_tells_peers_only_when_it_unblocked_something() {
    let (reg, _host) = load_team_host();
    spawn(&reg, "a");
    add_task(&reg, "first");
    add_task(&reg, "unrelated");
    let quiet_before = probe(&reg)["sent"].as_array().unwrap().len();
    ok(&reg, json!({ "op": "task_done", "task_id": "t2" }));
    assert_eq!(
        probe(&reg)["sent"].as_array().unwrap().len(),
        quiet_before,
        "finishing work nobody waited on is not news"
    );

    ok(
        &reg,
        json!({ "op": "task_add", "title": "third", "blocked_by": ["t1"] }),
    );
    let before = probe(&reg)["sent"].as_array().unwrap().len();
    ok(&reg, json!({ "op": "task_done", "task_id": "t1" }));

    let snap = probe(&reg);
    let sent = snap["sent"].as_array().unwrap();
    assert!(sent.len() > before, "unblocking should be announced");
    assert!(
        sent.last().unwrap()["text"]
            .as_str()
            .unwrap()
            .contains("now claimable")
    );
}

#[test]
fn released_work_goes_back_on_the_board() {
    let (reg, _host) = load_team_host();
    add_task(&reg, "write the migration");
    ok(&reg, json!({ "op": "task_claim", "task_id": "t1" }));
    ok(&reg, json!({ "op": "task_release", "task_id": "t1" }));

    let out = ok(&reg, json!({ "op": "task_list", "status": "pending" }));
    assert!(out.contains("t1"), "{out}");
}

#[test]
fn the_board_lists_what_is_on_it() {
    let (reg, _host) = load_team_host();
    assert!(ok(&reg, json!({ "op": "task_list" })).contains("empty"));

    add_task(&reg, "first");
    add_task(&reg, "second");
    ok(&reg, json!({ "op": "task_claim", "task_id": "t1" }));

    let all = ok(&reg, json!({ "op": "task_list" }));
    assert!(all.contains("first") && all.contains("second"), "{all}");
    let pending = ok(&reg, json!({ "op": "task_list", "status": "pending" }));
    assert!(
        !pending.contains("first") && pending.contains("second"),
        "{pending}"
    );
}

// ---------------------------------------------------------------------------
// Roster
// ---------------------------------------------------------------------------

#[test]
fn the_roster_names_the_peers() {
    let (reg, _host) = load_team_host();
    assert!(ok(&reg, json!({ "op": "list" })).contains("No teammates"));

    spawn(&reg, "reviewer");
    spawn(&reg, "scout");
    let out = ok(&reg, json!({ "op": "list" }));
    assert!(out.contains("reviewer") && out.contains("scout"), "{out}");
}

/// The description is rebuilt per request, so the model learns who its peers
/// are and what they are doing without anyone having to ask for a roster.
#[test]
fn the_tool_description_carries_the_live_roster() {
    let (reg, _host) = load_team_host();
    assert!(
        !described(&reg).contains("Live teammates"),
        "an empty team should not advertise a roster"
    );

    spawn(&reg, "reviewer");

    let desc = described(&reg);
    assert!(desc.contains("Live teammates"), "{desc}");
    assert!(desc.contains("reviewer"), "{desc}");
    assert!(desc.contains("messaging one wakes it"), "{desc}");
}

#[test]
fn an_unknown_op_is_refused() {
    let (reg, _host) = load_team_host();
    let parsed = reg
        .get(TEAM_TOOL)
        .unwrap()
        .tool
        .parse(&json!({ "op": "nope" }));
    assert!(
        parsed.is_err(),
        "the schema enum should reject an unknown op"
    );
}
