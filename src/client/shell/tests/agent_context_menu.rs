use super::*;

fn agent(pane_id: &str, display_agent: &str, state_change_seq: u64) -> ClientShellAgent {
    ClientShellAgent {
        pane_id: pane_id.into(),
        workspace_id: "ws_1".into(),
        tab_id: "tab_1".into(),
        name: None,
        display_agent: Some(display_agent.into()),
        agent: Some("claude".into()),
        title: None,
        terminal_title: None,
        terminal_title_stripped: None,
        agent_status: AgentStatus::Working,
        state_change_seq,
        state_labels: Vec::new(),
        tokens: Vec::new(),
        focused: false,
    }
}

fn two_agent_state() -> ClientShellState {
    let mut snapshot = snapshot();
    snapshot.panes.push(ClientShellPane {
        pane_id: "pane_2".into(),
        workspace_id: "ws_1".into(),
        tab_id: "tab_1".into(),
        label: None,
        cwd: Some("/repo".into()),
        foreground_cwd: Some("/repo".into()),
        focused: false,
        right_click_passthrough: false,
    });
    snapshot.agents = vec![agent("pane_1", "first", 2), agent("pane_2", "second", 1)];
    let mut state = ClientShellState::new(ClientShellConfig::from_config(&Config::default()));
    state.set_snapshot(Box::new(snapshot));
    state.set_pane_surface(surface());
    state.compose(106, 30).expect("composed frame");
    state
}

fn click(state: &mut ClientShellState, button: MouseButton, rect: Rect) -> ClientShellInput {
    state.handle_raw_events(vec![RawInputEvent::Mouse(MouseEvent {
        kind: MouseEventKind::Down(button),
        column: rect.x + 1,
        row: rect.y,
        modifiers: KeyModifiers::empty(),
    })])
}

fn agent_rect(state: &ClientShellState, pane_id: &str) -> Rect {
    state
        .hits
        .agents
        .iter()
        .find(|(_, id)| id == pane_id)
        .map(|(rect, _)| *rect)
        .expect("agent row hit")
}

fn right_click_agent(state: &mut ClientShellState, pane_id: &str) -> ClientShellInput {
    let rect = agent_rect(state, pane_id);
    click(state, MouseButton::Right, rect)
}

fn menu_labels(state: &ClientShellState) -> Vec<&'static str> {
    match state.overlay.as_ref() {
        Some(ClientShellOverlay::ContextMenu(menu)) => {
            menu.items().iter().map(|item| item.label).collect()
        }
        _ => panic!("agent context menu"),
    }
}

fn choose(state: &mut ClientShellState, label: &str) -> ClientShellInput {
    let index = menu_labels(state)
        .iter()
        .position(|item| *item == label)
        .unwrap_or_else(|| panic!("menu item {label}"));
    state.compose(106, 30).expect("context menu frame");
    let row = state.hits.context_menu_rows[index].0;
    click(state, MouseButton::Left, row)
}

fn ordered(state: &ClientShellState) -> Vec<String> {
    agent_sidebar::ordered_agent_pane_ids(
        state.snapshot.as_deref().expect("snapshot"),
        state.config.agent_panel_sort,
        &state.config.pinned_agents,
    )
}

#[test]
fn right_clicking_an_agent_row_pins_it_to_the_top() {
    let mut state = two_agent_state();
    let unpinned = ordered(&state);
    assert_eq!(unpinned.last().map(String::as_str), Some("pane_2"));

    let open = right_click_agent(&mut state, "pane_2");
    assert!(open.actions.is_empty());
    assert_eq!(menu_labels(&state), ["Pin", "Rename", "Close"]);

    choose(&mut state, "Pin");
    assert!(state.overlay.is_none());
    assert_eq!(state.config.pinned_agents, ["pane_2"]);
    assert_eq!(ordered(&state).first().map(String::as_str), Some("pane_2"));

    let frame = state.compose(106, 30).expect("pinned frame");
    let rows = frame_rows(&frame);
    let pinned_row = agent_rect(&state, "pane_2");
    assert_eq!(pinned_row.y, state.hits.agent_body.y);
    assert!(rows[pinned_row.y as usize].contains("📌"));
    let row_text = rows[pinned_row.y as usize..pinned_row.bottom() as usize].join("\n");
    assert!(row_text.contains("second"), "{row_text}");

    right_click_agent(&mut state, "pane_2");
    assert_eq!(menu_labels(&state)[0], "Unpin");
    choose(&mut state, "Unpin");
    assert!(state.config.pinned_agents.is_empty());
    assert_eq!(ordered(&state), unpinned);
}

#[test]
fn agent_rename_starts_from_the_shown_name_and_a_manual_name_wins() {
    let mut state = two_agent_state();
    right_click_agent(&mut state, "pane_2");
    choose(&mut state, "Rename");
    match state.overlay.as_ref() {
        Some(ClientShellOverlay::Rename(ClientRenameOverlay {
            input,
            target: ClientRenameTarget::Pane { pane_id },
            ..
        })) => {
            assert_eq!(pane_id, "pane_2");
            assert_eq!(input.as_str(), "second");
        }
        _ => panic!("pane rename overlay"),
    }

    let mut renamed = state.snapshot.as_deref().expect("snapshot").clone();
    renamed.panes[1].label = Some("mine".into());
    state.set_snapshot(Box::new(renamed));
    let snapshot = state.snapshot.as_deref().expect("snapshot");
    let row = agent_sidebar::agent_row(snapshot, "pane_2", &state.config, None).expect("row");
    assert!(row
        .rows
        .iter()
        .flatten()
        .any(|token| token.kind == crate::ui::ResolvedTokenKind::Agent("mine".into())));

    state.overlay = None;
    right_click_agent(&mut state, "pane_2");
    assert_eq!(
        menu_labels(&state),
        ["Pin", "Rename", "Clear name", "Close"]
    );
}

#[test]
fn closing_a_pinned_agent_closes_its_pane_and_drops_the_pin() {
    let mut state = two_agent_state();
    state.config.pinned_agents = vec!["pane_2".into()];
    right_click_agent(&mut state, "pane_1");
    choose(&mut state, "Pin");
    assert_eq!(state.config.pinned_agents, ["pane_2", "pane_1"]);

    right_click_agent(&mut state, "pane_2");
    let close = choose(&mut state, "Close");
    assert_eq!(state.config.pinned_agents, ["pane_1"]);
    let [ClientShellAction::Endpoint { request, .. }] = &close.actions[..] else {
        panic!("close should use the endpoint API");
    };
    assert!(matches!(
        &request.method,
        crate::api::schema::Method::PaneClose(target) if target.pane_id == "pane_2"
    ));
}

#[test]
fn right_clicking_open_agent_panel_space_starts_a_chat_tab() {
    let mut state = two_agent_state();
    let body = state.hits.agent_body;
    let last_row = state
        .hits
        .agents
        .iter()
        .map(|(rect, _)| rect.bottom())
        .max()
        .expect("agent rows");
    assert!(
        last_row < body.bottom(),
        "the panel needs open space below the rows"
    );
    let open_space = Rect::new(body.x, last_row, body.width, 1);

    let open = click(&mut state, MouseButton::Right, open_space);
    assert!(open.actions.is_empty());
    assert_eq!(menu_labels(&state), ["New chat"]);

    let new_chat = choose(&mut state, "New chat");
    assert!(state.overlay.is_none());
    assert!(matches!(
        &new_chat.actions[..],
        [ClientShellAction::StartChatTab { workspace_id }] if workspace_id == "ws_1"
    ));
}
