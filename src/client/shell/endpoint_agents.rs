use super::render::put_text;
use super::*;

pub(super) fn render_collapsed(
    buffer: &mut Buffer,
    area: Rect,
    endpoints: &[ClientShellEndpoint],
    active_endpoint_id: &ClientEndpointId,
    config: &ClientShellConfig,
    hits: &mut ShellHitMap,
) {
    let rows = agent_rows(endpoints, active_endpoint_id, config);
    for (index, row) in rows.into_iter().take(area.height as usize).enumerate() {
        let rect = Rect::new(area.x, area.y + index as u16, area.width, 1);
        if row.agent.focused {
            buffer.set_style(rect, Style::default().bg(config.palette.active_row_bg));
        }
        let initial = row.machine_label.chars().next().unwrap_or('?');
        put_text(
            buffer,
            rect.x,
            rect.y,
            rect.width,
            &format!(
                "{initial}{}",
                status_icon(row.agent.status, config.status_indicators)
            ),
            Style::default()
                .fg(if row.stale {
                    config.palette.overlay0
                } else {
                    status_color(row.agent.status, &config.palette)
                })
                .add_modifier(if row.stale {
                    Modifier::DIM
                } else {
                    Modifier::empty()
                }),
        );
        hits.endpoint_agents
            .push((rect, row.endpoint_id, row.agent.pane_id));
    }
}

/// The all-machines agents panel. While the chat search holds a query (`filter`), it lists only
/// the matching chats, on every listed machine.
pub(super) fn render_expanded(
    buffer: &mut Buffer,
    area: Rect,
    agent_view_label: Option<&str>,
    endpoints: &[ClientShellEndpoint],
    active_endpoint_id: &ClientEndpointId,
    config: &ClientShellConfig,
    agent_scroll: &mut usize,
    filter: Option<&super::agent_sidebar::AgentFilter<'_>>,
    hits: &mut ShellHitMap,
) {
    let label = filter
        .map(|filter| filter.label.as_str())
        .or(agent_view_label);
    if !super::agent_sidebar::render_agent_panel_header(buffer, area, label, config, hits) {
        return;
    }
    let rows = match filter {
        Some(filter) => search_rows(endpoints, filter, config),
        None => agent_rows(endpoints, active_endpoint_id, config),
    };
    let reveal = filter
        .filter(|filter| filter.reveal_highlighted)
        .and_then(|_| rows.iter().position(|row| row.agent.selected));
    let empty_message = match filter {
        Some(filter) => Some(filter.empty_message),
        None => agent_view_label.map(|_| " no matching agents"),
    };
    super::agent_sidebar::render_agent_list(
        buffer,
        area,
        &rows,
        empty_message,
        config,
        agent_scroll,
        hits,
        reveal,
        |row| row.agent.rows.len(),
        |buffer, rect, row, hits| {
            super::agent_sidebar::render_agent_row(buffer, rect, &row.agent, config);
            if row.stale {
                buffer.set_style(
                    rect,
                    Style::default()
                        .fg(config.palette.overlay0)
                        .add_modifier(Modifier::DIM),
                );
            }
            hits.endpoint_agents
                .push((rect, row.endpoint_id.clone(), row.agent.pane_id.clone()));
        },
    );
}

impl ClientShellState {
    pub(super) fn reveal_endpoint_agent(
        &mut self,
        endpoint_id: &ClientEndpointId,
        pane_id: &str,
        body_height: u16,
    ) {
        if body_height == 0 {
            return;
        }
        let rows = agent_rows(
            super::endpoints::visible_endpoints(&self.endpoints, self.local_only),
            &self.active_endpoint_id,
            &self.config,
        );
        let Some(target) = rows
            .iter()
            .position(|row| &row.endpoint_id == endpoint_id && row.agent.pane_id == pane_id)
        else {
            return;
        };
        let heights = rows
            .iter()
            .map(|row| row.agent.rows.len().max(1).min(u16::MAX as usize) as u16)
            .collect::<Vec<_>>();
        let mut gaps = vec![self.config.agents.row_gap; rows.len()];
        if let Some(last) = gaps.last_mut() {
            *last = 0;
        }
        self.agent_scroll = super::scroll::list_scroll_start_to_reveal(
            &heights,
            &gaps,
            body_height,
            self.agent_scroll,
            target,
        );
    }
}

struct EndpointAgentRow {
    endpoint_id: ClientEndpointId,
    machine_label: String,
    stale: bool,
    agent: super::agent_sidebar::AgentRow,
}

fn agent_rows(
    endpoints: &[ClientShellEndpoint],
    active_endpoint_id: &ClientEndpointId,
    config: &ClientShellConfig,
) -> Vec<EndpointAgentRow> {
    let mut rendered_rows = endpoints
        .iter()
        .filter_map(|endpoint| {
            endpoint.snapshot.as_deref().map(|snapshot| {
                snapshot
                    .agents
                    .iter()
                    .filter_map(|agent| {
                        super::agent_sidebar::agent_row(
                            snapshot,
                            &agent.pane_id,
                            config,
                            Some(&endpoint.label),
                        )
                    })
                    .map(|agent| ((endpoint.endpoint_id.clone(), agent.pane_id.clone()), agent))
                    .collect::<Vec<_>>()
            })
        })
        .flatten()
        .collect::<HashMap<_, _>>();

    super::aggregate_navigation::aggregate_agent_rows(endpoints, active_endpoint_id, config)
        .into_iter()
        .filter_map(|row| {
            let key = (row.endpoint.endpoint_id.clone(), row.agent.pane_id.clone());
            let mut agent = rendered_rows.remove(&key)?;
            agent.focused &= row.endpoint.endpoint_id == active_endpoint_id;
            agent.pinned = config
                .pinned_agents_for(row.endpoint.endpoint_id)
                .contains(&agent.pane_id);
            Some(EndpointAgentRow {
                endpoint_id: row.endpoint.endpoint_id.clone(),
                machine_label: row.endpoint.label.to_owned(),
                stale: row.endpoint.stale(),
                agent,
            })
        })
        .collect()
}

/// The chat search's results, in rank order, on whichever listed machine each runs.
fn search_rows(
    endpoints: &[ClientShellEndpoint],
    filter: &super::agent_sidebar::AgentFilter<'_>,
    config: &ClientShellConfig,
) -> Vec<EndpointAgentRow> {
    filter
        .targets
        .iter()
        .filter_map(|target| {
            let (endpoint_id, pane_id) = target;
            let endpoint = endpoints
                .iter()
                .find(|endpoint| &endpoint.endpoint_id == endpoint_id)?;
            let snapshot = endpoint.snapshot.as_deref()?;
            let mut agent =
                super::agent_sidebar::agent_row(snapshot, pane_id, config, Some(&endpoint.label))?;
            agent.pinned = config
                .pinned_agents_for(&endpoint.endpoint_id)
                .contains(pane_id);
            agent.selected = filter.highlighted == Some(target);
            Some(EndpointAgentRow {
                endpoint_id: endpoint.endpoint_id.clone(),
                machine_label: endpoint.label.clone(),
                stale: endpoint.status != ClientEndpointStatus::Online,
                agent,
            })
        })
        .collect()
}
