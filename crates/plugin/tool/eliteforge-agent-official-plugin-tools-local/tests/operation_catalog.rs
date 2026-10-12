use eliteforge_agent_core::ToolOperationKind;
use eliteforge_agent_official_plugin_tools_local::local_tool_catalog;

#[test]
fn local_catalog_separates_queries_from_mutations_and_arbitrary_commands() {
    let catalog = local_tool_catalog();
    assert!(
        catalog
            .iter()
            .all(|tool| tool.operation_kind != ToolOperationKind::Unknown)
    );
    for (name, kind) in [
        ("fs_read_text", ToolOperationKind::Query),
        ("fs_write_text", ToolOperationKind::Command),
        ("fs_apply_unified_patch", ToolOperationKind::Command),
        ("system_exec", ToolOperationKind::Command),
        ("terminal_read", ToolOperationKind::Query),
        ("terminal_start", ToolOperationKind::Command),
    ] {
        assert_eq!(
            catalog
                .iter()
                .find(|tool| tool.call_name == name)
                .unwrap()
                .operation_kind,
            kind,
            "{name}"
        );
    }
}
