use sakala_agent_core::logs::redactor::redact_line;

#[test]
fn sensitive_environment_values_are_redacted() {
    let line = "TOKEN=one PASSWORD=two SECRET=three APP_KEY=four DATABASE_URL=postgres://user:pass@db/app ok=true";
    let redacted = redact_line(line);

    assert_eq!(
        redacted,
        "TOKEN=[REDACTED] PASSWORD=[REDACTED] SECRET=[REDACTED] APP_KEY=[REDACTED] DATABASE_URL=[REDACTED] ok=true"
    );
}

#[test]
fn non_sensitive_output_is_preserved() {
    let line = "deployment build finished successfully";

    assert_eq!(redact_line(line), line);
}
