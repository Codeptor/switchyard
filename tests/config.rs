use switchyard::config::SwitchyardConfig;

#[test]
fn parses_provider_agnostic_json_configuration() {
    let config = SwitchyardConfig::parse(
        r#"{
            "providers": [{
                "id": "example",
                "base_url": "https://example.test/anthropic",
                "auth": {
                    "type": "header",
                    "header": "Authorization",
                    "env_var": "EXAMPLE_API_KEY",
                    "prefix": "Bearer "
                },
                "models": [{"id": "model[1m]"}]
            }]
        }"#,
    )
    .expect("config");

    assert_eq!(config.providers.len(), 1);
    assert_eq!(config.providers[0].id, "example");
    assert_eq!(config.providers[0].models[0].id, "model[1m]");
    assert!(config.providers[0].validate().is_ok());
}

#[test]
fn omitted_providers_defaults_to_empty_registry() {
    let config = SwitchyardConfig::parse("{}").expect("config");
    assert!(config.providers.is_empty());
}

#[test]
fn malformed_configuration_returns_a_parse_error() {
    let error = SwitchyardConfig::parse("{not-json").expect_err("invalid config");
    assert!(error.to_string().contains("parse"));
}
