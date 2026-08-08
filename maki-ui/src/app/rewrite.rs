//! Redrafts a prompt on request, for [`crate::components::prompt_editor`].
//!
//! Shares the detached-task-and-channel shape with [`super::suggest`] and the
//! same model, but answers a question rather than volunteering one, so it
//! reports failure instead of staying quiet: the user asked and is watching a
//! spinner.
//!
//! The conversation is deliberately not sent. A prompt is usually edited before
//! the first turn, when there is no history to send, and a model given both a
//! transcript and an instruction tends to answer the transcript.

use std::sync::Arc;

use flume::Sender;
use maki_providers::provider::Provider;
use maki_providers::{ContentBlock, Message, Model, RequestOptions};
use maki_storage::id::SessionRef;
use serde_json::Value;

use super::App;

const REWRITE_SYSTEM: &str = "You rewrite prompts for a coding agent.";

const REWRITE_INSTRUCTION: &str = "<system-reminder>\nRewrite the prompt above following the \
instruction.\n- Reply with the rewritten prompt and nothing else. No preamble, no explanation, no \
quotes, no code fences.\n- Keep the user's intent. Change only what the instruction asks for.\n\
- Keep it in the user's voice, as an instruction to a coding agent.\n</system-reminder>";

const FAILED: &str = "rewrite failed";
const EMPTY: &str = "the model returned nothing";

/// A redraft, tagged with the editing session that asked for it so an answer
/// arriving after the modal was closed and reopened is dropped.
pub(crate) struct Rewrite {
    pub(crate) seq: u64,
    pub(crate) text: Result<String, String>,
}

impl App {
    pub(crate) fn start_rewrite(
        &mut self,
        provider: Arc<dyn Provider>,
        model: Model,
        draft: String,
        instruction: String,
    ) {
        let (tx, rx) = flume::bounded(1);
        self.rewrite_rx = Some(rx);

        let messages = vec![
            Message::user(format!(
                "<prompt>\n{draft}\n</prompt>\n\n<instruction>\n{instruction}\n</instruction>"
            )),
            Message::user(REWRITE_INSTRUCTION.to_string()),
        ];
        let session_id = SessionRef::from(self.state.session.id);
        smol::spawn(run_rewrite(
            provider,
            model,
            messages,
            self.rewrite_seq,
            tx,
            Some(session_id),
        ))
        .detach();
    }

    /// Called from the render tick.
    pub(crate) fn poll_rewrite(&mut self) {
        let Some(rx) = self.rewrite_rx.as_ref() else {
            return;
        };
        let Ok(rewrite) = rx.try_recv() else {
            return;
        };
        self.rewrite_rx = None;
        if rewrite.seq != self.rewrite_seq || !self.prompt_editor.is_open() {
            return;
        }
        match rewrite.text {
            Ok(text) => self.prompt_editor.apply_version(&text),
            Err(message) => self.prompt_editor.fail(message),
        }
    }

    /// The redraft never started. Reported in the editor rather than the
    /// statusline, which the modal is covering.
    pub(crate) fn fail_rewrite(&mut self, message: String) {
        self.rewrite_rx = None;
        self.prompt_editor.fail(message);
    }

    /// Bumped whenever a new editing session starts, so answers owed to the
    /// last one are recognisable as stale.
    pub(crate) fn open_prompt_editor(&mut self, draft: &str) {
        self.rewrite_seq += 1;
        self.rewrite_rx = None;
        self.prompt_editor.open(draft);
    }
}

async fn run_rewrite(
    provider: Arc<dyn Provider>,
    model: Model,
    messages: Vec<Message>,
    seq: u64,
    tx: Sender<Rewrite>,
    session_id: Option<SessionRef>,
) {
    let tools = Value::Array(vec![]);
    let messages = maki_providers::adapt_images_for_model(&model, &messages);
    let (event_tx, _event_rx) = flume::unbounded();

    let result = provider
        .stream_message(
            &model,
            &messages,
            REWRITE_SYSTEM,
            &tools,
            &event_tx,
            RequestOptions::default(),
            session_id.as_ref(),
        )
        .await;

    let text = match result {
        Ok(response) => match clean_rewrite(&response.message) {
            text if text.is_empty() => Err(EMPTY.to_string()),
            text => Ok(text),
        },
        Err(e) => Err(format!("{FAILED}: {e}")),
    };
    let _ = tx.send(Rewrite { seq, text });
}

/// Small models fence and preface despite being told not to. Only wrappers
/// around the whole answer are stripped, never anything inside it: a prompt
/// legitimately contains blank lines, dashes and quoted names.
fn clean_rewrite(message: &Message) -> String {
    let text: String = message
        .content
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    let text = text.trim();
    let text = strip_fence(text);
    strip_wrapping_quotes(text.trim()).trim().to_string()
}

fn strip_fence(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("```") else {
        return text;
    };
    // The opening fence may carry a language tag, which belongs to the fence
    // rather than the prompt.
    let rest = rest.split_once('\n').map_or("", |(_, body)| body);
    rest.trim_end()
        .strip_suffix("```")
        .map_or(text, str::trim_end)
}

fn strip_wrapping_quotes(text: &str) -> &str {
    for quote in ['"', '\''] {
        if let Some(inner) = text.strip_prefix(quote).and_then(|t| t.strip_suffix(quote))
            // Only when the quotes wrap everything, or `"a" and "b"` loses its
            // outermost pair and stops parsing as the user wrote it.
            && !inner.contains(quote)
        {
            return inner;
        }
    }
    text
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

    #[test_case("add auth",                      "add auth"        ; "already_clean")]
    #[test_case("  add auth  ",                  "add auth"        ; "padded")]
    #[test_case("\"add auth\"",                  "add auth"        ; "quoted")]
    #[test_case("'add auth'",                    "add auth"        ; "single_quoted")]
    #[test_case("```\nadd auth\n```",            "add auth"        ; "bare_fence")]
    #[test_case("```text\nadd auth\n```",        "add auth"        ; "tagged_fence")]
    fn wrappers_are_stripped(raw: &str, expected: &str) {
        assert_eq!(clean_rewrite(&text_message(raw)), expected);
    }

    /// Only the outermost pair, and only when it wraps the whole answer.
    #[test]
    fn inner_quotes_survive() {
        let raw = "rename \"foo\" to \"bar\"";
        assert_eq!(clean_rewrite(&text_message(raw)), raw);
    }

    #[test]
    fn multi_line_prompts_keep_their_shape() {
        let raw = "do this:\n\n- one\n- two";
        assert_eq!(clean_rewrite(&text_message(raw)), raw);
    }

    #[test]
    fn an_unterminated_fence_is_left_alone() {
        let raw = "```\nadd auth";
        assert_eq!(clean_rewrite(&text_message(raw)), raw);
    }

    #[test]
    fn empty_content_is_empty() {
        assert!(clean_rewrite(&text_message("   \n  ")).is_empty());
    }
}
