#![allow(dead_code)]
#[path = "../src/stall.rs"]
mod stall;
#[path = "../src/stall_settings.rs"]
mod stall_settings;

use pretty_assertions::assert_eq;
use stall::Assessment;
use stall::FinalKind;
use stall::StallDetector;
use stall::Suspicion;
use stall::TurnObservation;
use stall::digest;
use std::num::NonZeroU32;

fn turn(text: &str) -> TurnObservation {
    TurnObservation {
        automatic: true,
        complete: true,
        final_kind: FinalKind::from_text(text, &stall_settings::StallSettings::default()),
        ..Default::default()
    }
}
#[allow(clippy::expect_used)]
fn threshold() -> NonZeroU32 {
    NonZeroU32::new(/*n*/ 3).expect("positive test threshold")
}

#[test]
fn empty_and_literal_waits_require_three_distinct_automatic_turns() {
    for text in [
        "",
        " \n",
        "I’M STILL WAITING for the existing result.",
        "NO NEW EVIDENCE.",
    ] {
        let mut detector = StallDetector::default();
        for index in 1..=3 {
            let expected = Assessment::Suspected {
                reason: if text.trim().is_empty() {
                    Suspicion::EmptyFinal
                } else {
                    Suspicion::WaitingFinal
                },
                streak: index,
                threshold_reached: index == 3,
            };
            assert_eq!(
                detector.observe(&index.to_string(), turn(text), threshold()),
                expected
            );
            assert_eq!(
                detector.observe(&index.to_string(), turn(""), threshold()),
                expected
            );
        }
    }
}

#[test]
fn baseline_is_not_a_repetition_at_any_candidate_threshold() {
    for threshold in
        [3, 2, 4, 5].map(|value| NonZeroU32::new(value).expect("positive candidate threshold"))
    {
        let mut detector = StallDetector::default();
        for index in 0..=threshold.get() {
            let observation = TurnObservation {
                has_tools: true,
                actions: Some(digest("same action and outcome")),
                ..turn("waiting")
            };
            assert_eq!(
                detector.observe(&index.to_string(), observation, threshold),
                if index == 0 {
                    Assessment::NotSuspected
                } else {
                    Assessment::Suspected {
                        reason: Suspicion::RepeatedActions,
                        streak: index,
                        threshold_reached: index == threshold.get(),
                    }
                }
            );
        }
    }
}

#[test]
fn substantive_output_breaks_repetition_even_with_stable_tools_or_waiting_signoff() {
    for reset in ["final", "commentary"] {
        let mut detector = StallDetector::default();
        for index in 0..4 {
            let observation = TurnObservation {
                activity: reset == "commentary",
                has_tools: reset == "final",
                actions: (reset == "final").then(|| digest("unchanged test command and result")),
                ..turn(if reset == "final" {
                    "Here is the next completed section of the requested analysis."
                } else {
                    "Waiting for the remaining result."
                })
            };
            assert_eq!(
                detector.observe(&index.to_string(), observation, threshold()),
                Assessment::NotSuspected
            );
        }
        assert_eq!(
            detector.observe("next-wait", turn("waiting"), threshold()),
            Assessment::Suspected {
                reason: Suspicion::WaitingFinal,
                streak: 1,
                threshold_reached: false,
            }
        );
    }
}

#[test]
fn new_user_input_results_manual_turns_and_uncertainty_reset_the_streak() {
    for reset in ["missing", "user", "result", "manual"] {
        let mut detector = StallDetector::default();
        detector.observe("initial", turn(""), threshold());
        let mut observation = turn("");
        match reset {
            "missing" => observation.complete = false,
            "user" => observation.fresh_input = true,
            "result" => observation.external_result = Some(digest("new evidence")),
            "manual" => observation.automatic = false,
            _ => unreachable!(),
        }
        assert_eq!(
            detector.observe("reset", observation, threshold()),
            if reset == "missing" {
                Assessment::Unclassified
            } else {
                Assessment::NotSuspected
            }
        );
        assert_eq!(
            detector.observe("after", turn(""), threshold()),
            Assessment::Suspected {
                reason: Suspicion::EmptyFinal,
                streak: 1,
                threshold_reached: false
            }
        );
    }
}

#[test]
fn repeated_external_content_does_not_reset_but_new_content_does() {
    let mut detector = StallDetector::default();
    for (index, expected) in [
        Assessment::NotSuspected,
        Assessment::Suspected {
            reason: Suspicion::WaitingFinal,
            streak: 1,
            threshold_reached: false,
        },
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            detector.observe(
                &index.to_string(),
                TurnObservation {
                    external_result: Some(digest("unchanged result")),
                    ..turn("waiting")
                },
                threshold()
            ),
            expected
        );
    }
    assert_eq!(
        detector.observe(
            "new",
            TurnObservation {
                external_result: Some(digest("new result")),
                ..turn("waiting")
            },
            threshold()
        ),
        Assessment::NotSuspected
    );
}

#[test]
fn waiting_is_bounded_literal_matching_not_a_tool_name_heuristic() {
    let mut detector = StallDetector::default();
    for (index, observation) in [
        turn(&format!("waiting {}", "x".repeat(512))),
        TurnObservation {
            has_tools: true,
            ..turn("waiting")
        },
        turn("Completed source correction and verification."),
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(
            detector.observe(&index.to_string(), observation, threshold()),
            Assessment::NotSuspected
        );
    }
    assert_eq!(
        detector.observe(
            "custom",
            TurnObservation {
                final_kind: FinalKind::from_text(
                    "WORK IN PROGRESS",
                    &stall_settings::StallSettings {
                        prefixes: vec!["work in progress".to_owned()],
                        ..Default::default()
                    }
                ),
                ..turn("")
            },
            threshold()
        ),
        Assessment::Suspected {
            reason: Suspicion::WaitingFinal,
            streak: 1,
            threshold_reached: false
        }
    );
}
