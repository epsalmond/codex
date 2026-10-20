//! Rows and details for the tree-wide `/ps` view.

use crate::app_event::AppEvent;
use crate::app_event::ProcessTreeEntry;
use crate::bottom_pane::SelectionDescriptionLayout;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionRowDisplay;
use crate::bottom_pane::SelectionSecondaryAction;
use crate::bottom_pane::SelectionViewParams;
use crate::render::renderable::Renderable;
use crate::status_indicator_widget::fmt_elapsed_compact;
use crossterm::event::KeyCode;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

pub(super) const PROCESS_TREE_VIEW_ID: &str = "process-tree";
/// Output lines kept per terminal for the details pane and the history cell.
const MAX_OUTPUT_LINES: usize = 12;
const MAX_DETAIL_OUTPUT_LINES: usize = 6;

/// Keeps the newest non-empty output lines as plain text: escape sequences removed, and only the
/// last carriage-return segment of progress-style lines.
pub(super) fn recent_output_lines(output: &str) -> Vec<String> {
    let lines: Vec<_> = output
        .lines()
        .filter_map(|line| {
            let line = line.trim_end().rsplit('\r').next().unwrap_or_default();
            let plain: String = codex_ansi_escape::ansi_escape_line(line)
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
                .chars()
                .filter(|ch| !ch.is_control() || *ch == '\t')
                .collect();
            let plain = plain.trim_end().to_string();
            (!plain.is_empty()).then_some(plain)
        })
        .collect();
    lines[lines.len().saturating_sub(MAX_OUTPUT_LINES)..].to_vec()
}

fn elapsed(started_at: Option<i64>, now: i64) -> String {
    started_at
        .map(|started_at| {
            fmt_elapsed_compact(u64::try_from(now.saturating_sub(started_at)).unwrap_or(0))
        })
        .unwrap_or_else(|| "?".to_string())
}

/// Builds the picker; `now` is Unix seconds used for elapsed times.
pub(super) fn process_tree_view_params(
    entries: &[ProcessTreeEntry],
    now: i64,
) -> SelectionViewParams {
    let items = entries
        .iter()
        .map(|entry| {
            let show = entry.clone();
            let stop = entry.clone();
            SelectionItem {
                selection_key: Some(format!("{}:{}", entry.thread_id, entry.process_id)),
                name: entry.owner.clone(),
                category_tag: Some(
                    if entry.wakes_owner {
                        "wakes owner"
                    } else {
                        "no wake"
                    }
                    .to_string(),
                ),
                description: Some(format!(
                    "{} · {}",
                    elapsed(entry.started_at, now),
                    entry.command
                )),
                details: Some(Box::new(ProcessOutputDetails(entry.recent_output.clone()))),
                actions: vec![Box::new(move |tx| {
                    tx.send(AppEvent::ShowBackgroundTerminalOutput(Box::new(
                        show.clone(),
                    )));
                })],
                secondary_action: Some(SelectionSecondaryAction {
                    keys: vec![
                        crate::key_hint::plain(KeyCode::Char('x')),
                        crate::key_hint::shift(KeyCode::Char('x')),
                    ],
                    action: Box::new(move |tx| {
                        tx.send(AppEvent::StopBackgroundTerminal(Box::new(stop.clone())));
                    }),
                    footer_hint: footer_hint(),
                }),
                dismiss_on_select: true,
                search_value: Some(format!("{} {}", entry.owner, entry.command)),
                ..Default::default()
            }
        })
        .collect();
    SelectionViewParams {
        view_id: Some(PROCESS_TREE_VIEW_ID),
        title: Some("Background terminals".to_string()),
        subtitle: Some(
            "Root and subagents. Exits marked \"wakes owner\" start that agent's turn.".to_string(),
        ),
        footer_hint: Some(footer_hint()),
        items,
        is_searchable: true,
        search_activation: crate::bottom_pane::SelectionSearchActivation::Slash,
        row_display: SelectionRowDisplay::SingleLine,
        search_placeholder: Some("Search background terminals".to_string()),
        description_layout: SelectionDescriptionLayout::Columns,
        ..SelectionViewParams::picker()
    }
}

fn footer_hint() -> Line<'static> {
    vec![
        crate::key_hint::plain(KeyCode::Enter).into(),
        " show output · ".into(),
        crate::key_hint::plain(KeyCode::Char('x')).into(),
        " stop · ".into(),
        crate::key_hint::plain(KeyCode::Char('/')).into(),
        " search · ".into(),
        crate::key_hint::plain(KeyCode::Esc).into(),
        " back".into(),
    ]
    .into()
}

/// The highlighted terminal's newest output lines.
struct ProcessOutputDetails(Vec<String>);

impl ProcessOutputDetails {
    fn lines(&self) -> Vec<Line<'static>> {
        if self.0.is_empty() {
            return vec!["No recent output.".dim().italic().into()];
        }
        self.0[self.0.len().saturating_sub(MAX_DETAIL_OUTPUT_LINES)..]
            .iter()
            .map(|line| vec!["↳ ".dim(), line.clone().into()].into())
            .collect()
    }
}

impl Renderable for ProcessOutputDetails {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        Renderable::render(&Paragraph::new(self.lines()), area, buf);
    }

    fn desired_height(&self, _width: u16) -> u16 {
        self.lines().len().try_into().unwrap_or(u16::MAX)
    }
}
