use std::collections::HashMap;

use sakala_agent_core::{AgentConfig, AgentMode};

#[test]
fn config_defaults_to_safe_local_mode() {
    let config = AgentConfig::from_values(&HashMap::new()).expect("default config should load");

    assert_eq!(config.mode, AgentMode::Local);
    assert_eq!(config.agent_id, "local-agent-01");
    assert_eq!(config.api_url, "http://localhost:8000");
    assert_eq!(config.poll_interval_seconds, 3);
    assert_eq!(config.heartbeat_interval_seconds, 10);
    assert_eq!(config.runtime_network, "sakala-runtime");
    assert!(config.agent_token.is_none());
}

#[test]
fn connected_mode_rejects_placeholder_token() {
    let values = HashMap::from([
        ("SAKALA_AGENT_MODE".to_owned(), "connected".to_owned()),
        ("SAKALA_AGENT_TOKEN".to_owned(), "change-me".to_owned()),
    ]);

    assert!(AgentConfig::from_values(&values).is_err());
}
