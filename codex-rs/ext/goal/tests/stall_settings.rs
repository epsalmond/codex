#![allow(dead_code)]

#[path = "../src/stall.rs"]
mod stall;
#[path = "../src/stall_settings.rs"]
mod stall_settings;

use codex_core::config::GoalsToml;
use pretty_assertions::assert_eq;
use stall::FinalKind;
use stall_settings::parse_settings;
use stall_settings::settings;

#[test]
fn packaged_rules_and_replacements_drive_the_shared_matcher() -> anyhow::Result<()> {
    let classify = |text: &str, replacement: Option<&[String]>| {
        let config = settings(&GoalsToml {
            stall_waiting_prefixes: replacement.map(<[String]>::to_vec),
            ..Default::default()
        })
        .map_err(anyhow::Error::msg)?;
        Ok::<_, anyhow::Error>(FinalKind::from_text(text, &config))
    };
    assert_eq!(
        classify("I’M   STILL WAITING for results", None)?,
        FinalKind::Waiting
    );
    let replacement = vec!["  HOLDING   FOR REVIEW  ".to_owned()];
    assert_eq!(
        classify("Holding for review until tomorrow", Some(&replacement))?,
        FinalKind::Waiting
    );
    assert_eq!(
        classify("Waiting for results", Some(&replacement))?,
        FinalKind::Other
    );
    assert_eq!(
        classify("Waiting for results", Some(&[]))?,
        FinalKind::Other
    );
    assert_eq!(classify("", Some(&[]))?, FinalKind::Empty);
    Ok(())
}

#[test]
fn malformed_packaged_rules_and_unbounded_replacements_are_rejected() {
    for source in [
        "{",
        r#"{"version":2,"goals":{"stall_waiting_prefixes":[]}}"#,
        r#"{"version":1,"goals":{}}"#,
        r#"{"version":1,"goals":{"stall_waiting_prefixes":[" "]}}"#,
    ] {
        assert!(parse_settings(source, &GoalsToml::default()).is_err());
        assert!(
            parse_settings(
                source,
                &GoalsToml {
                    stall_waiting_prefixes: Some(Vec::new()),
                    stall_waiting_text_max_chars: Some(std::num::NonZeroU32::MIN),
                    stall_unlinked_timer_recognition: Some(false),
                    ..Default::default()
                }
            )
            .is_err()
        );
    }
    for replacement in [
        vec![" ".to_owned()],
        vec!["x".repeat(65)],
        vec!["x".to_owned(); 33],
    ] {
        assert!(
            settings(&GoalsToml {
                stall_waiting_prefixes: Some(replacement),
                ..Default::default()
            })
            .is_err()
        );
    }
    // Unicode limits count characters, rather than encoded bytes.
    assert!(
        settings(&GoalsToml {
            stall_waiting_prefixes: Some(vec!["é".repeat(64)]),
            ..Default::default()
        })
        .is_ok()
    );
}

#[test]
fn waiting_length_policy_uses_normalized_unicode_characters() -> anyhow::Result<()> {
    let config = settings(&GoalsToml {
        stall_waiting_prefixes: Some(vec!["é".to_owned()]),
        stall_waiting_text_max_chars: std::num::NonZeroU32::new(1),
        ..Default::default()
    })
    .map_err(anyhow::Error::msg)?;
    assert_eq!(FinalKind::from_text("  É  ", &config), FinalKind::Waiting);
    assert_eq!(FinalKind::from_text("ÉÉ", &config), FinalKind::Other);
    for limit in [0, 513] {
        assert!(
            serde_json::from_value::<GoalsToml>(
                serde_json::json!({"stall_waiting_text_max_chars":limit})
            )
            .is_err()
        );
    }
    Ok(())
}
