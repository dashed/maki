//! An editor for a prompt before it is sent.
//!
//! Two ways to change the draft, because they fail in different places. Typing
//! is exact but slow, and asking a model to reword is fast but approximate.
//! So the draft pane and the instruction box are both live and `tab` moves
//! between them.
//!
//! Every rewrite becomes a new version rather than overwriting the last, which
//! is what makes asking cheap: `ctrl+u` walks back out of a reword that made
//! things worse, so a bad instruction costs a keystroke instead of the draft.
//! Nothing reaches the input box until the draft is accepted.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::components::modal::Modal;
use crate::components::{Overlay, hint_line};
use crate::text_buffer::{EditResult, TextBuffer, is_newline_key};
use crate::theme;

const TITLE: &str = " Improve prompt ";
const WIDTH_PERCENT: u16 = 70;
const MAX_HEIGHT_PERCENT: u16 = 70;
/// Rows of draft kept on screen before it scrolls to follow the cursor.
const MAX_DRAFT_ROWS: usize = 12;
const PROMPT: &str = "> ";
const WORKING: &str = "rewriting…";
const INSTRUCTION_LABEL: &str = "how should this change?";
const DRAFT_LABEL: &str = "prompt";
const EMPTY_HINT: &str = "(empty)";

pub enum PromptEditorAction {
    Consumed,
    /// Ask the suggest model to redraft `draft` following `instruction`.
    Rewrite {
        draft: String,
        instruction: String,
    },
    /// Put this text in the input box and close.
    Accept(String),
    Close,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Focus {
    Draft,
    Instruction,
}

pub struct PromptEditor {
    open: bool,
    draft: TextBuffer,
    instruction: TextBuffer,
    focus: Focus,
    /// Older drafts, oldest first. Only rewrites push here: hand edits are not
    /// undo steps, because a per-keystroke history would bury the versions
    /// `ctrl+u` exists to reach.
    history: Vec<String>,
    pending: bool,
    error: Option<String>,
    /// Rows of draft scrolled off the top.
    scroll: usize,
}

impl Default for PromptEditor {
    fn default() -> Self {
        Self::new()
    }
}

impl PromptEditor {
    pub fn new() -> Self {
        Self {
            open: false,
            draft: TextBuffer::new(String::new()),
            instruction: TextBuffer::new(String::new()),
            focus: Focus::Instruction,
            history: Vec::new(),
            pending: false,
            error: None,
            scroll: 0,
        }
    }

    /// Opens on the instruction box: arriving with a draft in hand, the next
    /// thing to say is usually what is wrong with it, not a hand edit.
    pub fn open(&mut self, draft: &str) {
        self.open = true;
        self.draft = TextBuffer::new(draft.to_string());
        self.draft.move_to_end();
        self.instruction.clear();
        self.focus = Focus::Instruction;
        self.history.clear();
        self.pending = false;
        self.error = None;
        self.scroll = 0;
    }

    pub fn is_open(&self) -> bool {
        self.open
    }

    #[cfg(test)]
    pub fn is_pending(&self) -> bool {
        self.pending
    }

    #[cfg(test)]
    pub fn draft(&self) -> String {
        self.draft.value()
    }

    #[cfg(test)]
    pub fn version_count(&self) -> usize {
        self.history.len()
    }

    /// A rewrite came back. The old draft becomes a version rather than being
    /// dropped, and focus returns to the instruction box for the next round.
    pub fn apply_version(&mut self, text: &str) {
        self.pending = false;
        self.error = None;
        let previous = self.draft.value();
        if previous != text {
            self.history.push(previous);
        }
        self.draft = TextBuffer::new(text.to_string());
        self.draft.move_to_end();
        self.instruction.clear();
        self.focus = Focus::Instruction;
        self.scroll = 0;
    }

    /// Shown in place of the working line. The instruction survives so it can
    /// be retried or edited rather than retyped.
    pub fn fail(&mut self, message: String) {
        self.pending = false;
        self.error = Some(message);
    }

    fn undo(&mut self) {
        let Some(previous) = self.history.pop() else {
            return;
        };
        self.draft = TextBuffer::new(previous);
        self.draft.move_to_end();
        self.error = None;
        self.scroll = 0;
    }

    fn submit_instruction(&mut self) -> PromptEditorAction {
        let instruction = self.instruction.value().trim().to_string();
        let draft = self.draft.value();
        // An empty box means there is nothing left to ask for, so enter finishes
        // instead of stalling on a keystroke that would otherwise do nothing.
        if instruction.is_empty() {
            return self.accept();
        }
        if draft.trim().is_empty() || self.pending {
            return PromptEditorAction::Consumed;
        }
        self.pending = true;
        self.error = None;
        PromptEditorAction::Rewrite { draft, instruction }
    }

    fn accept(&mut self) -> PromptEditorAction {
        let draft = self.draft.value();
        if draft.trim().is_empty() {
            return PromptEditorAction::Consumed;
        }
        self.close();
        PromptEditorAction::Accept(draft)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> PromptEditorAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        if key.code == KeyCode::Esc {
            self.close();
            return PromptEditorAction::Close;
        }
        if ctrl && matches!(key.code, KeyCode::Char('s')) {
            return self.accept();
        }
        if ctrl && matches!(key.code, KeyCode::Char('u')) {
            self.undo();
            return PromptEditorAction::Consumed;
        }
        if key.code == KeyCode::Tab || key.code == KeyCode::BackTab {
            self.focus = match self.focus {
                Focus::Draft => Focus::Instruction,
                Focus::Instruction => Focus::Draft,
            };
            return PromptEditorAction::Consumed;
        }

        match self.focus {
            Focus::Instruction => {
                if key.code == KeyCode::Enter && !is_newline_key(&key) {
                    return self.submit_instruction();
                }
                self.instruction.handle_key(key);
            }
            Focus::Draft => {
                // Enter is a newline here: this pane holds a prompt that may
                // well be several lines, and submitting from it would make the
                // two panes disagree about what enter means.
                if key.code == KeyCode::Enter {
                    self.draft.add_line();
                    return PromptEditorAction::Consumed;
                }
                if self.draft.handle_key(key) != EditResult::Ignored {
                    // A hand edit invalidates nothing, but it does mean the
                    // error on screen is about an older draft.
                    self.error = None;
                }
            }
        }
        PromptEditorAction::Consumed
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        if !self.open {
            return Rect::default();
        }
        let t = theme::current();

        // Measured before the modal is sized, because how tall the draft wants
        // to be is what decides how tall the modal is.
        let probe = Modal {
            title: TITLE,
            width_percent: WIDTH_PERCENT,
            max_height_percent: MAX_HEIGHT_PERCENT,
        };
        let width = (area.width as u32 * WIDTH_PERCENT as u32 / 100).saturating_sub(2) as u16;
        let width = width.max(1) as usize;

        let draft_rows = buffer_rows(&self.draft, width);
        let visible_rows = draft_rows.len().clamp(1, MAX_DRAFT_ROWS);
        self.scroll = clamp_scroll(
            self.scroll,
            cursor_row(&self.draft, width),
            visible_rows,
            draft_rows.len(),
        );

        let mut lines: Vec<Line> = Vec::new();
        lines.push(label_line(DRAFT_LABEL, self.focus == Focus::Draft));
        lines.extend(render_buffer(
            &self.draft,
            width,
            self.focus == Focus::Draft,
            self.scroll,
            visible_rows,
        ));
        lines.push(Line::default());
        lines.push(label_line(
            INSTRUCTION_LABEL,
            self.focus == Focus::Instruction,
        ));

        if self.pending {
            lines.push(Line::from(Span::styled(
                format!("{PROMPT}{WORKING}"),
                t.tool_dim,
            )));
        } else {
            let mut instruction = render_buffer(
                &self.instruction,
                width.saturating_sub(PROMPT.len()).max(1),
                self.focus == Focus::Instruction,
                0,
                1,
            );
            let first = instruction.remove(0);
            let mut spans = vec![Span::styled(PROMPT, t.tool_dim)];
            spans.extend(first.spans);
            lines.push(Line::from(spans));
        }

        if let Some(ref error) = self.error {
            lines.push(Line::from(Span::styled(error.clone(), t.error)));
        }
        lines.push(Line::default());
        lines.extend(self.footer(width));

        let total = lines.len() as u16;
        let (popup, inner) = probe.render(frame, area, total);
        frame.render_widget(Paragraph::new(lines), inner);
        popup
    }

    /// Wrapped rather than truncated: five hints do not fit an 80-column
    /// terminal, and the ones that fall off the end are the ones a first-time
    /// user most needs.
    fn footer(&self, width: usize) -> Vec<Line<'static>> {
        let mut pairs: Vec<(&str, &str)> = vec![("tab", "switch pane")];
        if self.focus == Focus::Instruction {
            pairs.push(("enter", "rewrite"));
        }
        pairs.push(("ctrl+s", "accept"));
        if !self.history.is_empty() {
            pairs.push(("ctrl+u", "undo"));
        }
        pairs.push(("esc", "discard"));
        pack_hints(&pairs, width)
    }
}

/// Greedy: each hint keeps its key and description together on one line, so a
/// key never ends up stranded from what it does.
fn pack_hints(pairs: &[(&str, &str)], width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let mut row: Vec<(&str, &str)> = Vec::new();
    let mut used = 0;
    for &(key, desc) in pairs {
        let cost = key.len() + desc.len() + 3;
        if !row.is_empty() && used + cost > width {
            lines.push(hint_line(&row));
            row.clear();
            used = 0;
        }
        used += cost;
        row.push((key, desc));
    }
    if !row.is_empty() {
        lines.push(hint_line(&row));
    }
    lines
}

fn label_line(text: &str, focused: bool) -> Line<'static> {
    let t = theme::current();
    let style = if focused { t.panel_title } else { t.tool_dim };
    Line::from(Span::styled(text.to_string(), style))
}

/// Char-wrapped rather than word-wrapped, so the cursor row and column below
/// are computed the same way they are drawn.
fn buffer_rows(buffer: &TextBuffer, width: usize) -> Vec<(usize, usize)> {
    let mut rows = Vec::new();
    for (y, line) in buffer.lines().iter().enumerate() {
        let len = line.chars().count();
        let mut start = 0;
        loop {
            let end = (start + width).min(len);
            rows.push((y, start));
            if end >= len {
                break;
            }
            start = end;
        }
    }
    rows
}

fn cursor_row(buffer: &TextBuffer, width: usize) -> usize {
    buffer_rows(buffer, width)
        .iter()
        .rposition(|&(y, start)| y < buffer.y() || (y == buffer.y() && start <= buffer.x()))
        .unwrap_or(0)
}

fn clamp_scroll(scroll: usize, cursor: usize, visible: usize, total: usize) -> usize {
    let max_scroll = total.saturating_sub(visible);
    let scroll = scroll.min(max_scroll);
    if cursor < scroll {
        cursor
    } else if cursor >= scroll + visible {
        cursor + 1 - visible
    } else {
        scroll
    }
}

/// The cursor is a styled cell rather than the terminal's own, matching the
/// search box: maki hides the real cursor, so a pane without this looks dead.
fn render_buffer(
    buffer: &TextBuffer,
    width: usize,
    focused: bool,
    scroll: usize,
    visible: usize,
) -> Vec<Line<'static>> {
    let t = theme::current();
    let rows = buffer_rows(buffer, width);
    let cursor = cursor_row(buffer, width);
    let mut out = Vec::new();

    for (row_idx, &(y, start)) in rows.iter().enumerate().skip(scroll).take(visible) {
        let chars: Vec<char> = buffer.lines()[y].chars().skip(start).take(width).collect();
        let text: String = chars.iter().collect();
        if focused && row_idx == cursor {
            let col = buffer.x().saturating_sub(start).min(chars.len());
            let before: String = chars[..col].iter().collect();
            let under = chars.get(col).copied().unwrap_or(' ');
            let after: String = chars[(col + 1).min(chars.len())..].iter().collect();
            out.push(Line::from(vec![
                Span::styled(before, Style::default()),
                Span::styled(under.to_string(), t.cursor),
                Span::styled(after, Style::default()),
            ]));
        } else {
            out.push(Line::from(Span::styled(text, Style::default())));
        }
    }

    if out.is_empty() {
        let style = if focused { t.cursor } else { t.tool_dim };
        out.push(Line::from(vec![
            Span::styled(" ", style),
            Span::styled(EMPTY_HINT, t.tool_dim),
        ]));
    }
    out
}

impl Overlay for PromptEditor {
    fn is_open(&self) -> bool {
        self.open
    }

    fn close(&mut self) {
        self.open = false;
        self.pending = false;
        self.error = None;
        self.history.clear();
        self.instruction.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEventKind, KeyEventState};

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn plain(code: KeyCode) -> KeyEvent {
        key(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        key(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn typed(editor: &mut PromptEditor, text: &str) {
        for c in text.chars() {
            editor.handle_key(plain(KeyCode::Char(c)));
        }
    }

    fn opened(draft: &str) -> PromptEditor {
        let mut editor = PromptEditor::new();
        editor.open(draft);
        editor
    }

    #[test]
    fn typing_lands_in_the_instruction_box_first() {
        let mut editor = opened("add auth");
        typed(&mut editor, "shorter");
        assert_eq!(editor.instruction.value(), "shorter");
        assert_eq!(editor.draft(), "add auth");
    }

    /// The whole point of the manual pane: tab over and the same keystrokes
    /// edit the prompt itself.
    #[test]
    fn tab_moves_typing_to_the_draft() {
        let mut editor = opened("add auth");
        editor.handle_key(plain(KeyCode::Tab));
        typed(&mut editor, "!");
        assert_eq!(editor.draft(), "add auth!");
        assert_eq!(editor.instruction.value(), "");
    }

    #[test]
    fn tab_toggles_back() {
        let mut editor = opened("draft");
        editor.handle_key(plain(KeyCode::Tab));
        editor.handle_key(plain(KeyCode::Tab));
        typed(&mut editor, "x");
        assert_eq!(editor.instruction.value(), "x");
    }

    #[test]
    fn enter_in_the_draft_adds_a_line_rather_than_submitting() {
        let mut editor = opened("one");
        editor.handle_key(plain(KeyCode::Tab));
        editor.handle_key(plain(KeyCode::Enter));
        typed(&mut editor, "two");
        assert_eq!(editor.draft(), "one\ntwo");
        assert!(editor.is_open());
    }

    #[test]
    fn enter_with_an_instruction_asks_for_a_rewrite() {
        let mut editor = opened("add auth");
        typed(&mut editor, "be specific");
        match editor.handle_key(plain(KeyCode::Enter)) {
            PromptEditorAction::Rewrite { draft, instruction } => {
                assert_eq!(draft, "add auth");
                assert_eq!(instruction, "be specific");
            }
            _ => panic!("expected a rewrite"),
        }
        assert!(editor.is_pending());
    }

    /// Nothing left to ask for means the draft is done, so enter finishes
    /// instead of being a key that does nothing.
    #[test]
    fn enter_on_an_empty_instruction_accepts() {
        let mut editor = opened("add auth");
        match editor.handle_key(plain(KeyCode::Enter)) {
            PromptEditorAction::Accept(text) => assert_eq!(text, "add auth"),
            _ => panic!("expected accept"),
        }
        assert!(!editor.is_open());
    }

    #[test]
    fn ctrl_s_accepts_from_the_draft_pane() {
        let mut editor = opened("add auth");
        editor.handle_key(plain(KeyCode::Tab));
        typed(&mut editor, "!");
        match editor.handle_key(ctrl('s')) {
            PromptEditorAction::Accept(text) => assert_eq!(text, "add auth!"),
            _ => panic!("expected accept"),
        }
    }

    #[test]
    fn esc_discards_without_yielding_text() {
        let mut editor = opened("add auth");
        assert!(matches!(
            editor.handle_key(plain(KeyCode::Esc)),
            PromptEditorAction::Close
        ));
        assert!(!editor.is_open());
    }

    #[test]
    fn an_empty_draft_cannot_be_accepted() {
        let mut editor = opened("   ");
        assert!(matches!(
            editor.handle_key(ctrl('s')),
            PromptEditorAction::Consumed
        ));
        assert!(editor.is_open());
    }

    #[test]
    fn a_second_rewrite_is_refused_while_one_is_in_flight() {
        let mut editor = opened("add auth");
        typed(&mut editor, "shorter");
        editor.handle_key(plain(KeyCode::Enter));
        typed(&mut editor, "again");
        assert!(matches!(
            editor.handle_key(plain(KeyCode::Enter)),
            PromptEditorAction::Consumed
        ));
    }

    #[test]
    fn a_version_replaces_the_draft_and_clears_the_instruction() {
        let mut editor = opened("add auth");
        typed(&mut editor, "be specific");
        editor.handle_key(plain(KeyCode::Enter));
        editor.apply_version("add token-based auth to the login page");

        assert_eq!(editor.draft(), "add token-based auth to the login page");
        assert_eq!(editor.instruction.value(), "");
        assert!(!editor.is_pending());
        assert_eq!(editor.version_count(), 1);
    }

    #[test]
    fn undo_walks_back_out_of_a_rewrite() {
        let mut editor = opened("add auth");
        editor.apply_version("add token-based auth");
        editor.handle_key(ctrl('u'));
        assert_eq!(editor.draft(), "add auth");
        assert_eq!(editor.version_count(), 0);
    }

    #[test]
    fn undo_with_no_versions_is_harmless() {
        let mut editor = opened("add auth");
        editor.handle_key(ctrl('u'));
        assert_eq!(editor.draft(), "add auth");
    }

    /// Hand edits are not undo steps, or `ctrl+u` would spend a press per
    /// character before reaching the version the user meant.
    #[test]
    fn hand_edits_do_not_become_versions() {
        let mut editor = opened("add auth");
        editor.handle_key(plain(KeyCode::Tab));
        typed(&mut editor, " now");
        assert_eq!(editor.version_count(), 0);
    }

    #[test]
    fn a_failed_rewrite_keeps_the_instruction_for_a_retry() {
        let mut editor = opened("add auth");
        typed(&mut editor, "shorter");
        editor.handle_key(plain(KeyCode::Enter));
        editor.fail("model unavailable".into());

        assert!(!editor.is_pending());
        assert_eq!(editor.instruction.value(), "shorter");
        assert_eq!(editor.draft(), "add auth");
    }

    #[test]
    fn reopening_drops_the_previous_session() {
        let mut editor = opened("first");
        editor.apply_version("second");
        editor.open("third");
        assert_eq!(editor.draft(), "third");
        assert_eq!(editor.version_count(), 0);
    }

    #[test]
    fn cursor_row_follows_a_wrapped_line() {
        let buffer = TextBuffer::new("abcdef".to_string());
        assert_eq!(cursor_row(&buffer, 3), 0);
        let mut buffer = TextBuffer::new("abcdef".to_string());
        buffer.move_to_end();
        assert_eq!(cursor_row(&buffer, 3), 1);
    }

    #[test]
    fn scroll_follows_the_cursor_in_both_directions() {
        assert_eq!(clamp_scroll(0, 15, 12, 20), 4);
        assert_eq!(clamp_scroll(8, 2, 12, 20), 2);
        assert_eq!(clamp_scroll(0, 3, 12, 20), 0);
    }

    fn render(editor: &mut PromptEditor, width: u16, height: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                editor.view(frame, Rect::new(0, 0, width, height));
            })
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..buf.area.height)
            .map(|row| {
                (0..buf.area.width)
                    .map(|col| buf.cell((col, row)).unwrap().symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn both_panes_are_labelled_on_screen() {
        let mut editor = opened("add auth");
        let screen = render(&mut editor, 80, 24);
        assert!(screen.contains("add auth"), "{screen}");
        assert!(screen.contains(DRAFT_LABEL), "{screen}");
        assert!(screen.contains(INSTRUCTION_LABEL), "{screen}");
    }

    #[test]
    fn a_pending_rewrite_says_so() {
        let mut editor = opened("add auth");
        typed(&mut editor, "shorter");
        editor.handle_key(plain(KeyCode::Enter));
        assert!(render(&mut editor, 80, 24).contains(WORKING));
    }

    #[test]
    fn a_failure_is_shown_in_the_modal() {
        let mut editor = opened("add auth");
        editor.fail("model unavailable".into());
        assert!(render(&mut editor, 80, 24).contains("model unavailable"));
    }

    /// The undo hint only earns its place once there is something to undo.
    #[test]
    fn the_undo_hint_appears_with_the_first_version() {
        let mut editor = opened("add auth");
        assert!(!render(&mut editor, 120, 24).contains("undo"));
        editor.apply_version("add token auth");
        assert!(render(&mut editor, 120, 24).contains("undo"));
    }

    /// A hint that does not fit is worth a second row: dropping it silently
    /// hides the key a first-time user most needs.
    #[test]
    fn hints_wrap_instead_of_being_cut_off() {
        let pairs = [("tab", "switch pane"), ("esc", "discard")];
        assert_eq!(pack_hints(&pairs, 80).len(), 1);
        assert_eq!(pack_hints(&pairs, 20).len(), 2);
    }

    #[test]
    fn every_hint_survives_a_narrow_modal() {
        let mut editor = opened("add auth");
        editor.apply_version("add token auth");
        let screen = render(&mut editor, 80, 24);
        for hint in ["tab", "ctrl+s", "ctrl+u", "esc"] {
            assert!(screen.contains(hint), "{hint} missing from\n{screen}");
        }
    }

    /// Narrow terminals and long drafts are where wrap-and-cursor arithmetic
    /// goes out of bounds, so draw both rather than trusting the maths.
    #[test]
    fn awkward_sizes_render_without_panicking() {
        for (w, h) in [(20u16, 8u16), (200, 40), (30, 6)] {
            let mut editor = opened(&"long draft text ".repeat(40));
            editor.handle_key(plain(KeyCode::Tab));
            render(&mut editor, w, h);
            let mut empty = opened("");
            render(&mut empty, w, h);
        }
    }
}
