//! Picker for `/thinking`, so the options are visible instead of remembered.
//!
//! Only the levels the current model actually accepts are listed, which is why
//! this asks the registry rather than showing all six every time.

use crossterm::event::KeyEvent;
use ratatui::Frame;
use ratatui::layout::Rect;

use maki_providers::{Effort, Model, ThinkingConfig, model_registry, resolved_effort};

use crate::components::Overlay;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};

const TITLE: &str = " Thinking ";
const MAX_VISIBLE: u16 = 12;
const MODE_SECTION: &str = "Mode";
const EFFORT_SECTION: &str = "Effort";
const OFF_LABEL: &str = "off";
const ADAPTIVE_LABEL: &str = "adaptive";
const OFF_DETAIL: &str = "no reasoning";
const ADAPTIVE_DETAIL: &str = "let the model decide";
const CURRENT_DETAIL: &str = "current";
const MODEL_DEFAULT_DETAIL: &str = "model default";
const BUDGET_HINT: &str = "type /thinking <tokens> for a budget";

pub enum ThinkingPickerAction {
    Consumed,
    Select(ThinkingConfig),
    Close,
}

struct ThinkingEntry {
    label: String,
    detail: String,
    section: String,
    config: ThinkingConfig,
    current: bool,
}

impl PickerItem for ThinkingEntry {
    fn label(&self) -> &str {
        &self.label
    }

    fn detail(&self) -> Option<&str> {
        (!self.detail.is_empty()).then_some(self.detail.as_str())
    }

    fn section(&self) -> Option<&str> {
        Some(&self.section)
    }

    fn is_highlighted(&self) -> bool {
        self.current
    }
}

pub struct ThinkingPicker {
    picker: ListPicker<ThinkingEntry>,
}

impl ThinkingPicker {
    pub fn new() -> Self {
        Self {
            picker: ListPicker::new().with_max_visible(MAX_VISIBLE),
        }
    }

    pub fn open(&mut self, model: &Model, current: ThinkingConfig) {
        let entries = build_entries(model, current);
        let idx = entries.iter().position(|e| e.current).unwrap_or(0);
        self.picker.open(entries, TITLE);
        self.picker.select(idx);
    }

    pub fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub fn close(&mut self) {
        self.picker.close();
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        self.picker.handle_paste(text)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ThinkingPickerAction {
        match self.picker.handle_key(key) {
            PickerAction::Consumed => ThinkingPickerAction::Consumed,
            PickerAction::Select(entry) => ThinkingPickerAction::Select(entry.config),
            PickerAction::Close => ThinkingPickerAction::Close,
            PickerAction::Toggle(..) => ThinkingPickerAction::Consumed,
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        self.picker.view(frame, area)
    }
}

impl Overlay for ThinkingPicker {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close()
    }
}

/// Adaptive earns a detail line naming the level it resolves to, because
/// "adaptive" alone never told anyone what would actually be sent.
fn build_entries(model: &Model, current: ThinkingConfig) -> Vec<ThinkingEntry> {
    let options = model_registry::effort_options(&model.provider, &model.id);
    let overridden = model_registry::model_registry()
        .read()
        .unwrap()
        .effort_for(&model.spec());

    let adaptive_detail = match resolved_effort(model, ThinkingConfig::Adaptive) {
        Some(level) => format!("{ADAPTIVE_DETAIL} ({level})"),
        None => ADAPTIVE_DETAIL.to_string(),
    };

    let mut entries = vec![
        ThinkingEntry {
            label: OFF_LABEL.to_string(),
            detail: OFF_DETAIL.to_string(),
            section: MODE_SECTION.to_string(),
            config: ThinkingConfig::Off,
            current: matches!(current, ThinkingConfig::Off),
        },
        ThinkingEntry {
            label: ADAPTIVE_LABEL.to_string(),
            detail: adaptive_detail,
            section: MODE_SECTION.to_string(),
            config: ThinkingConfig::Adaptive,
            current: matches!(current, ThinkingConfig::Adaptive),
        },
    ];

    let Some(options) = options else {
        // Budget-only providers have no levels worth listing, so say how to set
        // one rather than showing an empty section.
        entries[1].detail = format!("{}, {BUDGET_HINT}", entries[1].detail);
        return entries;
    };

    // A per-model pick beats whatever is chosen here, so name it instead of
    // letting the choice look like it did nothing.
    let section = match overridden {
        Some(level) => format!("{EFFORT_SECTION} (this model is pinned to {level} by /effort)"),
        None => EFFORT_SECTION.to_string(),
    };

    entries.extend(options.supported.iter().map(|&level| ThinkingEntry {
        label: level.to_string(),
        detail: effort_detail(level, options.default, current),
        section: section.clone(),
        config: ThinkingConfig::Effort(level),
        current: matches!(current, ThinkingConfig::Effort(e) if e == level),
    }));
    entries
}

fn effort_detail(level: Effort, default: Option<Effort>, current: ThinkingConfig) -> String {
    let mut parts = Vec::new();
    if matches!(current, ThinkingConfig::Effort(e) if e == level) {
        parts.push(CURRENT_DETAIL);
    }
    if default == Some(level) {
        parts.push(MODEL_DEFAULT_DETAIL);
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::key;
    use crossterm::event::KeyCode;
    use test_case::test_case;

    const BUDGET_PROVIDER: maki_providers::provider::ProviderKind =
        maki_providers::provider::ProviderKind::Google;
    const EFFORT_PROVIDER: maki_providers::provider::ProviderKind =
        maki_providers::provider::ProviderKind::OpenAi;

    fn model(provider: maki_providers::provider::ProviderKind) -> Model {
        Model {
            id: "thinking-picker-probe".into(),
            provider: std::sync::Arc::<str>::from(provider.to_string()),
            tier: maki_providers::ModelTier::Medium,
            family: provider.family(),
            supports_tool_examples_override: None,
            supports_thinking_override: Some(true),
            supports_vision_override: None,
            pricing: maki_providers::ModelPricing::default(),
            max_output_tokens: Some(8192),
            context_window: 200_000,
        }
    }

    #[test]
    fn lists_only_levels_the_model_accepts() {
        let entries = build_entries(&model(EFFORT_PROVIDER), ThinkingConfig::Adaptive);
        let labels: Vec<&str> = entries.iter().map(|e| e.label.as_str()).collect();
        // OpenAI speaks the STANDARD dialect, which stops at high.
        assert_eq!(
            labels,
            vec![
                OFF_LABEL,
                ADAPTIVE_LABEL,
                "minimal",
                "low",
                "medium",
                "high"
            ]
        );
    }

    #[test]
    fn budget_providers_get_no_effort_rows() {
        let entries = build_entries(&model(BUDGET_PROVIDER), ThinkingConfig::Adaptive);
        let labels: Vec<&str> = entries.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(labels, vec![OFF_LABEL, ADAPTIVE_LABEL]);
        assert!(entries[1].detail.contains(BUDGET_HINT));
    }

    #[test_case(ThinkingConfig::Off,                  OFF_LABEL      ; "off")]
    #[test_case(ThinkingConfig::Adaptive,             ADAPTIVE_LABEL ; "adaptive")]
    #[test_case(ThinkingConfig::Effort(Effort::Low),  "low"          ; "effort")]
    fn opens_on_the_current_setting(current: ThinkingConfig, expected: &str) {
        let entries = build_entries(&model(EFFORT_PROVIDER), current);
        let marked: Vec<&str> = entries
            .iter()
            .filter(|e| e.current)
            .map(|e| e.label.as_str())
            .collect();
        assert_eq!(marked, vec![expected]);
    }

    /// A budget has no row of its own, so nothing should claim to be current.
    #[test]
    fn budget_setting_marks_nothing_current() {
        let entries = build_entries(&model(EFFORT_PROVIDER), ThinkingConfig::Budget(8192));
        assert!(entries.iter().all(|e| !e.current));
    }

    #[test]
    fn enter_selects_the_config() {
        let mut p = ThinkingPicker::new();
        p.open(&model(EFFORT_PROVIDER), ThinkingConfig::Off);
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(matches!(
            action,
            ThinkingPickerAction::Select(ThinkingConfig::Off)
        ));
    }

    #[test]
    fn esc_closes_without_selecting() {
        let mut p = ThinkingPicker::new();
        p.open(&model(EFFORT_PROVIDER), ThinkingConfig::Off);
        assert!(matches!(
            p.handle_key(key(KeyCode::Esc)),
            ThinkingPickerAction::Close
        ));
        assert!(!p.is_open());
    }
}
