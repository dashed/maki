//! Finishes the sentence you are part-way through typing, on request.
//!
//! Asked for with a keypress rather than volunteered on a timer. That is a
//! cheaper design in the obvious way — no calls go out while you type — but the
//! reason it reads better is that a completion nobody asked for has to be
//! silent about failing, and this one does not: every refusal here says why,
//! because a key that appears to do nothing is worse than a slow one.
//!
//! The conversation is deliberately not sent, as in [`super::rewrite`].
//!
//! The model is asked for the *whole* prompt rather than just the tail, and the
//! tail is recovered by stripping the prefix back off. That costs a few tokens
//! and buys the only reliable way to tell a real completion from a model that
//! quietly reworded what the user already typed: if the reply does not start
//! with their text verbatim, it is dropped rather than reconciled.

use std::sync::Arc;

use flume::Sender;
use maki_providers::provider::Provider;
use maki_providers::{ContentBlock, Message, Model, RequestOptions};
use maki_storage::id::SessionRef;
use serde_json::Value;

use super::{Action, App};

/// A ghost longer than this stops being a hint and becomes a wall of text.
const MAX_COMPLETION_CHARS: usize = 120;

const COMPLETE_SYSTEM: &str = "You complete half-written prompts for a coding agent.";

const COMPLETE_INSTRUCTION: &str = "<system-reminder>\nThe text above is a prompt the user is \
part-way through typing.\n- Reply with the whole prompt, completed.\n- Begin by reproducing the \
text above exactly, character for character, including any trailing space.\n- Then continue it, \
adding at most one short sentence.\n- Finish the user's thought. Do not answer it, and do not \
start a new one.\n- Reply with the prompt only. No preamble, no explanation, no quotes, no code \
fences.\n</system-reminder>";

pub(crate) const HINT: &str = "→ accept, alt+→ one word, esc dismiss";
pub(crate) const WORKING: &str = "completing…";
pub(crate) const NOTHING_TO_COMPLETE: &str = "nothing to complete yet";
pub(crate) const NOT_PROSE: &str = "completion is for prose, not commands";
pub(crate) const NO_COMPLETION: &str = "no completion for this draft";
pub(crate) const FAILED: &str = "completion failed";

/// A completion, tagged with the draft it continues so it can be discarded if
/// the user kept typing while it was in flight.
pub(crate) struct Completion {
    pub(crate) prefix: String,
    pub(crate) tail: Result<String, String>,
}

impl App {
    /// The keypress. Refusals are flashed rather than swallowed: the user
    /// pressed a key and is owed an answer either way.
    pub(crate) fn request_completion(&mut self) -> Vec<Action> {
        if self.completion_rx.is_some() {
            return vec![];
        }
        let prefix = self.input_box.buffer.value();
        if prefix.trim().is_empty() {
            self.flash(NOTHING_TO_COMPLETE.into());
            return vec![];
        }
        if !completable(&prefix) {
            self.flash(NOT_PROSE.into());
            return vec![];
        }
        if !self.input_box.cursor_at_end() {
            // A completion continues the end of the text, so it would appear
            // somewhere the cursor is not.
            self.input_box.buffer.move_to_end();
        }
        self.flash(WORKING.into());
        vec![Action::Complete(prefix)]
    }

    pub(crate) fn start_completion(
        &mut self,
        provider: Arc<dyn Provider>,
        model: Model,
        prefix: String,
    ) {
        let (tx, rx) = flume::bounded(1);
        self.completion_rx = Some(rx);

        let messages = vec![
            Message::user(prefix.clone()),
            Message::user(COMPLETE_INSTRUCTION.to_string()),
        ];
        let session_id = SessionRef::from(self.state.session.id);
        smol::spawn(run_complete(
            provider,
            model,
            messages,
            prefix,
            tx,
            Some(session_id),
        ))
        .detach();
    }

    /// Called from the render tick.
    pub(crate) fn poll_completion(&mut self) {
        let Some(rx) = self.completion_rx.as_ref() else {
            return;
        };
        let Ok(completion) = rx.try_recv() else {
            return;
        };
        self.completion_rx = None;
        // Typing does not stop while a request is in flight, and a completion
        // for text that has moved on would be nonsense.
        if completion.prefix != self.input_box.buffer.value() {
            return;
        }
        match completion.tail {
            Ok(tail) => {
                self.input_box.set_ghost(Some(tail));
                if self.input_box.ghost().is_some() {
                    // An unexplained dim tail is a puzzle exactly once.
                    if self.completion_hinted {
                        self.status_bar.clear_flash();
                    } else {
                        self.completion_hinted = true;
                        self.flash(HINT.into());
                    }
                }
            }
            Err(message) => self.flash(message),
        }
    }

    /// The request never started.
    pub(crate) fn fail_completion(&mut self, message: String) {
        self.completion_rx = None;
        self.flash(message);
    }
}

/// Slash commands belong to the palette and `!` to the shell, both of which
/// already complete their own input better than a model could.
fn completable(text: &str) -> bool {
    let trimmed = text.trim_start();
    !trimmed.starts_with('/') && !trimmed.starts_with('!')
}

async fn run_complete(
    provider: Arc<dyn Provider>,
    model: Model,
    messages: Vec<Message>,
    prefix: String,
    tx: Sender<Completion>,
    session_id: Option<SessionRef>,
) {
    let tools = Value::Array(vec![]);
    let messages = maki_providers::adapt_images_for_model(&model, &messages);
    let (event_tx, _event_rx) = flume::unbounded();

    let result = provider
        .stream_message(
            &model,
            &messages,
            COMPLETE_SYSTEM,
            &tools,
            &event_tx,
            RequestOptions::default(),
            session_id.as_ref(),
        )
        .await;

    let tail = match result {
        Ok(response) => {
            parse_completion(&response.message, &prefix).ok_or_else(|| NO_COMPLETION.to_string())
        }
        Err(e) => Err(format!("{FAILED}: {e}")),
    };
    let _ = tx.send(Completion { prefix, tail });
}

/// `None` whenever the reply is not the user's text plus something. Rejecting
/// is the point: a model that reworded the prefix would otherwise have its
/// edit silently applied to text the user is still looking at.
fn parse_completion(message: &Message, prefix: &str) -> Option<String> {
    let full: String = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");

    let full = strip_fence(full.trim_end());
    let tail = full.strip_prefix(prefix)?;
    // One line only: a ghost that grows the input box by three rows is worse
    // than no ghost.
    let tail = tail.split('\n').next()?.trim_end();
    if tail.is_empty() || tail.chars().count() > MAX_COMPLETION_CHARS {
        return None;
    }
    Some(tail.to_string())
}

fn strip_fence(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("```") else {
        return text;
    };
    let rest = rest.split_once('\n').map_or("", |(_, body)| body);
    rest.trim_end()
        .strip_suffix("```")
        .map_or(text, str::trim_end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    fn text_message(body: &str) -> Message {
        Message {
            content: vec![ContentBlock::Text {
                text: body.to_string(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn the_tail_is_what_the_user_has_not_typed() {
        let msg = text_message("add auth to the login page");
        assert_eq!(
            parse_completion(&msg, "add auth"),
            Some(" to the login page".into())
        );
    }

    /// Mid-word is the case that makes an exact prefix match necessary: no
    /// separator survives to reconstruct the join from.
    #[test]
    fn a_completion_can_land_mid_word() {
        let msg = text_message("add authentication");
        assert_eq!(
            parse_completion(&msg, "add auth"),
            Some("entication".into())
        );
    }

    #[test_case("Add auth to login"      ; "changed_case")]
    #[test_case("please add auth to it"  ; "prepended")]
    #[test_case("write the login page"   ; "rewrote_it")]
    fn a_reply_that_does_not_start_with_the_draft_is_dropped(reply: &str) {
        assert_eq!(parse_completion(&text_message(reply), "add auth"), None);
    }

    #[test]
    fn a_fenced_reply_is_unwrapped_before_matching() {
        let msg = text_message("```\nadd auth to the login page\n```");
        assert_eq!(
            parse_completion(&msg, "add auth"),
            Some(" to the login page".into())
        );
    }

    #[test]
    fn only_the_first_line_is_offered() {
        let msg = text_message("add auth to login\nand also add tests");
        assert_eq!(parse_completion(&msg, "add auth"), Some(" to login".into()));
    }

    #[test]
    fn a_reply_that_adds_nothing_is_dropped() {
        assert_eq!(
            parse_completion(&text_message("add auth"), "add auth"),
            None
        );
    }

    #[test]
    fn an_overlong_completion_is_dropped() {
        let long = "x".repeat(MAX_COMPLETION_CHARS + 1);
        let msg = text_message(&format!("add auth{long}"));
        assert_eq!(parse_completion(&msg, "add auth"), None);
    }

    #[test_case("add auth",   true   ; "ordinary_prose")]
    #[test_case("/model",     false  ; "slash_command")]
    #[test_case("  /model",   false  ; "indented_slash_command")]
    #[test_case("!ls -la",    false  ; "shell")]
    fn only_prose_is_worth_completing(text: &str, expected: bool) {
        assert_eq!(completable(text), expected);
    }
}
