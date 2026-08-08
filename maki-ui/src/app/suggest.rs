//! Drafts follow-up prompts once a turn ends.
//!
//! Shaped like [`super::btw`]: a detached task, a channel the render tick
//! drains, and no touching of agent history. It differs in two ways that matter.
//! It waits for the whole answer instead of streaming, because three prompts
//! appearing one word at a time would be worse than three appearing at once.
//! And it carries the run it belongs to, so an answer that arrives after the
//! next turn started is dropped rather than offered against the wrong context.

use std::sync::Arc;

use flume::Sender;
use maki_providers::provider::Provider;
use maki_providers::{ContentBlock, Message, Model, RequestOptions};
use maki_storage::id::SessionRef;
use serde_json::Value;

use super::App;

const MAX_SUGGESTIONS: usize = 3;
/// Long enough to be a real prompt, short enough to read at a glance.
const MAX_SUGGESTION_CHARS: usize = 90;

const SUGGEST_SYSTEM: &str = "You suggest what the user might ask next.";

const SUGGEST_INSTRUCTION: &str = "<system-reminder>\nRead the conversation above and write up to \
three prompts the user might plausibly send next.\n- One per line. No numbering, no bullets, no \
quotes, no commentary.\n- Write them as the user would type them, in the imperative.\n- Keep each \
under 12 words.\n- Suggest concrete next steps that follow from what just happened. Never suggest \
repeating work that is already done.\n- If nothing useful comes to mind, write nothing at all.\n\
</system-reminder>";

/// The generated prompts, tagged with the run they were drafted for.
pub(crate) struct Suggestions {
    pub(crate) run_id: u64,
    pub(crate) prompts: Vec<String>,
}

impl App {
    /// No-ops without a history mirror, which is the case for restored and
    /// subagent apps, and when the suggest role has no model pinned.
    pub(crate) fn start_suggestions(&mut self, provider: Arc<dyn Provider>, model: Model) {
        let Some(history) = self.shared_history.as_ref() else {
            return;
        };
        let mut messages = Vec::clone(&history.load().messages);
        if messages.is_empty() {
            return;
        }
        maki_agent::close_dangling_tool_calls(&mut messages, maki_agent::UNAVAILABLE_RESULT);
        messages.push(Message::user(SUGGEST_INSTRUCTION.to_string()));

        let (tx, rx) = flume::bounded(1);
        self.suggest_rx = Some(rx);

        let session_id = SessionRef::from(self.state.session.id);
        smol::spawn(run_suggest(
            provider,
            model,
            messages,
            self.run_id,
            tx,
            Some(session_id),
        ))
        .detach();
    }

    /// Called from the render tick. Silent on failure: a suggestion that did
    /// not arrive is not worth a message, and an error here is never the user's
    /// problem to solve.
    pub(crate) fn poll_suggestions(&mut self) {
        let Some(rx) = self.suggest_rx.as_ref() else {
            return;
        };
        let Ok(suggestions) = rx.try_recv() else {
            return;
        };
        self.suggest_rx = None;
        if suggestions.run_id != self.run_id || suggestions.prompts.is_empty() {
            return;
        }
        self.suggestions = suggestions.prompts;
        self.suggestions_hidden = false;
    }

    pub(crate) fn showing_suggestions(&self) -> bool {
        !self.suggestions.is_empty() && !self.suggestions_hidden
    }

    /// What the panel should draw: empty while hidden, so layout gives the rows
    /// back to the chat instead of leaving a gap.
    pub(crate) fn shown_suggestions(&self) -> &[String] {
        if self.suggestions_hidden {
            &[]
        } else {
            &self.suggestions
        }
    }

    /// Kept rather than dropped, so `ctrl+s` can put the same set back without
    /// paying for a second draft of prompts we already have.
    pub(crate) fn hide_suggestions(&mut self) {
        self.suggestions_hidden = true;
    }

    pub(crate) fn clear_suggestions(&mut self) {
        self.suggestions.clear();
        self.suggestions_hidden = false;
        self.suggest_rx = None;
    }
}

async fn run_suggest(
    provider: Arc<dyn Provider>,
    model: Model,
    messages: Vec<Message>,
    run_id: u64,
    tx: Sender<Suggestions>,
    session_id: Option<SessionRef>,
) {
    let tools = Value::Array(vec![]);
    let messages = maki_providers::adapt_images_for_model(&model, &messages);
    // Events are dropped on the floor: nothing shows until the whole answer is
    // in, so there is nothing to stream.
    let (event_tx, _event_rx) = flume::unbounded();

    let result = provider
        .stream_message(
            &model,
            &messages,
            SUGGEST_SYSTEM,
            &tools,
            &event_tx,
            RequestOptions::default(),
            session_id.as_ref(),
        )
        .await;

    if let Ok(response) = result {
        let prompts = parse_suggestions(&response.message);
        if !prompts.is_empty() {
            let _ = tx.send(Suggestions { run_id, prompts });
        }
    }
}

/// Small models decorate despite being asked not to, so strip the usual
/// leading bullet or numbering rather than showing it.
fn parse_suggestions(message: &Message) -> Vec<String> {
    message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .flat_map(str::lines)
        .map(strip_decoration)
        .filter(|line| !line.is_empty() && line.chars().count() <= MAX_SUGGESTION_CHARS)
        .take(MAX_SUGGESTIONS)
        .collect()
}

fn strip_decoration(line: &str) -> String {
    let line = line.trim();
    let line = line
        .trim_start_matches(['-', '*', '•'])
        .trim_start_matches(|c: char| c.is_ascii_digit())
        .trim_start_matches(['.', ')'])
        .trim();
    line.trim_matches('"').trim().to_string()
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

    #[test_case("- run the tests",       "run the tests"    ; "dash_bullet")]
    #[test_case("2. run the tests",      "run the tests"    ; "numbered")]
    #[test_case("• run the tests",       "run the tests"    ; "unicode_bullet")]
    #[test_case("\"run the tests\"",     "run the tests"    ; "quoted")]
    #[test_case("  run the tests  ",     "run the tests"    ; "padded")]
    #[test_case("run the tests",         "run the tests"    ; "already_clean")]
    fn decoration_is_stripped(raw: &str, expected: &str) {
        assert_eq!(strip_decoration(raw), expected);
    }

    #[test]
    fn takes_at_most_three_non_empty_lines() {
        let msg = text_message("one\n\ntwo\nthree\nfour");
        assert_eq!(parse_suggestions(&msg), vec!["one", "two", "three"]);
    }

    /// A model that ignores the length limit would push the input box off
    /// screen, so overlong lines are dropped rather than truncated.
    #[test]
    fn overlong_lines_are_dropped() {
        let long = "x".repeat(MAX_SUGGESTION_CHARS + 1);
        let msg = text_message(&format!("keep this\n{long}"));
        assert_eq!(parse_suggestions(&msg), vec!["keep this"]);
    }

    #[test]
    fn nothing_useful_yields_nothing() {
        assert!(parse_suggestions(&text_message("   \n\n  ")).is_empty());
    }
}
