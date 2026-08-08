//! Follow-up prompts offered above the input.
//!
//! Stateless like [`super::queue_panel`]: the list lives on `App`, this only
//! knows how tall it is and how to draw it. Deliberately not an `Overlay` — it
//! must never take the keyboard, because the point is that you can read it and
//! carry on typing.

use crate::theme;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

const LABEL: &str = " Next ";
const CHROME_ROWS: u16 = 2;
const ELLIPSIS: &str = "…";
/// Accept keys are ctrl-chorded so a bare digit still types a digit.
const ACCEPT_HINT: &str = "ctrl+";

pub fn height(count: usize) -> u16 {
    if count == 0 {
        0
    } else {
        count as u16 + CHROME_ROWS
    }
}

pub fn view(frame: &mut Frame, area: Rect, prompts: &[String]) {
    if prompts.is_empty() || area.height == 0 {
        return;
    }
    let t = theme::current();
    let content_width = area.width.saturating_sub(2) as usize;

    let lines: Vec<Line> = prompts
        .iter()
        .enumerate()
        .map(|(i, prompt)| {
            let key = format!(" {ACCEPT_HINT}{} ", i + 1);
            let room = content_width.saturating_sub(key.chars().count());
            Line::from(vec![
                Span::styled(key, t.keybind_key),
                Span::styled(truncate(prompt, room), t.tool_dim),
            ])
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(t.panel_border)
        .title(LABEL)
        .title_style(t.panel_title);
    frame.render_widget(Paragraph::new(lines).block(block), area);
}

fn truncate(text: &str, room: usize) -> String {
    let flat = text.replace('\n', " ");
    if flat.chars().count() <= room {
        return flat;
    }
    let keep = room.saturating_sub(ELLIPSIS.chars().count());
    flat.chars().take(keep).collect::<String>() + ELLIPSIS
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case(0, 0 ; "empty_takes_no_room")]
    #[test_case(1, 3 ; "one")]
    #[test_case(3, 5 ; "three")]
    fn height_grows_with_the_list(count: usize, expected: u16) {
        assert_eq!(height(count), expected);
    }

    #[test]
    fn long_prompts_are_truncated_not_wrapped() {
        let out = truncate("a very long suggestion indeed", 10);
        assert_eq!(out.chars().count(), 10);
        assert!(out.ends_with(ELLIPSIS));
    }

    /// Newlines would break the one-row-per-prompt layout.
    #[test]
    fn newlines_are_flattened() {
        assert_eq!(truncate("two\nlines", 20), "two lines");
    }
}
