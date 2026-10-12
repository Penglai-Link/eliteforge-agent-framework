use eliteforge_agent_core::{ToolDescriptor, ToolOperationKind};
use serde_json::json;

#[test]
fn omitted_metadata_is_unknown_and_declarations_survive_transport() {
    let descriptor: ToolDescriptor = serde_json::from_value(json!({
        "id":"plugin.lookup", "call_name":"lookup", "description":"Look up an item",
        "input_schema":{"type":"object"}
    }))
    .unwrap();
    assert_eq!(descriptor.operation_kind, ToolOperationKind::Unknown);
    for kind in [
        ToolOperationKind::Query,
        ToolOperationKind::Command,
        ToolOperationKind::Unknown,
    ] {
        let declared = descriptor.clone().with_operation_kind(kind);
        let transported: ToolDescriptor =
            serde_json::from_value(serde_json::to_value(&declared).unwrap()).unwrap();
        assert_eq!(transported, declared);
    }
    assert_eq!(
        ToolDescriptor::new("id", "name", "description", json!({})).operation_kind,
        ToolOperationKind::Unknown
    );
}
