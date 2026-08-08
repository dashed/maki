use std::sync::Arc;

use arc_swap::ArcSwapOption;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::text::{Line, Span};

use maki_providers::ModelTier;
use maki_providers::dynamic;
use maki_providers::model_registry;
use maki_providers::provider::ProviderKind;
use maki_providers::{Effort, model::EffortOptions};
use ratatui::widgets::Paragraph;

use crate::components::Overlay;
use crate::components::list_picker::{ListPicker, PickerAction, PickerItem};
use crate::components::modal::Modal;
use crate::theme;

const TITLE: &str = " Models ";
const RECENT_SECTION: &str = "Recent";
const ROLE_SECTION: &str = "Roles";
const PINNED_DETAIL: &str = "pinned";
const AUTO_DETAIL: &str = "auto";
const UNSET_DETAIL: &str = "unset";
/// Strongest first, so the list reads the way people talk about the roles.
const ROLE_TIERS: [ModelTier; 5] = [
    ModelTier::Strong,
    ModelTier::Medium,
    ModelTier::Weak,
    ModelTier::Compaction,
    ModelTier::Suggest,
];
const HELP_TITLE: &str = " Models help ";
const HELP_WIDTH_PERCENT: u16 = 72;
const HELP_MAX_HEIGHT_PERCENT: u16 = 80;
const DETAIL_SEP: &str = " · ";
const EFFORT_KEY: char = 'e';
const HELP_KEY: char = '?';

/// `(heading, key, description)`. Roles and effort answer different questions
/// and the footer alone never said which was which, so spell it out here.
const HELP_ROWS: &[(&str, &str, &str)] = &[
    (
        "Roles",
        "",
        "The rows at the top show which model holds each role right now,",
    ),
    (
        "",
        "",
        "whether you pinned it or maki resolved it, and what it costs.",
    ),
    (
        "Assign",
        "! @ # $ %",
        "Give the selected model a role: strong, medium, weak, compaction,",
    ),
    (
        "",
        "",
        "suggest. The last two are side jobs, not models the agent runs on.",
    ),
    (
        "",
        "",
        "Roles decide which model does what job. Subagents take the first",
    ),
    (
        "",
        "",
        "model in each role. Press the same key again to unassign.",
    ),
    (
        "Effort",
        "ctrl+e",
        "Cycle how hard this model reasons, through the levels it actually",
    ),
    (
        "",
        "",
        "supports. Wraps around to following /thinking again.",
    ),
    (
        "",
        "",
        "OpenRouter publishes these per model; others use a provider default.",
    ),
    ("Pick", "Enter", "Use this model for the session."),
];

fn footer_line() -> Line<'static> {
    let t = theme::current();
    Line::from(vec![
        Span::styled("  Enter", t.keybind_key),
        Span::styled(" select", t.tool_dim),
        Span::styled("  !", t.keybind_key),
        Span::styled(" strong", t.tool_dim),
        Span::styled("  @", t.keybind_key),
        Span::styled(" medium", t.tool_dim),
        Span::styled("  #", t.keybind_key),
        Span::styled(" weak", t.tool_dim),
        Span::styled("  $", t.keybind_key),
        Span::styled(" compaction", t.tool_dim),
        Span::styled("  %", t.keybind_key),
        Span::styled(" suggest", t.tool_dim),
        Span::styled("  ctrl+e", t.keybind_key),
        Span::styled(" effort", t.tool_dim),
        Span::styled("  ?", t.keybind_key),
        Span::styled(" help", t.tool_dim),
    ])
}

/// Walks the model's own levels and falls off the end back to `None`, which
/// means "follow /thinking". A stale level the model no longer lists also lands
/// on `None` rather than sticking.
fn next_effort(current: Option<Effort>, supported: &[Effort]) -> Option<Effort> {
    match current {
        None => supported.first().copied(),
        Some(level) => {
            let idx = supported.iter().position(|&s| s == level)?;
            supported.get(idx + 1).copied()
        }
    }
}

fn tier_for_shortcut(key: KeyEvent) -> Option<ModelTier> {
    let digit = match (key.code, key.modifiers.contains(KeyModifiers::SHIFT)) {
        // Kitty protocol: Shift+digit reported with base key + SHIFT modifier
        (KeyCode::Char(c @ '1'..='5'), true) => c,
        // Legacy terminals: Shift+digit reported as the resulting character
        (KeyCode::Char('!' | '¡'), false) => '1', // US, ES
        (KeyCode::Char('@' | '"' | '™'), false) => '2', // US, UK/DE
        (KeyCode::Char('#' | '§' | '£'), false) => '3', // US, DE, UK
        (KeyCode::Char('$' | '€' | '¤'), false) => '4', // US, EU, Nordic
        (KeyCode::Char('%' | '°'), false) => '5', // US, FR
        _ => return None,
    };
    match digit {
        '1' => Some(ModelTier::Strong),
        '2' => Some(ModelTier::Medium),
        '3' => Some(ModelTier::Weak),
        '4' => Some(ModelTier::Compaction),
        '5' => Some(ModelTier::Suggest),
        _ => None,
    }
}

pub enum ModelPickerAction {
    Consumed,
    Select(String),
    AssignTier(String, ModelTier),
    UnassignTier(String, ModelTier),
    SetEffort(String, Effort),
    ClearEffort(String),
    Close,
}

struct ModelEntry {
    spec: String,
    id: String,
    provider_display: String,
    suffix: Option<String>,
    detail: String,
    override_tiers: Vec<ModelTier>,
    effort: Option<Effort>,
    supported_efforts: Vec<Effort>,
    /// Role summary rows sit above the real list. They carry the spec they
    /// resolve to so the normal keys still work on them, but they must not
    /// steal the "current model" cursor from the model's own row.
    is_role: bool,
}

impl PickerItem for ModelEntry {
    fn label(&self) -> &str {
        &self.id
    }

    fn suffix(&self) -> Option<&str> {
        self.suffix.as_deref()
    }

    fn detail(&self) -> Option<&str> {
        Some(&self.detail)
    }

    fn section(&self) -> Option<&str> {
        Some(self.provider_display.as_str())
    }

    fn is_highlighted(&self) -> bool {
        !self.override_tiers.is_empty()
    }

    fn is_summary(&self) -> bool {
        self.is_role
    }
}

pub struct ModelPicker {
    picker: ListPicker<ModelEntry>,
    models: Arc<ArcSwapOption<Vec<String>>>,
    recents: Vec<String>,
    current_spec: String,
    last_spec_count: usize,
    dirty: bool,
    show_help: bool,
}

impl ModelPicker {
    pub fn new(models: Arc<ArcSwapOption<Vec<String>>>) -> Self {
        Self {
            picker: ListPicker::new().with_footer_builder(footer_line),
            models,
            recents: Vec::new(),
            current_spec: String::new(),
            last_spec_count: 0,
            dirty: false,
            show_help: false,
        }
    }

    pub fn set_recents(&mut self, recents: Vec<String>) {
        self.recents = recents;
        self.dirty = true;
    }

    pub fn open(&mut self, current_spec: &str) {
        self.current_spec = current_spec.to_owned();
        let (entries, idx) = self.load_entries();
        self.picker.open(entries, TITLE);
        self.picker.select(idx);
    }

    fn try_refresh(&mut self) {
        if !self.picker.is_open() {
            return;
        }
        let guard = self.models.load();
        let spec_count = guard.as_deref().map_or(0, Vec::len);
        if spec_count == self.last_spec_count && !self.dirty {
            return;
        }
        drop(guard);
        self.dirty = false;
        let (entries, idx) = self.load_entries();
        self.picker.replace_items(entries);
        self.picker.select(idx);
    }

    fn load_entries(&mut self) -> (Vec<ModelEntry>, usize) {
        let guard = self.models.load();
        let specs = guard.as_deref();
        self.last_spec_count = specs.map_or(0, Vec::len);
        let mut entries: Vec<ModelEntry> = role_entries();
        let recent_specs = self.recents.clone();
        for spec in &recent_specs {
            if let Some(mut e) = parse_model_entry(spec) {
                e.suffix = Some(std::mem::take(&mut e.provider_display));
                e.provider_display = RECENT_SECTION.to_string();
                entries.push(e);
            }
        }
        let mut full: Vec<ModelEntry> = specs
            .map(|s| s.iter().filter_map(|s| parse_model_entry(s)).collect())
            .unwrap_or_default();
        full.sort_by(|a, b| {
            a.provider_display
                .cmp(&b.provider_display)
                .then_with(|| a.id.cmp(&b.id))
        });
        entries.extend(full);
        let idx = entries
            .iter()
            .position(|e| !e.is_role && e.spec == self.current_spec)
            .unwrap_or(0);
        (entries, idx)
    }

    pub fn is_open(&self) -> bool {
        self.picker.is_open()
    }

    pub fn close(&mut self) {
        self.show_help = false;
        self.picker.close();
    }

    pub fn contains(&self, pos: Position) -> bool {
        self.picker.contains(pos)
    }

    pub fn scroll(&mut self, delta: i32) {
        self.picker.scroll(delta);
    }

    pub fn handle_paste(&mut self, text: &str) -> bool {
        self.picker.handle_paste(text)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> ModelPickerAction {
        // Help swallows the next key so `?` never both opens and acts.
        if self.show_help {
            self.show_help = false;
            return ModelPickerAction::Consumed;
        }
        if key.code == KeyCode::Char(HELP_KEY) && !key.modifiers.contains(KeyModifiers::CONTROL) {
            self.show_help = true;
            return ModelPickerAction::Consumed;
        }
        // Ctrl-chorded: the picker's search box takes every bare printable
        // character, so claiming a plain letter makes it untypeable.
        if key.code == KeyCode::Char(EFFORT_KEY)
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && let Some(entry) = self.picker.selected_item()
            && !entry.spec.is_empty()
            && !entry.supported_efforts.is_empty()
        {
            let spec = entry.spec.clone();
            let next = next_effort(entry.effort, &entry.supported_efforts);
            self.dirty = true;
            return match next {
                Some(level) => ModelPickerAction::SetEffort(spec, level),
                None => ModelPickerAction::ClearEffort(spec),
            };
        }
        if let Some(tier) = tier_for_shortcut(key)
            && let Some(entry) = self.picker.selected_item()
            && !entry.spec.is_empty()
        {
            let spec = entry.spec.clone();
            self.dirty = true;
            if entry.override_tiers.contains(&tier) {
                return ModelPickerAction::UnassignTier(spec, tier);
            }
            return ModelPickerAction::AssignTier(spec, tier);
        }
        match self.picker.handle_key(key) {
            PickerAction::Consumed => ModelPickerAction::Consumed,
            // Enter on an unset role has nothing to switch to.
            PickerAction::Select(entry) if entry.spec.is_empty() => ModelPickerAction::Consumed,
            PickerAction::Select(entry) => ModelPickerAction::Select(entry.spec),
            PickerAction::Close => ModelPickerAction::Close,
            PickerAction::Toggle(..) => ModelPickerAction::Consumed,
        }
    }

    pub fn view(&mut self, frame: &mut Frame, area: Rect) -> Rect {
        self.try_refresh();
        let picker_area = self.picker.view(frame, area);
        if self.show_help {
            self.view_help(frame, area);
        }
        picker_area
    }

    fn view_help(&self, frame: &mut Frame, area: Rect) {
        let t = theme::current();
        let key_width = HELP_ROWS
            .iter()
            .map(|(_, key, _)| key.len())
            .max()
            .unwrap_or(0);
        let lines: Vec<Line> = HELP_ROWS
            .iter()
            .map(|(heading, key, desc)| {
                Line::from(vec![
                    Span::styled(format!("  {heading:<8}"), t.keybind_section),
                    Span::styled(format!("{key:<key_width$}  "), t.keybind_key),
                    Span::styled(*desc, t.keybind_desc),
                ])
            })
            .collect();
        let (_, inner) = Modal {
            title: HELP_TITLE,
            width_percent: HELP_WIDTH_PERCENT,
            max_height_percent: HELP_MAX_HEIGHT_PERCENT,
        }
        .render(frame, area, lines.len() as u16);
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

impl Overlay for ModelPicker {
    fn is_open(&self) -> bool {
        self.is_open()
    }

    fn close(&mut self) {
        self.close()
    }
}

fn parse_model_entry(spec: &str) -> Option<ModelEntry> {
    let (provider_str, model_id) = spec.split_once('/')?;

    let provider_display = if let Ok(kind) = provider_str.parse::<ProviderKind>() {
        kind.display_name().to_string()
    } else if let Some(name) = dynamic::display_name(provider_str) {
        name.to_string()
    } else if let Some(info) = maki_providers::catalog_provider_if_available(provider_str) {
        info.display_name.clone()
    } else if let Some(builtin) = maki_config::providers::builtin_provider(provider_str) {
        builtin.display_name.to_string()
    } else {
        let config = maki_config::providers::ProvidersConfig::load();
        config.get(provider_str)?;
        maki_config::providers::resolve_display_name(provider_str, config.get(provider_str))
    };

    let map = model_registry::model_registry().read().unwrap();
    let override_tiers: Vec<ModelTier> = [
        ModelTier::Strong,
        ModelTier::Medium,
        ModelTier::Weak,
        ModelTier::Compaction,
    ]
    .into_iter()
    .filter(|&t| map.has_override(spec, t))
    .collect();
    let override_label = map.override_tier_label(spec);
    let effort = map.effort_for(spec);
    drop(map);
    let tier = override_label.unwrap_or_else(|| match maki_providers::Model::from_spec(spec) {
        Ok(m) => m.tier.to_string(),
        Err(_) => String::new(),
    });
    let supported_efforts = model_registry::effort_options(provider_str, model_id)
        .map(|o: EffortOptions| o.supported)
        .unwrap_or_default();
    let detail = match effort {
        Some(level) => format!("{tier}{DETAIL_SEP}{level}"),
        None => tier,
    };
    let id = model_id.to_string();
    Some(ModelEntry {
        spec: spec.to_string(),
        id,
        provider_display,
        suffix: None,
        detail,
        override_tiers,
        effort,
        supported_efforts,
        is_role: false,
    })
}

/// Four rows naming which model plays each role. Without these you had to hunt
/// one highlighted row out of hundreds to learn what `!`/`@`/`#`/`$` had done.
fn role_entries() -> Vec<ModelEntry> {
    ROLE_TIERS
        .iter()
        .map(|&tier| {
            let (resolved, pinned) = {
                let map = model_registry::model_registry().read().unwrap();
                let resolved = map.spec_for_tier_any(tier);
                let pinned = resolved
                    .as_deref()
                    .is_some_and(|spec| map.has_override(spec, tier));
                (resolved, pinned)
            };

            let Some(spec) = resolved else {
                return role_row(tier, String::new(), None, UNSET_DETAIL.to_string());
            };
            let model_id = spec
                .split_once('/')
                .map_or(spec.clone(), |(_, id)| id.into());
            let status = if pinned { PINNED_DETAIL } else { AUTO_DETAIL };
            let detail = match price_label(&spec) {
                Some(price) => format!("{status}{DETAIL_SEP}{price}"),
                None => status.to_string(),
            };
            role_row(tier, spec, Some(model_id), detail)
        })
        .collect()
}

fn role_row(tier: ModelTier, spec: String, model_id: Option<String>, detail: String) -> ModelEntry {
    ModelEntry {
        spec,
        id: tier.to_string(),
        provider_display: ROLE_SECTION.to_string(),
        suffix: model_id,
        detail,
        override_tiers: Vec::new(),
        effort: None,
        supported_efforts: Vec::new(),
        is_role: true,
    }
}

/// Only when the price is actually known, so an undiscovered model shows
/// nothing rather than a confident `$0.00/$0.00`.
fn price_label(spec: &str) -> Option<String> {
    let pricing = maki_providers::Model::from_spec(spec).ok()?.pricing;
    (pricing.input > 0.0 || pricing.output > 0.0)
        .then(|| format!("${:.2}/${:.2}", pricing.input, pricing.output))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::key;
    use crate::components::keybindings::key as kb;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use test_case::test_case;

    fn test_models() -> Arc<ArcSwapOption<Vec<String>>> {
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
            "zai/glm-5".into(),
        ])));
        models
    }

    #[test_case(key(KeyCode::Esc)          ; "esc_closes")]
    #[test_case(kb::QUIT.to_key_event()    ; "ctrl_c_closes")]
    fn close_keys(cancel_key: KeyEvent) {
        let mut p = ModelPicker::new(test_models());
        p.open("");
        let action = p.handle_key(cancel_key);
        assert!(matches!(action, ModelPickerAction::Close));
        assert!(!p.is_open());
    }

    #[test]
    fn refresh_updates_items_and_preserves_search() {
        let models = Arc::new(ArcSwapOption::empty());
        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
        ])));
        let mut p = ModelPicker::new(models.clone());
        p.open("");

        p.handle_key(key(KeyCode::Char('o')));
        p.handle_key(key(KeyCode::Char('p')));

        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
        ])));
        p.try_refresh();

        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s.contains("opus")),
            "after refresh, 'op' filter should match opus"
        );
    }

    #[test]
    fn open_preselects_current_model() {
        let mut p = ModelPicker::new(test_models());
        p.open("anthropic/claude-opus-4-6-20260101");
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s == "anthropic/claude-opus-4-6-20260101")
        );
    }

    #[test]
    fn parse_model_entry_valid() {
        let entry = parse_model_entry("anthropic/claude-sonnet-4-20250514").unwrap();
        assert_eq!(entry.id, "claude-sonnet-4-20250514");
        assert_eq!(entry.provider_display, "Anthropic");
        assert!(!entry.detail.is_empty());
    }

    #[test]
    fn parse_model_entry_no_slash() {
        assert!(parse_model_entry("no-slash").is_none());
    }

    #[test]
    fn role_rows_cover_every_tier_and_lead_the_list() {
        let entries = role_entries();
        let labels: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(
            labels,
            vec!["strong", "medium", "weak", "compaction", "suggest"]
        );
        assert!(entries.iter().all(|e| e.is_role));
        assert!(entries.iter().all(|e| e.provider_display == ROLE_SECTION));
    }

    /// An unset role has no model behind it, so acting on it would otherwise
    /// assign a tier to the empty spec.
    #[test]
    fn unset_role_row_is_inert() {
        let row = role_row(ModelTier::Weak, String::new(), None, UNSET_DETAIL.into());
        assert!(row.spec.is_empty());
        assert!(row.supported_efforts.is_empty());
        assert_eq!(row.detail, UNSET_DETAIL);
    }

    /// `op` fuzzy-matches `compaction`, so without excluding summary rows from
    /// search a role row would surface every time someone hunted for opus.
    #[test]
    fn role_rows_drop_out_of_search_results() {
        let mut p = ModelPicker::new(test_models());
        p.open("");
        p.handle_key(key(KeyCode::Char('o')));
        p.handle_key(key(KeyCode::Char('p')));

        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s.contains("opus")),
            "search must reach opus, not a role row",
        );
    }

    /// A shortcut on a bare letter makes that letter untypeable in the search
    /// box. `e` was bound to effort cycling and so no model with an e in its
    /// name could be searched for, which is most of them.
    #[test_case('e' ; "the_effort_key")]
    #[test_case('s' ; "an_ordinary_letter")]
    fn plain_letters_reach_the_search_box(letter: char) {
        let mut p = ModelPicker::new(test_models());
        p.open("");
        let before = p.picker.visible_len();

        p.handle_key(key(KeyCode::Char(letter)));

        assert!(
            p.picker.visible_len() < before,
            "typing '{letter}' must filter the list, not be swallowed"
        );
    }

    #[test]
    fn ctrl_e_still_cycles_effort() {
        let mut p = ModelPicker::new(test_models());
        p.open("");
        // Land on a real model row that has effort levels to cycle.
        p.picker
            .select_item_by(|e| !e.is_role && !e.supported_efforts.is_empty());

        let action = p.handle_key(KeyEvent::new(
            KeyCode::Char(EFFORT_KEY),
            KeyModifiers::CONTROL,
        ));
        assert!(matches!(
            action,
            ModelPickerAction::SetEffort(..) | ModelPickerAction::ClearEffort(..)
        ));
    }

    #[test]
    fn role_rows_never_steal_the_current_model_cursor() {
        let mut p = ModelPicker::new(test_models());
        p.open("zai/glm-5");
        let selected = p.picker.selected_item().expect("a row is selected");
        assert!(!selected.is_role);
        assert_eq!(selected.spec, "zai/glm-5");
    }

    #[test_case(key(KeyCode::Char('!')),           ModelTier::Strong     ; "legacy_bang_strong")]
    #[test_case(key(KeyCode::Char('$')),           ModelTier::Compaction ; "legacy_dollar_compaction")]
    #[test_case(key(KeyCode::Char('€')),           ModelTier::Compaction ; "legacy_euro_compaction")]
    #[test_case(KeyEvent::new(KeyCode::Char('1'), KeyModifiers::SHIFT), ModelTier::Strong     ; "kitty_shift_1_strong")]
    #[test_case(KeyEvent::new(KeyCode::Char('4'), KeyModifiers::SHIFT), ModelTier::Compaction ; "kitty_shift_4_compaction")]
    fn tier_shortcut_assigns_and_keeps_picker_open(k: KeyEvent, want: ModelTier) {
        let mut p = ModelPicker::new(test_models());
        p.open("anthropic/claude-sonnet-4-20250514");
        let action = p.handle_key(k);
        assert!(
            matches!(&action, ModelPickerAction::AssignTier(s, t)
                if s == "anthropic/claude-sonnet-4-20250514" && *t == want),
            "expected AssignTier(claude-sonnet, {want:?}), got something else",
        );
        assert!(p.is_open());
    }

    #[test]
    fn refresh_preserves_selection_for_current_model() {
        let models = Arc::new(ArcSwapOption::empty());
        let mut p = ModelPicker::new(models.clone());
        p.open("anthropic/claude-opus-4-6-20260101");

        models.store(Some(Arc::new(vec![
            "anthropic/claude-sonnet-4-20250514".into(),
            "anthropic/claude-opus-4-6-20260101".into(),
            "zai/glm-5".into(),
        ])));
        p.try_refresh();

        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s == "anthropic/claude-opus-4-6-20260101"),
            "after async model arrival, current model should still be selected"
        );
    }

    #[test]
    fn recents_include_current_model_preselected() {
        let models = test_models();
        let mut p = ModelPicker::new(models);
        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("anthropic/claude-opus-4-6-20260101");

        // Role summary rows lead the list, so the first real entry is the one
        // that matters here.
        p.picker.select_item_by(|e| !e.is_role);
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s == "zai/glm-5"),
            "first entry should be the most recent model",
        );

        p.set_recents(vec![
            "zai/glm-5".into(),
            "anthropic/claude-sonnet-4-20250514".into(),
        ]);
        p.open("zai/glm-5");
        let action = p.handle_key(key(KeyCode::Enter));
        assert!(
            matches!(action, ModelPickerAction::Select(ref s) if s == "zai/glm-5"),
            "current model should be preselected within Recent",
        );
    }
}
