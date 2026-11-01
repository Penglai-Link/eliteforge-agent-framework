use eliteforge_agent_core::*;
#[test]
fn concurrent_readers_keep_their_own_language() {
    let tool=std::sync::Arc::new(ToolDescriptor::new("test", "test", "Canonical", serde_json::json!({})).with_display_resource(r#"{"defaultLocale":"en-US","tools":{"test":{"zh-CN":{"name":"测试"},"en-US":{"name":"Test"}}}}"#));
    let readers = (0..24)
        .map(|index| {
            let tool = tool.clone();
            std::thread::spawn(move || {
                let (locale, name) = if index % 2 == 0 {
                    ("zh-CN", "测试")
                } else {
                    ("en-US", "Test")
                };
                for _ in 0..20 {
                    assert_eq!(tool.resolve_display(locale).name, name);
                }
            })
        })
        .collect::<Vec<_>>();
    for reader in readers {
        reader.join().unwrap();
    }
}
#[test]
fn locale_fallback_is_per_field_and_preserves_model_contract() {
    let tool = ToolDescriptor::new("third.party", "third_party", "Canonical model instructions", serde_json::json!({"properties":{"path":{"type":"string"}}}))
            .with_display_resource(r#"{"defaultLocale":"en-US","tools":{"third.party":{"en-US":{"name":"Read file","description":"Read a file","parameters":{"/properties/path":"File path"}},"zh-CN":{"name":"读取文件","parameters":{"/properties/path":"文件路径"}}}}}"#);
    let original = serde_json::to_value(&tool).unwrap();
    let zh = tool.resolve_display("fr-FR;q=0.9,zh-TW;q=0.8,en;q=0.1");
    assert_eq!(zh.name, "读取文件");
    assert_eq!(zh.description, "Read a file");
    assert_eq!(
        zh.input_schema["properties"]["path"]["description"],
        "文件路径"
    );
    assert_eq!(tool.resolve_display("de-DE").name, "Read file");
    assert_eq!(
        tool.resolve_display("en-US").input_schema["properties"]["path"]["description"],
        "File path"
    );
    assert_eq!(serde_json::to_value(&tool).unwrap(), original);
    assert_eq!(tool.description, "Canonical model instructions");
    assert!(
        tool.input_schema["properties"]["path"]
            .get("description")
            .is_none()
    );
}
