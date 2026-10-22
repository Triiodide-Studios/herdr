use ratatui::style::Color;

use super::*;
use crate::client::chat_search::{test_support, ChatSearchIndex};

fn chat(pane_id: &str, display_agent: &str, title: &str) -> ClientShellAgent {
    ClientShellAgent {
        pane_id: pane_id.into(),
        workspace_id: "ws_1".into(),
        tab_id: "tab_1".into(),
        name: None,
        display_agent: Some(display_agent.into()),
        agent: Some("claude".into()),
        title: Some(title.into()),
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: AgentStatus::Working,
        state_change_seq: 1,
        state_labels: Vec::new(),
        tokens: Vec::new(),
        focused: false,
    }
}

fn open_chats() -> ClientShellSnapshot {
    let mut snapshot = snapshot();
    for pane_id in ["pane_2", "pane_3"] {
        snapshot.panes.push(ClientShellPane {
            pane_id: pane_id.into(),
            workspace_id: "ws_1".into(),
            tab_id: "tab_1".into(),
            label: None,
            cwd: Some("/repo".into()),
            foreground_cwd: Some("/repo".into()),
            focused: false,
            right_click_passthrough: false,
        });
    }
    snapshot.agents = vec![
        chat("pane_1", "solver", "Diagnose sub bass realism"),
        chat("pane_2", "lumen", "Find why sunlight is blocked"),
        chat("pane_3", "bacteria", "Plan the new enemy"),
    ];
    snapshot
}

fn indexed() -> ChatSearchIndex {
    ChatSearchIndex::with_snapshot(test_support::snapshot(vec![
        test_support::chat(
            "pane_1",
            &["solver"],
            "Diagnose sub bass realism",
            &["subbass", "wave", "shared"],
        ),
        test_support::chat(
            "pane_2",
            &["lumen"],
            "Find why sunlight is blocked",
            &["sunlight", "probe", "shared"],
        ),
        test_support::chat("pane_3", &["bacteria"], "Plan the new enemy", &["enemy"]),
    ]))
}

fn state_with(index: ChatSearchIndex, snapshot: ClientShellSnapshot) -> ClientShellState {
    let config = ClientShellConfig::from_config(&Config::default()).with_chat_search(index);
    let mut state = ClientShellState::new(config);
    state.set_snapshot(Box::new(snapshot));
    state.set_pane_surface(surface());
    state.compose(106, 30).expect("composed frame");
    state
}

fn searching_state(index: ChatSearchIndex) -> ClientShellState {
    state_with(index, open_chats())
}

fn key(state: &mut ClientShellState, code: KeyCode) -> ClientShellInput {
    let mut outcome = ClientShellInput::default();
    state.handle_key(
        crate::input::TerminalKey::new(code, KeyModifiers::NONE),
        &mut outcome,
    );
    outcome
}

fn click(state: &mut ClientShellState, rect: Rect) -> ClientShellInput {
    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: rect.x + 1,
        row: rect.y,
        modifiers: KeyModifiers::empty(),
    })])
}

/// Focuses the box with a click and types `query`, as a window would receive it.
fn search_for(state: &mut ClientShellState, query: &str) {
    let search = state.hits.chat_search;
    click(state, search);
    state.handle_input_bytes(query.as_bytes());
    state.compose(106, 30).expect("frame with results");
}

/// The sidebar's rows, cut to its width, in a window `height` rows tall.
fn sidebar_rows_of(state: &mut ClientShellState, height: u16) -> Vec<String> {
    let frame = state.compose(106, height).expect("frame");
    frame_rows(&frame)
        .iter()
        .map(|row| row.chars().take(26).collect::<String>())
        .collect()
}

fn sidebar_rows(state: &mut ClientShellState) -> Vec<String> {
    sidebar_rows_of(state, 30)
}

fn row_of(rows: &[String], needle: &str) -> usize {
    rows.iter()
        .position(|row| row.contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} not in the sidebar: {rows:#?}"))
}

fn listed_agents(state: &ClientShellState) -> Vec<&str> {
    state
        .hits
        .agents
        .iter()
        .map(|(_, pane_id)| pane_id.as_str())
        .collect()
}

fn focus_target(outcome: &ClientShellInput) -> Option<&str> {
    match &outcome.actions[..] {
        [ClientShellAction::Endpoint { request, .. }] => match &request.method {
            crate::api::schema::Method::PaneFocus(target) => Some(target.pane_id.as_str()),
            _ => None,
        },
        _ => None,
    }
}

#[test]
fn the_search_is_a_section_of_its_own_between_the_spaces_and_the_agents() {
    let mut state = searching_state(indexed());
    let rows = sidebar_rows(&mut state);
    let spaces = row_of(&rows, " spaces");
    let new = row_of(&rows, " new");
    let search = row_of(&rows, " search");
    let agents = row_of(&rows, " agents");
    assert!(spaces < new && new < search && search < agents, "{rows:#?}");

    let input = search + 1;
    assert!(rows[input].contains("find a chat"), "{rows:#?}");
    assert_eq!(usize::from(state.hits.chat_search.y), input);
    assert!(
        !state.hits.workspaces.is_empty(),
        "the spaces list is untouched"
    );
    assert_eq!(listed_agents(&state), ["pane_1", "pane_2", "pane_3"]);
}

#[test]
fn typing_keeps_only_the_matching_chats_in_the_agents_panel() {
    let mut state = searching_state(indexed());
    let search = state.hits.chat_search;
    click(&mut state, search);
    assert!(state.chat_search.is_focused());

    let typed = state.handle_input_bytes(b"sunl");
    assert!(
        typed.requests.is_empty(),
        "typing in the box must not reach the pane: {:?}",
        typed.requests
    );
    let rows = sidebar_rows(&mut state);
    let text = rows.join("\n");
    assert_eq!(state.chat_search.results(), ["pane_2"]);
    assert_eq!(listed_agents(&state), ["pane_2"]);
    assert!(text.contains("lumen"), "{text}");
    assert!(
        !text.contains("solver") && !text.contains("bacteria"),
        "{text}"
    );
    assert!(
        text.contains("1 of 3"),
        "the count replaces the sort label: {text}"
    );
    assert!(
        rows[row_of(&rows, " search") + 1].contains("sunl"),
        "{text}"
    );
    assert!(
        !state.hits.workspaces.is_empty(),
        "the spaces list is untouched"
    );
}

#[test]
fn matches_are_listed_best_first_not_in_the_panels_own_order() {
    let mut state = searching_state(ChatSearchIndex::with_snapshot(test_support::snapshot(
        vec![
            test_support::chat("pane_1", &["a1"], "plain notes", &["shader"]),
            test_support::chat("pane_2", &["b2"], "shader compile", &["shader"]),
        ],
    )));
    search_for(&mut state, "shader compile");
    assert_eq!(state.chat_search.results(), ["pane_2", "pane_1"]);
    assert_eq!(listed_agents(&state), ["pane_2", "pane_1"]);
}

#[test]
fn key_events_type_too_and_edit_the_query() {
    let mut state = searching_state(indexed());
    let search = state.hits.chat_search;
    click(&mut state, search);
    for code in "bass".chars().map(KeyCode::Char) {
        let outcome = key(&mut state, code);
        assert!(outcome.requests.is_empty());
    }
    state.compose(106, 30).expect("frame");
    assert_eq!(state.chat_search.results(), ["pane_1"]);
    key(&mut state, KeyCode::Backspace);
    state.compose(106, 30).expect("frame");
    assert_eq!(
        state.chat_search.results(),
        ["pane_1"],
        "'bas' begins 'bass'"
    );
    key(&mut state, KeyCode::Backspace);
    state.compose(106, 30).expect("frame");
    assert!(
        state.chat_search.results().is_empty(),
        "a two-letter query only matches whole words"
    );
}

#[test]
fn the_terminal_cursor_follows_the_typing() {
    let mut state = searching_state(indexed());
    let search = state.hits.chat_search;
    click(&mut state, search);
    state.handle_input_bytes(b"ab");
    let frame = state.compose(106, 30).expect("frame");
    let cursor = frame.cursor.clone().expect("cursor in the box");
    assert_eq!(cursor.y, search.y);
    assert_eq!(cursor.x, search.x + 3 + 2);
    assert!(cursor.visible);
}

#[test]
fn enter_goes_to_the_highlighted_chat_and_the_full_list_returns() {
    let mut state = searching_state(indexed());
    search_for(&mut state, "sunlight");
    let enter = key(&mut state, KeyCode::Enter);
    assert_eq!(focus_target(&enter), Some("pane_2"));
    assert!(!state.chat_search.is_focused());
    assert!(!state.chat_search.is_active());
    state.compose(106, 30).expect("frame");
    assert_eq!(listed_agents(&state), ["pane_1", "pane_2", "pane_3"]);
}

#[test]
fn arrow_keys_move_the_highlight_between_matches() {
    let mut state = searching_state(indexed());
    search_for(&mut state, "shared");
    assert_eq!(state.chat_search.results(), ["pane_1", "pane_2"]);
    assert_eq!(state.chat_search.selected_pane(), Some("pane_1"));

    key(&mut state, KeyCode::Down);
    assert_eq!(state.chat_search.selected_pane(), Some("pane_2"));
    key(&mut state, KeyCode::Down);
    assert_eq!(
        state.chat_search.selected_pane(),
        Some("pane_2"),
        "stops at the last match"
    );
    key(&mut state, KeyCode::Up);
    assert_eq!(state.chat_search.selected_pane(), Some("pane_1"));

    key(&mut state, KeyCode::Down);
    let enter = key(&mut state, KeyCode::Enter);
    assert_eq!(focus_target(&enter), Some("pane_2"));
}

#[test]
fn the_highlighted_row_is_drawn_with_the_selection_background() {
    let mut state = searching_state(indexed());
    search_for(&mut state, "shared");
    let palette = state.config.palette.clone();
    let selection = sidebar::workspace_selection_background(&palette);
    let background_of = |state: &mut ClientShellState, pane_id: &str| {
        let frame = state.compose(106, 30).expect("frame");
        let rect = state
            .hits
            .agents
            .iter()
            .find(|(_, id)| id == pane_id)
            .map(|(rect, _)| *rect)
            .expect("listed");
        let buffer = frame.to_ratatui_buffer().expect("buffer");
        buffer[(rect.x + 1, rect.y)].bg
    };
    assert_eq!(background_of(&mut state, "pane_1"), selection);
    assert_ne!(background_of(&mut state, "pane_2"), selection);
    key(&mut state, KeyCode::Down);
    assert_ne!(background_of(&mut state, "pane_1"), selection);
    assert_eq!(background_of(&mut state, "pane_2"), selection);
}

fn luminance(color: Color) -> f64 {
    let (r, g, b) = match color {
        Color::Rgb(r, g, b) => (r, g, b),
        Color::Black => (0, 0, 0),
        Color::Gray => (192, 192, 192),
        Color::DarkGray => (128, 128, 128),
        Color::White => (255, 255, 255),
        other => panic!("not a colour the search box should use: {other:?}"),
    };
    let channel = |value: u8| {
        let value = f64::from(value) / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(r) + 0.7152 * channel(g) + 0.0722 * channel(b)
}

fn contrast(a: Color, b: Color) -> f64 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// The cell of the input row `column` columns in, after composing a window with `palette`.
fn input_cell(
    state: &mut ClientShellState,
    palette: Palette,
    column: u16,
) -> ratatui::buffer::Cell {
    state.config.palette = palette;
    let frame = state.compose(106, 30).expect("frame");
    let row = state.hits.chat_search;
    frame.to_ratatui_buffer().expect("buffer")[(row.x + column, row.y)].clone()
}

#[test]
fn what_is_typed_in_the_box_keeps_its_contrast_on_every_theme() {
    const TEXT_COLUMN: u16 = 3;
    let mut problems = Vec::new();
    for name in crate::config::THEME_NAMES {
        let palette = Palette::from_name(name).expect("theme");

        let mut hint_state = searching_state(indexed());
        let hint = input_cell(&mut hint_state, palette.clone(), TEXT_COLUMN);
        assert_eq!(hint.symbol(), "f", "{name}: the placeholder");
        let hint_contrast = contrast(hint.fg, hint.bg);
        if hint_contrast < 4.5 {
            problems.push(format!("{name}: the placeholder is {hint_contrast:.1}:1"));
        }

        let mut typing_state = searching_state(indexed());
        search_for(&mut typing_state, "vhs");
        let typed = input_cell(&mut typing_state, palette, TEXT_COLUMN);
        assert_eq!(typed.symbol(), "v", "{name}: the typed text");
        let typed_contrast = contrast(typed.fg, typed.bg);
        if typed_contrast < 7.0 {
            problems.push(format!("{name}: typed text is {typed_contrast:.1}:1"));
        }
        if typed.bg != hint.bg {
            problems.push(format!("{name}: focus changed the fill"));
        }
    }
    assert!(problems.is_empty(), "{problems:#?}");
}

#[test]
fn the_box_is_darker_than_the_sidebar_on_dark_themes() {
    for name in ["catppuccin", "tokyo-night", "dracula", "nord", "gruvbox"] {
        let palette = Palette::from_name(name).expect("theme");
        let mut state = searching_state(indexed());
        let field = input_cell(&mut state, palette.clone(), 3);
        assert!(
            luminance(field.bg) < luminance(palette.panel_bg),
            "{name}: the box should be an inset, not a lighter patch"
        );
    }
}

#[test]
fn the_prompt_marks_focus() {
    let palette = Palette::catppuccin();
    let mut idle = searching_state(indexed());
    let unfocused = input_cell(&mut idle, palette.clone(), 1);
    let mut focused = searching_state(indexed());
    search_for(&mut focused, "vhs");
    let typing = input_cell(&mut focused, palette.clone(), 1);
    assert_eq!(typing.fg, palette.accent);
    assert_ne!(unfocused.fg, palette.accent);
}

#[test]
fn escape_clears_the_search_and_gives_the_keyboard_back() {
    let mut state = searching_state(indexed());
    search_for(&mut state, "sunlight");
    let outcome = key(&mut state, KeyCode::Esc);
    assert!(outcome.requests.is_empty() && outcome.actions.is_empty());
    assert!(!state.chat_search.is_focused());
    assert!(!state.chat_search.is_active());
    state.compose(106, 30).expect("frame");
    assert_eq!(listed_agents(&state).len(), 3);

    let typed = state.handle_input_bytes(b"x");
    assert_eq!(typed.requests.len(), 1, "typing reaches the pane again");
}

#[test]
fn clicking_a_listed_result_goes_to_that_chat() {
    let mut state = searching_state(indexed());
    search_for(&mut state, "sunlight");
    let (rect, pane_id) = state.hits.agents[0].clone();
    assert_eq!(pane_id, "pane_2");
    let outcome = click(&mut state, rect);
    assert_eq!(focus_target(&outcome), Some("pane_2"));
    assert!(!state.chat_search.is_active());
}

#[test]
fn the_clear_button_ends_the_search() {
    let mut state = searching_state(indexed());
    search_for(&mut state, "sunlight");
    let clear = state.hits.chat_search_clear;
    assert!(!clear.is_empty());
    click(&mut state, clear);
    assert!(!state.chat_search.is_active());
    assert!(!state.chat_search.is_focused());
    state.compose(106, 30).expect("frame");
    assert_eq!(listed_agents(&state).len(), 3);
}

#[test]
fn clicking_elsewhere_gives_the_keyboard_back_but_keeps_the_filter() {
    let mut state = searching_state(indexed());
    search_for(&mut state, "sunlight");
    let pane = state.hits.panes[0].rect;
    click(&mut state, pane);
    assert!(!state.chat_search.is_focused());
    assert!(state.chat_search.is_active());
    state.compose(106, 30).expect("frame");
    assert_eq!(
        listed_agents(&state),
        ["pane_2"],
        "the filter stays until cleared"
    );

    let typed = state.handle_input_bytes(b"x");
    assert_eq!(typed.requests.len(), 1, "typing reaches the pane again");
}

#[test]
fn chats_that_closed_drop_out_of_the_results() {
    let mut state = searching_state(indexed());
    search_for(&mut state, "shared");
    assert_eq!(state.chat_search.results(), ["pane_1", "pane_2"]);

    let mut fewer = open_chats();
    fewer.agents.retain(|agent| agent.pane_id != "pane_2");
    state.set_snapshot(Box::new(fewer));
    state.compose(106, 30).expect("frame");
    assert_eq!(state.chat_search.results(), ["pane_1"]);
    assert_eq!(listed_agents(&state), ["pane_1"]);
}

#[test]
fn the_panel_says_when_nothing_matches_or_the_index_is_not_ready() {
    let mut state = searching_state(indexed());
    search_for(&mut state, "kubernetes");
    let text = sidebar_rows(&mut state).join("\n");
    assert!(text.contains("no matching chats"), "{text}");
    assert!(text.contains("0 of 3"), "{text}");
    assert!(listed_agents(&state).is_empty());

    let mut waiting = searching_state(ChatSearchIndex::inert());
    search_for(&mut waiting, "sunlight");
    let text = sidebar_rows(&mut waiting).join("\n");
    assert!(text.contains("indexing chats"), "the panel says so: {text}");
    assert!(
        text.contains("indexing…"),
        "the section heading says so: {text}"
    );
}

#[test]
fn a_sidebar_too_short_for_the_section_keeps_the_agents_and_the_keyboard_with_the_pane() {
    let mut state = searching_state(indexed());
    let frame = state.compose(106, 14).expect("short frame");
    assert!(
        state.hits.chat_search.is_empty(),
        "{:?}",
        state.hits.chat_search
    );
    assert!(!frame_rows(&frame).iter().any(|row| row.contains(" search")));
    assert!(
        !state.hits.agents.is_empty(),
        "the agents panel keeps the room"
    );
    let typed = state.handle_input_bytes(b"x");
    assert_eq!(typed.requests.len(), 1);
}

#[test]
fn choosing_a_chat_far_down_the_list_scrolls_the_full_list_to_it() {
    let mut crowded = snapshot();
    crowded.agents = (1..=40)
        .map(|number| {
            chat(
                &format!("pane_{number}"),
                &format!("chat-{number}"),
                "title",
            )
        })
        .collect();
    let index = ChatSearchIndex::with_snapshot(test_support::snapshot(vec![test_support::chat(
        "pane_40",
        &["chat-40"],
        "needle haystack",
        &[],
    )]));
    let mut state = state_with(index, crowded);
    assert!(
        !listed_agents(&state).contains(&"pane_40"),
        "the last chat starts off screen"
    );

    search_for(&mut state, "needle");
    assert_eq!(listed_agents(&state), ["pane_40"]);
    let enter = key(&mut state, KeyCode::Enter);
    assert_eq!(focus_target(&enter), Some("pane_40"));
    state.compose(106, 30).expect("frame");
    assert!(
        listed_agents(&state).contains(&"pane_40"),
        "the full list is scrolled to the chat: {:?}",
        listed_agents(&state)
    );
}

#[test]
fn the_spaces_section_can_be_dragged_down_to_four_rows_in_a_tall_window() {
    let mut state = searching_state(indexed());
    state.compose(106, 70).expect("tall frame");
    let spaces_rows = |state: &ClientShellState| {
        state.hits.sidebar_section_divider.y - state.hits.sidebar_divider.y
    };
    assert!(
        spaces_rows(&state) > 20,
        "the default split is half the sidebar"
    );

    let divider = state.hits.sidebar_section_divider;
    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: divider.x + 2,
        row: divider.y,
        modifiers: KeyModifiers::empty(),
    })]);
    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Drag(MouseButton::Left),
        column: divider.x + 2,
        row: 1,
        modifiers: KeyModifiers::empty(),
    })]);
    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        column: divider.x + 2,
        row: 1,
        modifiers: KeyModifiers::empty(),
    })]);
    state.compose(106, 70).expect("dragged frame");
    assert_eq!(spaces_rows(&state), 4);
    let rows = sidebar_rows_of(&mut state, 70);
    assert!(
        rows[row_of(&rows, " new")].contains("menu"),
        "the footer survives: {rows:#?}"
    );
    assert!(
        !state.hits.workspaces.is_empty(),
        "one space still fits above the footer"
    );
}
