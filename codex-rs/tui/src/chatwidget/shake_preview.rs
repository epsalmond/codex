//! Confirmation UI for a measured, non-mutating shake preview.

use codex_protocol::ThreadId;
use codex_protocol::num_format::format_with_separators;
use codex_protocol::protocol::ShakeMode;
use codex_protocol::shake::ShakePreview;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget;

use super::ChatWidget;
use super::shake_cost::Billing;
use super::shake_cost::Pricing;
use crate::app_command::AppCommand;
use crate::app_event::AppEvent;
use crate::bottom_pane::SelectionItem;
use crate::bottom_pane::SelectionViewParams;
use crate::bottom_pane::popup_consts::standard_popup_hint_line;
use crate::render::renderable::Renderable;
use codex_protocol::config_types::AutoCompactTokenLimitScope;

struct PreviewHeader {
    title: String,
    description: String,
}

impl PreviewHeader {
    fn lines(&self, width: u16) -> Vec<Line<'_>> {
        let mut lines = vec![Line::from(self.title.as_str().bold())];
        for paragraph in self.description.split('\n') {
            lines.extend(
                textwrap::wrap(paragraph, usize::from(width.max(/*other*/ 1)))
                    .into_iter()
                    .map(Line::from),
            );
        }
        lines
    }
}

impl Renderable for PreviewHeader {
    fn render(&self, area: Rect, buf: &mut Buffer) {
        Widget::render(Paragraph::new(self.lines(area.width)), area, buf);
    }

    fn desired_height(&self, width: u16) -> u16 {
        self.lines(width).len().try_into().unwrap_or(u16::MAX)
    }
}

impl ChatWidget {
    pub(crate) fn show_shake_preview(
        &mut self,
        thread_id: ThreadId,
        mode: ShakeMode,
        preview: ShakePreview,
    ) {
        if self.thread_id != Some(thread_id) {
            return;
        }
        self.handle_shake_completed();
        if let Some(reason) = preview.unavailable_reason {
            self.add_info_message(reason, /*hint*/ None);
            return;
        }
        if preview.tool_outputs == 0
            && preview.text_blocks == 0
            && preview.images == 0
            && preview.thinking_blocks == 0
        {
            self.add_info_message("Nothing to shake.".to_string(), /*hint*/ None);
            return;
        }
        let freed = preview
            .tokens_before
            .saturating_sub(preview.tokens_after)
            .max(/*other*/ 0);
        let percent = if preview.tokens_before > 0 {
            100.0 * freed as f64 / preview.tokens_before as f64
        } else {
            0.0
        };
        let affected = match mode {
            ShakeMode::Elide => format!(
                "{} tool outputs and {} text blocks. Recent ~4k tokens stay protected; removed text is recoverable.",
                preview.tool_outputs, preview.text_blocks,
            ),
            ShakeMode::Images => format!(
                "Images to remove: {}. This mode does not save recovery artifacts.",
                preview.images
            ),
            ShakeMode::Thinking => format!(
                "Thinking blocks to remove: {}. This mode does not save recovery artifacts.",
                preview.thinking_blocks
            ),
        };
        let before = format_with_separators(preview.tokens_before);
        let after = format_with_separators(preview.tokens_after);
        let request_before = self
            .token_info
            .as_ref()
            .map(|info| info.last_token_usage.tokens_in_context_window())
            .filter(|tokens| *tokens >= preview.tokens_before && *tokens > 0);
        let mut advice = Vec::new();
        advice.push(match self.last_response_clock {
            Some(at) => format!(
                "Idle: {}m since last response. Cache expiry is not observed.",
                chrono::Local::now()
                    .signed_duration_since(at)
                    .num_minutes()
                    .max(/*other*/ 0)
            ),
            None => "Idle age unavailable; cache state unknown.".to_string(),
        });
        // A recognized model name on a custom endpoint does not establish its billing rules.
        let known_endpoint = |url: &str| {
            matches!(
                url.trim_end_matches('/'),
                "https://api.openai.com/v1" | "https://chatgpt.com/backend-api/codex"
            )
        };
        let billing = if self.config.model_provider_id != "openai"
            || !self
                .runtime_model_provider_base_url
                .as_deref()
                .is_none_or(known_endpoint)
            || !self
                .config
                .model_provider
                .base_url
                .as_deref()
                .is_none_or(known_endpoint)
        {
            Billing::Unknown
        } else if self.has_codex_backend_auth {
            Billing::Codex
        } else if self
            .runtime_model_provider_base_url
            .as_deref()
            .is_none_or(|url| url.trim_end_matches('/') == "https://api.openai.com/v1")
            && self
                .config
                .model_provider
                .base_url
                .as_deref()
                .is_none_or(|url| url.trim_end_matches('/') == "https://api.openai.com/v1")
        {
            Billing::Api
        } else {
            Billing::Unknown
        };
        if let Some(request_before) = request_before {
            let request_after = request_before.saturating_sub(freed);
            advice.push(format!(
                "Request estimate: {} → {} (latest context minus reduction; overhead may differ).",
                format_with_separators(request_before),
                format_with_separators(request_after)
            ));
            advice.push(match Pricing::for_model(self.current_model(), billing) {
                Some(pricing) => pricing.description(request_before, request_after),
                None => "Cost scenarios unavailable for this model or billing route.".to_string(),
            });
            if self.config.model_auto_compact_token_limit_scope == AutoCompactTokenLimitScope::Total
                && let Some(limit) = self
                    .config
                    .model_auto_compact_token_limit
                    .filter(|limit| *limit > 0)
            {
                advice.push(format!("Configured compact budget: {}. Estimated headroom {} → {} tokens (model may compact earlier).",
                    format_with_separators(limit),
                    format_with_separators(limit.saturating_sub(request_before).max(/*other*/ 0)),
                    format_with_separators(limit.saturating_sub(request_after).max(/*other*/ 0))));
            } else {
                advice.push("Auto-compact headroom unavailable for the active budget.".to_string());
            }
        } else {
            advice.push("Full-request estimate unavailable; conversation counts alone cannot establish pricing or compact headroom.".to_string());
        }
        advice
            .push("Favor shake when it delays compaction, not simply before /compact.".to_string());
        let advice = advice.join("\n");
        let freed = format_with_separators(freed);
        let subtitle = format!(
            "Estimated conversation: {before} → {after} tokens. Frees ~{freed} ({percent:.0}%).\n{affected}\n\n{advice}\n\nConversation excludes instructions/tools. Costs exclude recovery and output; not plan-allowance predictions.",
        );
        self.bottom_pane.show_selection_view(SelectionViewParams {
            header: Box::new(PreviewHeader {
                title: format!("Shake preview ({})", mode.as_str()),
                description: subtitle,
            }),
            footer_hint: Some(standard_popup_hint_line()),
            items: vec![
                SelectionItem {
                    name: "Shake".to_string(),
                    actions: vec![Box::new(move |tx| {
                        tx.send(AppEvent::SubmitThreadOp {
                            thread_id,
                            op: AppCommand::Shake {
                                mode,
                                expected_fingerprint: preview.fingerprint.clone(),
                            },
                        });
                    })],
                    dismiss_on_select: true,
                    ..Default::default()
                },
                SelectionItem {
                    name: "Cancel".to_string(),
                    dismiss_on_select: true,
                    ..Default::default()
                },
            ],
            ..Default::default()
        });
        self.request_redraw();
    }
}
