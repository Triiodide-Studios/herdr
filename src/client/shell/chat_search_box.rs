//! The "search" section of the sidebar, between the spaces and the agents. While its box holds a
//! query, the agents panel lists only the chats that match it, best match first.

use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEventKind, KeyModifiers};
use ratatui::style::Color;

use super::agent_sidebar::AgentFilter;
use super::render::{display_width, put_right_text, put_text};
use super::*;
use crate::client::chat_search::{ChatHit, ChatSearchIndex, ChatText, IndexSnapshot, IndexStatus};

/// A chat the search found: the machine it runs on and its pane.
pub(super) type ChatTarget = (ClientEndpointId, String);

const PROMPT: &str = " / ";
const PLACEHOLDER: &str = "find a chat";
const CLEAR_BUTTON: &str = " × ";
const HEADING: &str = " search";
/// Divider, heading and the box.
const SECTION_ROWS: u16 = 3;
/// What the agents panel keeps below the section: its divider, heading and spacer, and five rows
/// of chats. A window too short for that has no search section and the agents get all the room.
const MIN_AGENT_PANEL_ROWS: u16 = 8;

pub(super) struct ChatSearchState {
    index: ChatSearchIndex,
    snapshot: Arc<IndexSnapshot>,
    editor: TextEditor,
    focused: bool,
    /// Position in `results` of the highlighted chat.
    selected: usize,
    /// The index's answer for `ranked_for`: its generation and the query it was asked.
    ranked: Vec<ChatHit>,
    ranked_for: Option<(u64, String)>,
    /// The ranked chats that are still open, best first.
    results: Vec<ChatTarget>,
    /// The highlight moved: the agents panel scrolls to it the next time it is drawn.
    reveal_selected: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ChatSearchKey {
    /// Not a key the box uses; nothing changed.
    Ignored,
    /// The query or the highlight changed.
    Changed,
    /// Go to this chat.
    Activate(ChatTarget),
    /// Leave the search.
    Close,
}

impl ChatSearchState {
    pub(super) fn new(index: ChatSearchIndex) -> Self {
        Self {
            snapshot: index.snapshot(),
            index,
            editor: TextEditor::default(),
            focused: false,
            selected: 0,
            ranked: Vec::new(),
            ranked_for: None,
            results: Vec::new(),
            reveal_selected: false,
        }
    }

    pub(super) fn is_focused(&self) -> bool {
        self.focused
    }

    /// Whether the box holds a query, so the agents panel shows only its results.
    pub(super) fn is_active(&self) -> bool {
        !self.editor.trim().is_empty()
    }

    pub(super) fn focus(&mut self) {
        self.focused = true;
        self.index.request_refresh();
    }

    pub(super) fn blur(&mut self) {
        self.focused = false;
    }

    pub(super) fn clear(&mut self) {
        self.editor.clear();
        self.focused = false;
        self.selected = 0;
        self.ranked.clear();
        self.ranked_for = None;
        self.results.clear();
        self.reveal_selected = false;
    }

    pub(super) fn insert_text(&mut self, text: &str) {
        if self.editor.insert(text) {
            self.selected = 0;
            self.reveal_selected = true;
        }
    }

    pub(super) fn handle_key(&mut self, key: &crate::input::TerminalKey) -> ChatSearchKey {
        if key.kind == KeyEventKind::Release {
            return ChatSearchKey::Ignored;
        }
        let (code, modifiers) = crate::config::normalize_key_combo((key.code, key.modifiers));
        let plain = modifiers.is_empty();
        let control = modifiers == KeyModifiers::CONTROL;
        match code {
            KeyCode::Esc if plain => return ChatSearchKey::Close,
            KeyCode::Enter if plain => {
                return self
                    .results
                    .get(self.selected)
                    .cloned()
                    .map_or(ChatSearchKey::Ignored, ChatSearchKey::Activate);
            }
            KeyCode::Up if plain => return self.move_selection(false),
            KeyCode::BackTab => return self.move_selection(false),
            KeyCode::Down | KeyCode::Tab if plain => return self.move_selection(true),
            KeyCode::Char('p') if control => return self.move_selection(false),
            KeyCode::Char('n') if control => return self.move_selection(true),
            _ => {}
        }
        match self.editor.handle_key(key) {
            Some(true) => {
                self.selected = 0;
                self.reveal_selected = true;
                ChatSearchKey::Changed
            }
            Some(false) => ChatSearchKey::Changed,
            None => ChatSearchKey::Ignored,
        }
    }

    fn move_selection(&mut self, forward: bool) -> ChatSearchKey {
        let last = self.results.len().saturating_sub(1);
        let next = if forward {
            (self.selected + 1).min(last)
        } else {
            self.selected.saturating_sub(1)
        };
        if next == self.selected {
            return ChatSearchKey::Ignored;
        }
        self.selected = next;
        self.reveal_selected = true;
        ChatSearchKey::Changed
    }

    /// Brings the results up to date with the index and with the chats open on the listed
    /// `machines`, and hands the index the chats it does not read itself: those of the saved
    /// machines (and, in a `--remote` client, of the remote machine), by name and title. Cheap
    /// enough for every frame: the index is searched only when its generation or the query changed.
    pub(super) fn refresh(&mut self, machines: &[(&ClientEndpointId, &ClientShellSnapshot)]) {
        let pushed = machines
            .iter()
            .filter(|(endpoint_id, _)| !(endpoint_id.is_local() && self.index.indexes_local()))
            .flat_map(|(endpoint_id, snapshot)| {
                snapshot.agents.iter().map(|agent| ChatText {
                    endpoint_id: (*endpoint_id).clone(),
                    pane_id: agent.pane_id.clone(),
                    names: [&agent.display_agent, &agent.name]
                        .into_iter()
                        .flatten()
                        .cloned()
                        .collect(),
                    title: agent
                        .title
                        .clone()
                        .or_else(|| agent.terminal_title_stripped.clone())
                        .unwrap_or_default(),
                    keywords: Vec::new(),
                })
            })
            .collect();
        self.index.push_chats(pushed);
        self.snapshot = self.index.snapshot();
        let query = self.editor.trim();
        if query.is_empty() {
            self.ranked.clear();
            self.ranked_for = None;
            self.results.clear();
            self.selected = 0;
            return;
        }
        let generation = self.snapshot.generation();
        let current = self
            .ranked_for
            .as_ref()
            .is_some_and(|(searched, asked)| *searched == generation && asked == query);
        if !current {
            self.ranked = self.snapshot.search(query);
            self.ranked_for = Some((generation, query.to_owned()));
        }
        self.results = self
            .ranked
            .iter()
            .filter(|hit| {
                machines.iter().any(|(endpoint_id, snapshot)| {
                    **endpoint_id == hit.endpoint_id
                        && snapshot
                            .agents
                            .iter()
                            .any(|agent| agent.pane_id == hit.pane_id)
                })
            })
            .map(|hit| (hit.endpoint_id.clone(), hit.pane_id.clone()))
            .collect();
        self.selected = self.selected.min(self.results.len().saturating_sub(1));
    }

    /// What the agents panel lists while a query is typed: the matching chats, in rank order,
    /// with the highlighted one marked once the box has the keyboard.
    pub(super) fn agent_filter(&self, open_chats: usize) -> Option<AgentFilter<'_>> {
        self.is_active().then(|| AgentFilter {
            targets: &self.results,
            label: format!("{} of {open_chats}", self.results.len()),
            highlighted: self
                .focused
                .then(|| self.results.get(self.selected))
                .flatten(),
            reveal_highlighted: self.reveal_selected,
            empty_message: match self.snapshot.status() {
                IndexStatus::Starting => " indexing chats…",
                IndexStatus::ModelUnavailable(_) => " no match (search model missing)",
                IndexStatus::Ready => " no matching chats",
            },
        })
    }

    /// The agents panel has drawn the highlight.
    pub(super) fn reveal_done(&mut self) {
        self.reveal_selected = false;
    }

    /// A short note on the search's own state, for the right of the heading. Searching by
    /// letters alone when the model is missing needs no note.
    fn notice(&self) -> Option<&'static str> {
        match self.snapshot.status() {
            IndexStatus::Starting => Some("indexing…"),
            IndexStatus::ModelUnavailable(_) | IndexStatus::Ready => None,
        }
    }

    /// The panes of the results, best first.
    #[cfg(test)]
    pub(super) fn results(&self) -> Vec<&str> {
        self.results
            .iter()
            .map(|(_, pane_id)| pane_id.as_str())
            .collect()
    }

    #[cfg(test)]
    pub(super) fn index(&self) -> &ChatSearchIndex {
        &self.index
    }

    #[cfg(test)]
    pub(super) fn result_targets(&self) -> &[ChatTarget] {
        &self.results
    }

    #[cfg(test)]
    pub(super) fn selected_pane(&self) -> Option<&str> {
        self.results
            .get(self.selected)
            .map(|(_, pane_id)| pane_id.as_str())
    }
}

/// Splits the sidebar's lower part into the search section and the agents panel, or gives the
/// agents panel all of it when there is no room for the section.
pub(super) fn split_detail_area(detail: Rect) -> (Rect, Rect) {
    if detail.height < SECTION_ROWS + MIN_AGENT_PANEL_ROWS || detail.width < 8 {
        return (Rect::default(), detail);
    }
    (
        Rect::new(detail.x, detail.y, detail.width, SECTION_ROWS),
        Rect::new(
            detail.x,
            detail.y + SECTION_ROWS,
            detail.width,
            detail.height - SECTION_ROWS,
        ),
    )
}

pub(super) fn render_section(
    buffer: &mut Buffer,
    area: Rect,
    state: &ChatSearchState,
    config: &ClientShellConfig,
    hits: &mut ShellHitMap,
) {
    if area.is_empty() {
        return;
    }
    let palette = &config.palette;
    put_text(
        buffer,
        area.x,
        area.y,
        area.width,
        &"─".repeat(area.width as usize),
        Style::default().fg(palette.surface_dim),
    );
    let heading = Rect::new(area.x, area.y + 1, area.width, 1);
    put_text(
        buffer,
        heading.x,
        heading.y,
        heading.width,
        HEADING,
        Style::default()
            .fg(palette.overlay0)
            .add_modifier(Modifier::BOLD),
    );
    if let Some(notice) = state.notice() {
        if display_width(notice) + display_width(HEADING) < heading.width {
            put_right_text(
                buffer,
                heading,
                heading.y,
                notice,
                Style::default()
                    .fg(palette.overlay0)
                    .add_modifier(Modifier::DIM),
            );
        }
    }

    let row = Rect::new(area.x, area.y + 2, area.width, 1);
    let field = FieldColors::of(palette);
    let base = Style::default().bg(field.fill).fg(field.text);
    buffer.set_style(row, base);
    hits.chat_search = row;
    put_text(
        buffer,
        row.x,
        row.y,
        row.width,
        PROMPT,
        if state.focused {
            base.fg(palette.accent).add_modifier(Modifier::BOLD)
        } else {
            base.fg(field.hint)
        },
    );
    let clear_width = if state.is_active() {
        display_width(CLEAR_BUTTON).min(row.width)
    } else {
        0
    };
    let prompt_width = display_width(PROMPT);
    let input = Rect::new(
        row.x + prompt_width,
        row.y,
        row.width.saturating_sub(prompt_width + clear_width),
        1,
    );
    if state.editor.is_empty() && !state.focused {
        put_text(
            buffer,
            input.x,
            input.y,
            input.width,
            PLACEHOLDER,
            base.fg(field.hint),
        );
    } else {
        let cursor = super::text_editor::render(buffer, input, &state.editor, base);
        hits.chat_search_cursor = cursor.filter(|_| state.focused);
    }
    if clear_width > 0 {
        hits.chat_search_clear = Rect::new(row.right() - clear_width, row.y, clear_width, 1);
        put_text(
            buffer,
            hits.chat_search_clear.x,
            row.y,
            clear_width,
            CLEAR_BUTTON,
            base.fg(field.hint),
        );
    }
}

/// Typed text on the box: WCAG's AAA level, since terminal text is small.
const TEXT_CONTRAST: f32 = 7.0;
/// The placeholder, prompt and clear button: WCAG's AA level.
const HINT_CONTRAST: f32 = 4.5;

type Rgb = (u8, u8, u8);

/// Colours of the input row. The box is an inset, darker than the sidebar (or lighter, on a light
/// theme), and its text is the theme's own pushed toward black or white until it reads on that
/// fill, so what is typed stays legible whatever the theme's surface colours are. The 16-colour
/// terminal theme has none to derive from and gets the palette's black and white. Focus shows on
/// the prompt, not on the fill.
struct FieldColors {
    fill: Color,
    text: Color,
    hint: Color,
}

impl FieldColors {
    fn of(palette: &Palette) -> Self {
        let Color::Rgb(r, g, b) = palette.panel_bg else {
            // Dark gray (ANSI 8), not gray (ANSI 7): most terminals draw gray close to white,
            // and the placeholder must read as a hint. It still meets AA on the black fill.
            return Self {
                fill: Color::Black,
                text: Color::White,
                hint: Color::DarkGray,
            };
        };
        let light = (299 * u32::from(r) + 587 * u32::from(g) + 114 * u32::from(b)) / 1000 > 128;
        let (pole, inset) = if light {
            ((255, 255, 255), 0.6)
        } else {
            ((0, 0, 0), 0.45)
        };
        let fill = mix((r, g, b), pole, inset);
        let readable = |color: Color, minimum: f32| {
            let ink = if light { (0, 0, 0) } else { (255, 255, 255) };
            let from = match color {
                Color::Rgb(r, g, b) => (r, g, b),
                _ => ink,
            };
            let (r, g, b) = (0..=10)
                .map(|step| mix(from, ink, step as f32 / 10.0))
                .find(|candidate| contrast(*candidate, fill) >= minimum)
                .unwrap_or(ink);
            Color::Rgb(r, g, b)
        };
        Self {
            fill: Color::Rgb(fill.0, fill.1, fill.2),
            text: readable(palette.text, TEXT_CONTRAST),
            hint: readable(palette.subtext0, HINT_CONTRAST),
        }
    }
}

fn mix(from: Rgb, to: Rgb, share: f32) -> Rgb {
    let channel = |from: u8, to: u8| {
        (f32::from(from) + (f32::from(to) - f32::from(from)) * share).round() as u8
    };
    (
        channel(from.0, to.0),
        channel(from.1, to.1),
        channel(from.2, to.2),
    )
}

fn luminance((r, g, b): Rgb) -> f32 {
    let channel = |value: u8| {
        let value = f32::from(value) / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
}

fn contrast(a: Rgb, b: Rgb) -> f32 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

impl ClientShellState {
    pub(super) fn route_chat_search_key(
        &mut self,
        key: &crate::input::TerminalKey,
        outcome: &mut ClientShellInput,
    ) {
        match self.chat_search.handle_key(key) {
            ChatSearchKey::Ignored => {}
            ChatSearchKey::Changed => outcome.repaint = true,
            ChatSearchKey::Activate(target) => self.go_to_searched_chat(target, outcome),
            ChatSearchKey::Close => {
                self.chat_search.clear();
                outcome.repaint = true;
            }
        }
    }

    /// Ends the search, scrolls the full agents list to the chat, and focuses it. With saved
    /// machines listed, the chat's machine is made active first if it is not.
    fn go_to_searched_chat(
        &mut self,
        (endpoint_id, pane_id): ChatTarget,
        outcome: &mut ClientShellInput,
    ) {
        self.chat_search.clear();
        let all_machines =
            super::endpoints::visible_endpoints(&self.endpoints, self.local_only).len() > 1;
        if all_machines || endpoint_id != self.active_endpoint_id {
            self.reveal_endpoint_agent(&endpoint_id, &pane_id, self.hits.agent_body.height);
            self.focus_or_activate(
                endpoint_id,
                ClientEndpointFocusTarget::Pane(pane_id),
                outcome,
            );
            outcome.repaint = true;
            return;
        }
        self.reveal_agent(&pane_id);
        self.push_endpoint_method(
            crate::api::schema::Method::PaneFocus(crate::api::schema::PaneTarget { pane_id }),
            outcome,
        );
        outcome.repaint = true;
    }

    fn reveal_agent(&mut self, pane_id: &str) {
        let Some(snapshot) = self.snapshot.as_deref() else {
            return;
        };
        let rows = super::agent_sidebar::agent_rows(snapshot, &self.config, None);
        let Some(target) = rows.iter().position(|row| row.pane_id == pane_id) else {
            return;
        };
        let heights = rows
            .iter()
            .map(|row| row.rows.len().max(1).min(usize::from(u16::MAX)) as u16)
            .collect::<Vec<_>>();
        let mut gaps = vec![self.config.agents.row_gap; rows.len()];
        if let Some(last) = gaps.last_mut() {
            *last = 0;
        }
        self.agent_scroll = super::scroll::list_scroll_start_to_reveal(
            &heights,
            &gaps,
            self.hits.agent_body.height,
            self.agent_scroll,
            target,
        );
    }

    /// A left click at `point`. Returns whether the search used it; a click anywhere else only
    /// takes the keyboard back from the box. While a query is typed, the agents panel rows are
    /// the results, so a click on one goes to that chat.
    pub(super) fn handle_chat_search_click(
        &mut self,
        point: (u16, u16),
        outcome: &mut ClientShellInput,
    ) -> bool {
        if super::contains(self.hits.chat_search_clear, point) {
            self.chat_search.clear();
            outcome.repaint = true;
            return true;
        }
        if super::contains(self.hits.chat_search, point) {
            self.chat_search.focus();
            outcome.repaint = true;
            return true;
        }
        let chosen = self
            .chat_search
            .is_active()
            .then(|| {
                self.hits
                    .agents
                    .iter()
                    .find(|(rect, _)| super::contains(*rect, point))
                    .map(|(_, pane_id)| (self.active_endpoint_id.clone(), pane_id.clone()))
                    .or_else(|| {
                        self.hits
                            .endpoint_agents
                            .iter()
                            .find(|(rect, _, _)| super::contains(*rect, point))
                            .map(|(_, endpoint_id, pane_id)| (endpoint_id.clone(), pane_id.clone()))
                    })
            })
            .flatten();
        if let Some(target) = chosen {
            self.go_to_searched_chat(target, outcome);
            return true;
        }
        if self.chat_search.is_focused() {
            self.chat_search.blur();
            outcome.repaint = true;
        }
        false
    }

    #[cfg(test)]
    pub(super) fn chat_search_index_for_test(&self) -> &ChatSearchIndex {
        self.chat_search.index()
    }

    /// Keys and text go to the box while it has the keyboard, in the terminal mode only.
    pub(super) fn chat_search_has_keyboard(&self) -> bool {
        self.overlay.is_none()
            && self.mode == ClientShellMode::Terminal
            && self.chat_search.is_focused()
    }
}
