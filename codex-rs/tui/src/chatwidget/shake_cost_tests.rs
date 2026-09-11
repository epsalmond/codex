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
