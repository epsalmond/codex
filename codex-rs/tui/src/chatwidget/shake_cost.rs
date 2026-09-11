//! Input-only shake scenarios, not a prediction of cache hits or plan allowance.
//!
//! Rates checked 2026-09-10:
//! https://help.openai.com/en/articles/20001415
//! https://developers.openai.com/api/docs/pricing
//! https://developers.openai.com/api/docs/guides/prompt-caching
//! Normalize to the model's standard input rate: cached reads cost 0.1x;
//! Codex has no write surcharge, while the API write scenario costs 1.25x.

use codex_protocol::num_format::format_with_separators;

const LONG_CONTEXT_THRESHOLD: i64 = 272_000;

#[derive(Clone, Copy)]
pub(super) enum Billing {
    Codex,
    Api,
    Unknown,
}

pub(super) struct Pricing {
    billing: Billing,
    long_context_multiplier: f64,
}

#[derive(Debug, PartialEq)]
pub(super) struct CostScenarios {
    pub cold_saving_percent: f64,
    pub warm_next_change_percent: f64,
    pub warm_break_even_requests: Option<u64>,
}

impl Pricing {
    pub(super) fn for_model(model: &str, billing: Billing) -> Option<Self> {
        let long_context_multiplier = match (model, billing) {
            ("gpt-6-astra", Billing::Codex) => 1.0,
            (
                "gpt-6-astra" | "gpt-5.6-sol" | "gpt-5.6-terra" | "gpt-5.6-luna",
                Billing::Codex | Billing::Api,
            ) => 2.0,
            _ => return None,
        };
        Some(Self {
            billing,
            long_context_multiplier,
        })
    }

    pub(super) fn scenarios(&self, before: i64, after: i64) -> CostScenarios {
        let weighted = |tokens| {
            tokens as f64
                * if tokens > LONG_CONTEXT_THRESHOLD {
                    self.long_context_multiplier
                } else {
                    1.0
                }
        };
        let before = weighted(before);
        let after = weighted(after);
        let write_rate = match self.billing {
            Billing::Api => 1.25,
            Billing::Codex | Billing::Unknown => 1.0,
        };
        let cold_before = before * write_rate;
        let cold_after = after * write_rate;
        let read_before = before * 0.1;
        let read_after = after * 0.1;
        let recurring_saving = read_before - read_after;
        // Across N identical requests: N * read_before vs
        // cold_after + (N - 1) * read_after. No growth or recovery reads.
        let warm_break_even_requests = (recurring_saving > 0.0).then(|| {
            ((cold_after - read_after) / recurring_saving)
                .ceil()
                .max(/*other*/ 1.0) as u64
        });
        CostScenarios {
            cold_saving_percent: if cold_before > 0.0 {
                100.0 * (cold_before - cold_after) / cold_before
            } else {
                0.0
            },
            warm_next_change_percent: if read_before > 0.0 {
                100.0 * (cold_after - read_before) / read_before
            } else {
                0.0
            },
            warm_break_even_requests,
        }
    }

    pub(super) fn description(&self, before: i64, after: i64) -> String {
        let costs = self.scenarios(before, after);
        let cold = costs.cold_saving_percent;
        let warm = costs.warm_next_change_percent.abs();
        let direction = if costs.warm_next_change_percent > 0.0 {
            "more"
        } else {
            "less"
        };
        let payback = match costs.warm_break_even_requests {
            Some(1) => "break-even on request 1".to_string(),
            Some(n) => format!("break-even by request ~{n}"),
            None => "no payback at unchanged sizes".to_string(),
        };
        let billing = match self.billing {
            Billing::Codex => "Codex",
            Billing::Api => "API (1.25x cache writes)",
            Billing::Unknown => "Unknown",
        };
        let threshold = if self.long_context_multiplier == 1.0 {
            "Astra has no >272k surcharge.".to_string()
        } else if before > LONG_CONTEXT_THRESHOLD && after <= LONG_CONTEXT_THRESHOLD {
            format!(
                "May leave >272k tier (2x input, 1.5x output); ~{} tokens margin.",
                format_with_separators(LONG_CONTEXT_THRESHOLD - after)
            )
        } else if after > LONG_CONTEXT_THRESHOLD {
            format!(
                "Still above 272k; needs ~{} more tokens removed to leave 2x input / 1.5x output tier.",
                format_with_separators(after - LONG_CONTEXT_THRESHOLD)
            )
        } else {
            format!(
                "Below 272k in this estimate; ~{} tokens margin before 2x input / 1.5x output tier.",
                format_with_separators(LONG_CONTEXT_THRESHOLD - after)
            )
        };
        format!(
            "{billing}: {threshold}\nCold cache: next input ~{cold:.0}% cheaper.\nWarm cache, full invalidation: next input ~{warm:.0}% {direction}; {payback}.\nAssumes standard rates, fixed sizes and subsequent cache hits."
        )
    }
}

#[cfg(test)]
#[path = "shake_cost_tests.rs"]
mod tests;
