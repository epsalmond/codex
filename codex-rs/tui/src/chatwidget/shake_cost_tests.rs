use super::*;
use pretty_assertions::assert_eq;

#[test]
fn cold_and_warm_scenarios_include_threshold_and_billing() {
    for (model, billing, cold_percent, warm_percent, requests) in [
        ("gpt-6-astra", Billing::Codex, 33, 567, 18),
        ("gpt-5.6-sol", Billing::Codex, 67, 233, 5),
        ("gpt-5.6-terra", Billing::Codex, 67, 233, 5),
        ("gpt-5.6-luna", Billing::Codex, 67, 233, 5),
        ("gpt-6-astra", Billing::Api, 67, 317, 6),
    ] {
        let cost = Pricing::for_model(model, billing)
            .unwrap()
            .scenarios(/*before*/ 300_000, /*after*/ 200_000);
        assert_eq!(
            (
                cost.cold_saving_percent.round() as i64,
                cost.warm_next_change_percent.round() as i64,
                cost.warm_break_even_requests,
            ),
            (cold_percent, warm_percent, Some(requests)),
        );
    }
}

#[test]
fn exact_threshold_no_reduction_and_unknown_models() {
    let pricing = Pricing::for_model("gpt-5.6-sol", Billing::Codex).unwrap();
    assert_eq!(
        [272_000, 272_001].map(|before| {
            pricing
                .scenarios(before, /*after*/ 136_000)
                .cold_saving_percent
                .round() as i64
        }),
        [50, 75],
    );
    assert_eq!(
        pricing
            .scenarios(/*before*/ 200_000, /*after*/ 200_000)
            .warm_break_even_requests,
        None,
    );
    for model in ["custom", "gpt-6-astra-custom", "gpt-5.6-sol-next"] {
        assert!(Pricing::for_model(model, Billing::Codex).is_none());
    }
    assert!(Pricing::for_model("gpt-6-astra", Billing::Unknown).is_none());
}

#[test]
fn large_reduction_can_pay_back_on_first_warm_request() {
    let pricing = Pricing::for_model("gpt-6-astra", Billing::Codex).unwrap();
    assert_eq!(
        pricing.scenarios(/*before*/ 200_000, /*after*/ 10_000),
        CostScenarios {
            cold_saving_percent: 95.0,
            warm_next_change_percent: -50.0,
            warm_break_even_requests: Some(1),
        },
    );
}

#[test]
fn turns_clause_is_omitted_without_turn_pace_history() {
    let pace = RequestPace::default();
    assert_eq!(pace.turns_for(/*requests*/ 5), None);
    assert_eq!(
        payback_line(Some(5), &pace, /*expired_cache_ttl*/ None),
        "Pays back after ~5 requests."
    );
}

#[test]
fn turn_pace_converts_requests_to_turns() {
    let mut pace = RequestPace::default();
    // Two completed turns: 3 requests, then 2 requests -> 2.5 requests per turn.
    for (turn_requests, start_total) in [(3, 0), (2, 3_000)] {
        pace.start_turn(/*started_live*/ true);
        for request in 1..=turn_requests {
            pace.observe_total_tokens(start_total + request * 1_000, /*turn_running*/ true);
        }
        pace.complete_turn();
    }
    assert_eq!(pace.turns_for(/*requests*/ 5), Some(2));
    assert_eq!(pace.turns_for(/*requests*/ 6), Some(3));
    assert_eq!(pace.turns_for(/*requests*/ 1), Some(1));
    assert_eq!(
        payback_line(Some(5), &pace, /*expired_cache_ttl*/ None),
        "Pays back after ~5 requests ≈ 2 turns at this session's pace."
    );
    assert_eq!(
        payback_line(Some(2), &pace, /*expired_cache_ttl*/ None),
        "Pays back after ~2 requests ≈ 1 turn at this session's pace."
    );
    assert_eq!(
        payback_line(Some(1), &pace, /*expired_cache_ttl*/ None),
        "Pays back on the first request."
    );
    assert_eq!(
        payback_line(
            /*break_even*/ None, &pace, /*expired_cache_ttl*/ None
        ),
        "No payback at unchanged sizes."
    );
}

#[test]
fn turn_pace_ignores_non_request_usage_updates() {
    let mut pace = RequestPace::default();
    // A resumed thread reports its restored totals before any turn runs.
    pace.observe_total_tokens(/*total*/ 50_000, /*turn_running*/ false);
    pace.start_turn(/*started_live*/ true);
    // Rate-limit refreshes and context re-estimates repeat the same total.
    pace.observe_total_tokens(/*total*/ 50_000, /*turn_running*/ true);
    pace.observe_total_tokens(/*total*/ 60_000, /*turn_running*/ true);
    pace.observe_total_tokens(/*total*/ 60_000, /*turn_running*/ true);
    pace.complete_turn();
    assert_eq!(pace.turns_for(/*requests*/ 4), Some(4));

    // Interrupted turns never complete, and turns with no observed request
    // do not dilute the pace.
    pace.start_turn(/*started_live*/ true);
    pace.observe_total_tokens(/*total*/ 70_000, /*turn_running*/ true);
    pace.start_turn(/*started_live*/ true);
    pace.complete_turn();
    assert_eq!(pace.turns_for(/*requests*/ 4), Some(4));
}

#[test]
fn expired_cache_pays_back_immediately() {
    let pace = RequestPace::default();
    assert_eq!(
        payback_line(Some(5), &pace, Some(Duration::from_secs(3_600))),
        "Pays back immediately: idle exceeds the 60m prompt-cache TTL; cache likely cold."
    );
    assert_eq!(
        payback_line(Some(5), &pace, Some(Duration::from_secs(90))),
        "Pays back immediately: idle exceeds the 90s prompt-cache TTL; cache likely cold."
    );
    assert_eq!(
        payback_line(
            /*break_even*/ None,
            &pace,
            Some(Duration::from_secs(3_600))
        ),
        "No payback at unchanged sizes."
    );
}
