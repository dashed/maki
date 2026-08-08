//! `maki.session`: host session primitives. Session management round-trips to
//! the UI event loop, which owns live runtimes and storage. `notify` posts
//! directly to the agent mailbox so synchronous callbacks can use it.

use maki_agent::SessionMailbox;
use maki_lua_macro::{lua_fn, lua_table};
use maki_storage::id::MakiId;
use mlua::{Lua, Result as LuaResult, Table, Value};

use crate::api::util::command::{NO_UI_ERR, SessionRequest, UiAction, ui_roundtrip};
use crate::api::util::convert::json_to_lua;
use crate::api::util::pair::{Pair, err_pair, try_pair};

const BLANK_NOTIFY_ERR: &str = "text must not be blank";
const SESSION_REQUIRED_ERR: &str = "session is required";
const DELIVERED: &str = "delivered";

async fn roundtrip(
    lua: Lua,
    tx: Option<flume::Sender<UiAction>>,
    req: SessionRequest,
) -> LuaResult<Pair<Value>> {
    let reply =
        try_pair!(ui_roundtrip(tx.as_ref(), |reply_tx| UiAction::Session { req, reply_tx }).await);
    let value = try_pair!(reply);
    Ok((Some(json_to_lua(&lua, &value)?), None))
}

/// Lists sessions stored for the current project. Answered from a
/// background scan, so a slow disk never blocks the UI.
///
/// @return (table|nil, string|nil) Array of `{id, title, updated_at}`, or nil and an error.
/// @example
/// local stored, err = maki.session.list()
#[lua_fn]
async fn list(lua: Lua, #[ctx] tx: Option<flume::Sender<UiAction>>) -> LuaResult<Pair<Value>> {
    roundtrip(lua, tx, SessionRequest::List).await
}

/// Lists the sessions currently running in this UI. Status is "working",
/// "needs_input", or "idle".
///
/// @return (table|nil, string|nil) Array of `{id, title, status, updated_at, focused}`, or nil and an error.
/// @example
/// local live, err = maki.session.live()
#[lua_fn]
async fn live(lua: Lua, #[ctx] tx: Option<flume::Sender<UiAction>>) -> LuaResult<Pair<Value>> {
    roundtrip(lua, tx, SessionRequest::Live).await
}

/// Returns the id of the currently focused session.
///
/// @return (string|nil, string|nil) Session id, or nil and an error.
/// @example
/// local id = maki.session.current()
#[lua_fn]
async fn current(lua: Lua, #[ctx] tx: Option<flume::Sender<UiAction>>) -> LuaResult<Pair<Value>> {
    roundtrip(lua, tx, SessionRequest::Current).await
}

/// Switches the UI to the session with {id}. The session must be live.
///
/// @param id string Session id, as returned by `list()` or `live()`.
/// @return (boolean|nil, string|nil) true on success, or nil and an error.
/// @example
/// local _, err = maki.session.focus(id)
#[lua_fn]
async fn focus(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    id: String,
) -> LuaResult<Pair<Value>> {
    roundtrip(lua, tx, SessionRequest::Focus { id }).await
}

/// Deletes a session and its stored history, cancelling it first if it
/// is running. The focused session cannot be deleted.
///
/// @param id string Session id to delete.
/// @return (boolean|nil, string|nil) true on success, or nil and an error.
/// @example
/// local _, err = maki.session.delete(id)
#[lua_fn]
async fn delete(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    id: String,
) -> LuaResult<Pair<Value>> {
    roundtrip(lua, tx, SessionRequest::Delete { id }).await
}

/// Starts a new session in the current project.
///
/// @param opts table? Optional fields: prompt (string) first user message
///   to submit right away; focus (boolean) switch the UI to the new session;
///   model (string) a model spec, or a tier name ("weak", "medium", "strong",
///   "compaction") resolved through the model roles. Defaults to the current
///   model, which is rarely what a background session wants.
/// @return (string|nil, string|nil) New session id, or nil and an error.
/// @example
/// local id, err = maki.session.new({ prompt = "fix the tests", focus = true })
/// local id, err = maki.session.new({ prompt = "review this", model = "weak" })
#[lua_fn]
async fn new(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    opts: Option<Table>,
) -> LuaResult<Pair<Value>> {
    let (prompt, focus, model) = match opts {
        Some(opts) => (
            opts.get("prompt")?,
            opts.get("focus").unwrap_or(false),
            opts.get("model")?,
        ),
        None => (None, false, None),
    };
    roundtrip(
        lua,
        tx,
        SessionRequest::New {
            prompt,
            focus,
            model,
        },
    )
    .await
}

/// Sends {text} as a regular user prompt to a live session. The text is
/// never interpreted: slash commands, `exit`, and `!` shell prefixes are
/// all sent to the model verbatim. If the session is currently streaming,
/// the prompt is queued and picked up when the agent reaches it.
///
/// @param text string The prompt to send. Must not be blank.
/// @param opts table? Optional fields: session (string) id of a live
///   session; defaults to the focused one.
/// @return (string|nil, string|nil) "started" or "queued", or nil and an error.
/// @example
/// local state, err = maki.session.prompt("run the tests", { session = id })
#[lua_fn]
async fn prompt(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    text: String,
    opts: Option<Table>,
) -> LuaResult<Pair<Value>> {
    let id = match opts {
        Some(opts) => opts.get("session")?,
        None => None,
    };
    roundtrip(lua, tx, SessionRequest::Prompt { id, text }).await
}

/// Reports {text} to a live session and says how it landed.
///
/// The same delivery as `notify`, but answered by the UI event loop, which
/// can see the recipient's state and act on it in the same step — so the
/// outcome is observed rather than guessed. Use this from a tool handler;
/// `notify` stays the synchronous one for autocmds and job callbacks.
///
/// Outcomes: `"woken"` the session was idle and a turn was started for it;
/// `"injected"` it was working, so the message joins its next model call;
/// `"delivered"` it is queued and will be read at the recipient's next run.
///
/// Honest limits. `"woken"` and `"injected"` need the TUI — under `-p`, ACP
/// and the SDK the answer is always `"delivered"`, because nothing outside
/// the interactive UI starts a turn for a waiting message. `"injected"` also
/// covers a session parked on a permission prompt, where the message rides in
/// only once a human answers. None of them means the message was read.
///
/// @param text string What to report. Must not be blank.
/// @param opts table Options:
///   `session` (string) id of a live session.
///   `wake` (boolean) start a turn if the session is idle (default false).
///   `from` (string) who to attribute it to, for the model and the transcript.
/// @return (string|nil, string|nil) the outcome, or nil and an error.
/// @example
/// local how, err = maki.session.send("tests are green", { session = id, wake = true, from = "reviewer" })
#[lua_fn]
async fn send(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    text: String,
    opts: Option<Table>,
) -> LuaResult<Pair<Value>> {
    if text.trim().is_empty() {
        return Ok(err_pair(BLANK_NOTIFY_ERR));
    }
    let Some(opts) = opts else {
        return Ok(err_pair(SESSION_REQUIRED_ERR));
    };
    let Some(id) = opts.get::<Option<String>>("session")? else {
        return Ok(err_pair(SESSION_REQUIRED_ERR));
    };
    let wake = opts.get("wake").unwrap_or(false);
    let from: Option<String> = opts.get("from")?;

    let req = SessionRequest::Notify {
        id: id.clone(),
        text: text.clone(),
        from: from.clone(),
        wake,
    };
    let reply = ui_roundtrip(tx.as_ref(), |reply_tx| UiAction::Session { req, reply_tx }).await;

    // No event loop to ask. Deliver the same message the same way and answer
    // with the weaker word, rather than failing a send that would have worked.
    let no_ui = match &reply {
        Err(e) => *e == NO_UI_ERR,
        Ok(Err(e)) => e == NO_UI_ERR,
        Ok(Ok(_)) => false,
    };
    if no_ui {
        let session_id: MakiId = match id.parse() {
            Ok(id) => id,
            Err(error) => return Ok(err_pair(error)),
        };
        if let Err(error) = SessionMailbox::notify(session_id, text, from.as_deref(), wake) {
            return Ok(err_pair(error));
        }
        return Ok((Some(Value::String(lua.create_string(DELIVERED)?)), None));
    }

    let value = try_pair!(try_pair!(reply));
    Ok((Some(json_to_lua(&lua, &value)?), None))
}

/// Reports {text} to a live session without creating a user turn. The
/// observation waits for the session's next agent run.
///
/// @param text string What to report. Must not be blank.
/// @param opts table Options:
///   `session` (string) id of a live session.
///   `wake` (boolean) start a TUI turn when it next becomes idle (default false).
/// @return (boolean|nil, string|nil) true, or nil and an error.
/// @example
/// maki.session.notify("[monitor] deploy failed", { session = id, wake = true })
/// maki.session.notify("build is green", { session = id, wake = true, from = "reviewer" })
#[lua_fn]
fn notify(_lua: &Lua, text: String, opts: Option<Table>) -> LuaResult<Pair<bool>> {
    if text.trim().is_empty() {
        return Ok(err_pair(BLANK_NOTIFY_ERR));
    }
    let Some(opts) = opts else {
        return Ok(err_pair(SESSION_REQUIRED_ERR));
    };
    let Some(raw_id) = opts.get::<Option<String>>("session")? else {
        return Ok(err_pair(SESSION_REQUIRED_ERR));
    };
    let session_id: MakiId = match raw_id.parse() {
        Ok(id) => id,
        Err(error) => return Ok(err_pair(error)),
    };
    let wake = opts.get("wake").unwrap_or(false);
    // Named by the caller, not derived here. A tool handler knows its own
    // session from `ctx:session_id()`, which is what makes the name something
    // the model calling that tool cannot choose.
    let from: Option<String> = opts.get("from")?;
    if let Err(error) = SessionMailbox::notify(session_id, text, from.as_deref(), wake) {
        return Ok(err_pair(error));
    }
    Ok((Some(true), None))
}

/// Renames a session, live or stored.
///
/// @param opts table Required fields: id (string) session to rename;
///   title (string) the new title.
/// @return (boolean|nil, string|nil) true on success, or nil and an error.
/// @example
/// local _, err = maki.session.set_title({ id = id, title = "refactor" })
#[lua_fn]
async fn set_title(
    lua: Lua,
    #[ctx] tx: Option<flume::Sender<UiAction>>,
    opts: Table,
) -> LuaResult<Pair<Value>> {
    let req = SessionRequest::SetTitle {
        id: opts.get("id")?,
        title: opts.get("title")?,
    };
    roundtrip(lua, tx, req).await
}

lua_table! {
    /// Host session primitives. The interactive UI can run several sessions
    /// at once; these functions let plugins list, create, focus, rename, and
    /// delete them. Session management returns `nil, "no interactive UI
    /// attached"` without a UI. `notify` instead targets a live agent mailbox
    /// directly, so it also works under ACP and SDK frontends.
    "maki.session" => pub(crate) fn create_session_table(tx: Option<flume::Sender<UiAction>>),
    DOCS [list(tx), live(tx), current(tx), focus(tx), delete(tx), new(tx), prompt(tx), send(tx), notify(), set_title(tx)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::util::command::NO_UI_ERR;
    use mlua::Value;
    use serde_json::json;
    use test_case::test_case;

    fn lua_with_session(tx: Option<flume::Sender<UiAction>>) -> Lua {
        let lua = Lua::new();
        let t = create_session_table(&lua, tx).unwrap();
        lua.globals().set("session", t).unwrap();
        lua
    }

    #[test]
    fn live_without_ui_returns_error_pair() {
        let lua = lua_with_session(None);
        let (val, err): (Value, Option<String>) =
            smol::block_on(lua.load("return session.live()").eval_async()).unwrap();
        assert!(val.is_nil());
        assert_eq!(err.as_deref(), Some(NO_UI_ERR));
    }

    /// Without an event loop there is nobody to observe the outcome, but the
    /// message can still be delivered — so the send succeeds with the weaker
    /// word rather than failing.
    #[test_case("return session.new({ model = 'weak' })", Some("weak") ; "tier_name")]
    #[test_case("return session.new({ model = 'anthropic/claude-x' })", Some("anthropic/claude-x") ; "explicit_spec")]
    #[test_case("return session.new({})", None ; "omitted_means_inherit")]
    fn new_forwards_the_model(code: &str, expected: Option<&str>) {
        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = lua_with_session(Some(tx));
        let expected = expected.map(str::to_owned);
        let checker = std::thread::spawn(move || {
            let Ok(UiAction::Session {
                req: SessionRequest::New { model, .. },
                reply_tx,
            }) = rx.recv()
            else {
                panic!("expected new request");
            };
            assert_eq!(model, expected);
            reply_tx.send(Ok(json!("new-id"))).unwrap();
        });

        let (_val, err): (Value, Option<String>) =
            smol::block_on(lua.load(code).eval_async()).unwrap();

        checker.join().unwrap();
        assert_eq!(err, None);
    }

    #[test]
    fn send_without_a_ui_falls_back_to_direct_delivery() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        let lua = lua_with_session(None);
        let code = format!("return session.send('ping', {{ session = '{id}', from = 'lead' }})");

        let (val, err): (Value, Option<String>) =
            smol::block_on(lua.load(&code).eval_async()).unwrap();

        assert_eq!(err, None);
        assert_eq!(val.as_string().unwrap().to_str().unwrap(), DELIVERED);
        let delivered = mailbox.drain();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].user_text(), Some("ping"));
        assert_eq!(delivered[0].observation_sender(), Some("lead"));
    }

    /// The recipient has to exist even on the fallback path, or a typo in a
    /// teammate id would look like a successful send.
    #[test]
    fn send_to_a_dead_session_still_fails() {
        let id = MakiId::generate();
        let lua = lua_with_session(None);
        let code = format!("return session.send('ping', {{ session = '{id}' }})");

        let (val, err): (Value, Option<String>) =
            smol::block_on(lua.load(&code).eval_async()).unwrap();

        assert!(val.is_nil());
        assert!(err.unwrap().contains("session not live"));
    }

    #[test]
    fn send_forwards_the_sender_and_wake_to_the_ui() {
        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = lua_with_session(Some(tx));
        let checker = std::thread::spawn(move || {
            let Ok(UiAction::Session {
                req:
                    SessionRequest::Notify {
                        id,
                        text,
                        from,
                        wake,
                    },
                reply_tx,
            }) = rx.recv()
            else {
                panic!("expected notify request");
            };
            assert_eq!(id, "abc");
            assert_eq!(text, "ping");
            assert_eq!(from.as_deref(), Some("reviewer"));
            assert!(wake);
            reply_tx.send(Ok(json!("woken"))).unwrap();
        });

        let (val, err): (Value, Option<String>) = smol::block_on(
            lua.load(
                "return session.send('ping', { session = 'abc', from = 'reviewer', wake = true })",
            )
            .eval_async(),
        )
        .unwrap();

        checker.join().unwrap();
        assert_eq!(err, None);
        assert_eq!(val.as_string().unwrap().to_str().unwrap(), "woken");
    }

    #[test]
    fn focus_roundtrips_through_ui_channel() {
        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = lua_with_session(Some(tx));
        std::thread::spawn(move || {
            let Ok(UiAction::Session {
                req: SessionRequest::Focus { id },
                reply_tx,
            }) = rx.recv()
            else {
                panic!("expected focus request");
            };
            reply_tx.send(Ok(json!({ "focused": id }))).unwrap();
        });
        let (val, err): (Table, Option<String>) =
            smol::block_on(lua.load("return session.focus('abc')").eval_async()).unwrap();
        assert_eq!(err, None);
        assert_eq!(val.get::<String>("focused").unwrap(), "abc");
    }

    #[test_case("return session.prompt('hi', { session = 'abc' })", Some("abc") ; "explicit_session_id")]
    #[test_case("return session.prompt('hi')", None ; "defaults_to_focused")]
    fn prompt_forwards_text_and_session_id(code: &str, expected_id: Option<&str>) {
        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = lua_with_session(Some(tx));
        let expected_id = expected_id.map(str::to_owned);
        let checker = std::thread::spawn(move || {
            let Ok(UiAction::Session {
                req: SessionRequest::Prompt { id, text },
                reply_tx,
            }) = rx.recv()
            else {
                panic!("expected prompt request");
            };
            assert_eq!(id, expected_id);
            assert_eq!(text, "hi");
            reply_tx.send(Ok(json!("queued"))).unwrap();
        });
        let (val, err): (String, Option<String>) =
            smol::block_on(lua.load(code).eval_async()).unwrap();
        checker.join().unwrap();
        assert_eq!(err, None);
        assert_eq!(val, "queued");
    }

    #[test]
    fn notify_is_synchronous_and_queues_an_observation() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        let (tx, rx) = flume::unbounded::<UiAction>();
        let lua = lua_with_session(Some(tx));
        lua.globals().set("session_id", id.to_string()).unwrap();

        let (value, error): (bool, Option<String>) = lua
            .load("return session.notify('built', { session = session_id })")
            .eval()
            .unwrap();

        assert!(value);
        assert_eq!(error, None);
        let messages = mailbox.drain();
        assert_eq!(messages.len(), 1);
        assert!(messages[0].is_observation());
        assert_eq!(messages[0].user_text(), Some("built"));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn waking_notify_sets_the_mailbox_wake_flag() {
        let id = MakiId::generate();
        let mailbox = SessionMailbox::register(id);
        let lua = lua_with_session(None);
        lua.globals().set("session_id", id.to_string()).unwrap();

        let (value, error): (bool, Option<String>) = lua
            .load("return session.notify('failed', { session = session_id, wake = true })")
            .eval()
            .unwrap();

        assert!(value);
        assert_eq!(error, None);
        assert_eq!(mailbox.claim_wake().len(), 1);
    }

    #[test]
    fn notify_rejects_missing_and_non_live_sessions() {
        let lua = lua_with_session(None);
        let (_, missing): (Value, Option<String>) =
            lua.load("return session.notify('built')").eval().unwrap();
        assert_eq!(missing.as_deref(), Some(SESSION_REQUIRED_ERR));

        let id = MakiId::generate();
        lua.globals().set("session_id", id.to_string()).unwrap();
        let (_, not_live): (Value, Option<String>) = lua
            .load("return session.notify('built', { session = session_id })")
            .eval()
            .unwrap();
        assert_eq!(not_live, Some(format!("session not live: {id}")));
    }

    #[test]
    fn notify_rejects_blank_text_and_invalid_session_ids() {
        let lua = lua_with_session(None);
        let (_, blank): (Value, Option<String>) = lua
            .load("return session.notify(' ', { session = 'invalid' })")
            .eval()
            .unwrap();
        assert_eq!(blank.as_deref(), Some(BLANK_NOTIFY_ERR));

        let (_, invalid): (Value, Option<String>) = lua
            .load("return session.notify('built', { session = 'invalid' })")
            .eval()
            .unwrap();
        assert!(invalid.is_some_and(|error| error.contains("invalid base58")));
    }

    #[test]
    fn set_title_with_wrong_type_throws() {
        let lua = lua_with_session(None);
        let result: LuaResult<Value> =
            smol::block_on(lua.load("return session.set_title('oops')").eval_async());
        assert!(result.unwrap_err().to_string().contains("table"));
    }
}
