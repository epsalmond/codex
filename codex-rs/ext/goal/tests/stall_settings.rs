#![allow(dead_code)]

#[path = "../src/stall.rs"]
mod stall;
#[path = "../src/stall_settings.rs"]
mod stall_settings;

use pretty_assertions::assert_eq;
use stall::FinalKind;
use stall_settings::parse_waiting_prefixes;
use stall_settings::waiting_prefixes;

#[test]
fn packaged_rules_and_replacements_drive_the_shared_matcher() -> anyhow::Result<()> {
    let classify = |text: &str, replacement: Option<&[String]>| {
        let prefixes = waiting_prefixes(replacement).map_err(anyhow::Error::msg)?;
        Ok::<_, anyhow::Error>(FinalKind::from_text(
            text,
            &prefixes.iter().map(String::as_str).collect::<Vec<_>>(),
        ))
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
        assert!(parse_waiting_prefixes(source, None).is_err());
    }
    for replacement in [
        vec![" ".to_owned()],
        vec!["x".repeat(65)],
        vec!["x".to_owned(); 33],
    ] {
        assert!(waiting_prefixes(Some(&replacement)).is_err());
    }
    // Unicode limits count characters, and explicit replacement needs no
    // packaged fallback, even if that build-time data were malformed.
    assert_eq!(
        parse_waiting_prefixes("{", Some(&["é".repeat(64)])),
        Ok(vec!["é".repeat(64)])
    );
}
