use eliteforge_agent_official_plugin_a2ui::expand_descriptor;
use serde_json::json;

#[test]
fn parameterless_expand_has_explicit_object_properties() {
    let descriptor = expand_descriptor();
    // Providers inspect the object properties even when the tool takes no arguments.
    assert!(descriptor.input_schema["properties"].is_object());
    let validator = jsonschema::validator_for(&descriptor.input_schema).unwrap();
    assert!(validator.is_valid(&json!({})));
    assert!(!validator.is_valid(&json!({"unexpected": true})));
    assert!(!validator.is_valid(&json!(null)));
}
