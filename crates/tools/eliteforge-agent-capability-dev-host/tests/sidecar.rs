use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_tools_mcp::DiscoverMcpTools;
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};

struct Layout;
#[tokio::test]
async fn remote_native_chat_sdk_can_call_host_and_dependencies_with_trace_context() {
    let directory = tempfile::tempdir().unwrap();
    let manager = CapabilityPackManager::new(directory.path()).unwrap();
    install(&manager, "1.0.0");
    let agent = builder(selection(&manager, "1.0.0", "model.host", false))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("host-context", "host-demo");
    request.extensions.insert(TraceContext {
        traceparent: "00-11111111111111111111111111111111-2222222222222222-01".into(),
        tracestate: "".into(),
    });
    let result = agent.run(request).await.unwrap();
    assert!(
        result
            .output
            .starts_with("host-context:nested-model:00-1111")
    );
    agent.close().await;
}
#[tokio::test]
async fn remote_loop_routes_to_a_dependency_package_and_denies_undeclared_calls() {
    let directory = tempfile::tempdir().unwrap();
    let manager = CapabilityPackManager::new(directory.path()).unwrap();
    install_with(&manager, "1.0.0", |manifest| {
        manifest["id"] = json!("demo.extensions");
        manifest["plugins"] = json!(["extension.plugin"]);
        manifest["pages"] = json!([]);
        manifest["permissions"] = json!([]);
        manifest["extension_points"][0]["owner"] = json!("extension.plugin");
        let implementations = manifest["implementations"].as_array_mut().unwrap();
        implementations.retain(|implementation| implementation["id"] == "demo.custom");
    });
    install_with(&manager, "1.0.0", |manifest| {
        manifest["extension_points"] = json!([]);
        manifest["dependencies"] = json!([{"id":"demo.extensions","version":"=1.0.0"}]);
        manifest["implementations"]
            .as_array_mut()
            .unwrap()
            .retain(|implementation| implementation["id"] != "demo.custom");
    });
    let pack = manager
        .select(
            "demo.pack",
            &"=1.0.0".parse().unwrap(),
            "nested.agent",
            "config.1",
            Value::Null,
        )
        .unwrap()
        .implementations([
            "demo.chat",
            "demo.state",
            "demo.interactions",
            "demo.conversations",
            "demo.loop",
        ])
        .unwrap();
    let agent = builder(pack).build().await.unwrap();
    assert_eq!(
        agent.execute(ListCapabilityInstances).await.unwrap().len(),
        2
    );
    assert_eq!(
        agent
            .run(RunRequest::new("nested", "hello"))
            .await
            .unwrap()
            .output,
        "reply@1.0.0"
    );
    agent.close().await;
}
struct Inspect;
#[async_trait]
impl AgentLoop for Inspect {
    async fn run(
        &self,
        _: &RunRequest,
        _: &RunContext,
        _: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        Err(AgentError::Run("inspection only".into()))
    }
}
#[tokio::test]
async fn mcp_only_package_owns_an_extra_child_and_unselected_mcp_stays_inert() {
    use eliteforge_agent_official_plugin_storage_local::LocalStoragePlugin;
    let directory = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir_in(directory.path()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(artifacts.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let manager = CapabilityPackManager::new(directory.path().join("store")).unwrap();
    let source = tempfile::tempdir().unwrap();
    let executable = if cfg!(windows) {
        "bin/mcp.exe"
    } else {
        "bin/mcp"
    };
    std::fs::create_dir_all(source.path().join("bin")).unwrap();
    std::fs::copy(
        env!("CARGO_BIN_EXE_capability-mcp"),
        source.path().join(executable),
    )
    .unwrap();
    std::fs::write(source.path().join("manifest.json"),serde_json::to_vec(&json!({"manifest_version":1,"id":"demo.mcp","name":"MCP only","version":"1.0.0","framework":"^0.1","mcp":[{"id":"demo.mcp.server","transport":"stdio","program":executable}]})).unwrap()).unwrap();
    manager
        .install_bytes(
            &build_capability_archive(source.path(), None).unwrap(),
            InstallPolicy::Development,
        )
        .unwrap();
    let selected = manager
        .select(
            "demo.mcp",
            &"=1.0.0".parse().unwrap(),
            "mcp.agent",
            "config.1",
            Value::Null,
        )
        .unwrap();
    let storage = directory.path().join("agent.sqlite");
    let make = |pack: ManagedCapabilityPack| {
        AgentBuilder::new()
            .default_models(false)
            .capability_pack(pack)
            .plugin_factory(ConfiguredPluginFactory::from_config(
                LocalStoragePlugin::new(&storage, artifacts.path()),
            ))
            .plugin_factory(ConfiguredPluginFactory::new(
                "framework.loop",
                vec![],
                || Ok(Arc::new(AgentLoopPlugin::new(Inspect))),
            ))
    };
    let agent = make(
        selected
            .clone()
            .contributions(CapabilityContributionSelection {
                mcp: Some(vec![]),
                ..Default::default()
            })
            .unwrap(),
    )
    .build()
    .await
    .unwrap();
    let identity = agent
        .execute(ListCapabilityInstances)
        .await
        .unwrap()
        .remove(0);
    let instance = manager.instance(&identity).unwrap();
    assert!(!instance.data_directory().unwrap().join("mcp.pid").exists());
    agent.close().await;
    let agent = make(selected).build().await.unwrap();
    let tools = agent
        .execute(DiscoverMcpTools {
            plugin_id: "demo.mcp.server".into(),
        })
        .await
        .unwrap();
    assert_eq!(tools.len(), 1);
    let identity = agent
        .execute(ListCapabilityInstances)
        .await
        .unwrap()
        .remove(0);
    let instance = manager.instance(&identity).unwrap();
    assert!(instance.pack().manifest.backend.is_none());
    let pid: i32 = std::fs::read_to_string(instance.data_directory().unwrap().join("mcp.pid"))
        .unwrap()
        .parse()
        .unwrap();
    assert!(agent.close().await.is_success());
    #[cfg(unix)]
    assert_eq!(
        unsafe { libc::kill(pid, 0) },
        -1,
        "managed stdio MCP child survived close"
    );
    #[cfg(windows)]
    {
        let output = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .output()
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&output.stdout)
                .split_whitespace()
                .any(|field| field == pid.to_string())
        );
    }
}
#[tokio::test]
async fn waiting_remote_tool_pins_original_version_across_host_restart() {
    let directory = tempfile::tempdir().unwrap();
    let manager = CapabilityPackManager::new(directory.path()).unwrap();
    install(&manager, "1.0.0");
    install(&manager, "2.0.0");
    let select = |version: &str| {
        manager
            .select(
                "demo.pack",
                &format!("={version}").parse().unwrap(),
                "durable.agent",
                "config.1",
                Value::Null,
            )
            .unwrap()
            .implementations([
                "demo.chat",
                "demo.state",
                "demo.interactions",
                "demo.conversations",
                "demo.tools",
                "demo.tool-policy",
                "demo.tool-middleware",
            ])
            .unwrap()
    };
    let agent = builder(select("1.0.0")).build().await.unwrap();
    let result = agent
        .run(RunRequest::new("waiting", "tool-demo-wait"))
        .await
        .unwrap();
    assert_eq!(result.status, RunStatus::AwaitingInput);
    let checkpoint = agent.latest_checkpoint("waiting").await.unwrap().unwrap();
    assert_eq!(checkpoint.capability_versions[0].version, "1.0.0");
    assert!(matches!(
        agent.reconfigure(builder(select("2.0.0"))).await,
        Err(ReconfigureError::Busy(_))
    ));
    agent.close().await;
    assert!(matches!(
        manager.remove("demo.pack", &"1.0.0".parse().unwrap()),
        Err(RuntimeError::Busy(_))
    ));
    let reopened = CapabilityPackManager::new(directory.path()).unwrap();
    assert!(!reopened.durable_pins().unwrap().is_empty());
    let wrong = builder(
        reopened
            .select(
                "demo.pack",
                &"=2.0.0".parse().unwrap(),
                "durable.agent",
                "config.1",
                Value::Null,
            )
            .unwrap()
            .implementations([
                "demo.chat",
                "demo.state",
                "demo.interactions",
                "demo.conversations",
                "demo.tools",
            ])
            .unwrap(),
    )
    .build()
    .await
    .unwrap();
    let mut request = RunRequest::new("waiting", "ignored");
    request.resume = Some(ResumeRequest {
        run_id: checkpoint.run_id,
        response: Some(InteractionResponse {
            interaction_id: result.interactions[0].id,
            value: json!("approved"),
        }),
    });
    assert!(wrong.run(request.clone()).await.is_err());
    wrong.close().await;
    let restored = builder(select("1.0.0")).build().await.unwrap();
    assert_eq!(
        restored.run(request).await.unwrap().status,
        RunStatus::Completed
    );
    assert!(manager.durable_pins().unwrap().is_empty());
    restored.close().await;
    manager
        .remove("demo.pack", &"1.0.0".parse().unwrap())
        .unwrap();
}
#[tokio::test]
async fn remote_tools_policy_middleware_and_skills_use_native_execution_lifecycle() {
    let directory = tempfile::tempdir().unwrap();
    let manager = CapabilityPackManager::new(directory.path()).unwrap();
    install(&manager, "1.0.0");
    let pack = manager
        .select(
            "demo.pack",
            &"=1.0.0".parse().unwrap(),
            "tools.agent",
            "config.1",
            Value::Null,
        )
        .unwrap()
        .implementations([
            "demo.chat",
            "demo.state",
            "demo.interactions",
            "demo.conversations",
            "demo.tools",
            "demo.tool-policy",
            "demo.tool-middleware",
            "demo.skills",
        ])
        .unwrap();
    let agent = builder(pack).build().await.unwrap();
    let result = agent
        .run(RunRequest::new("tools", "tool-demo"))
        .await
        .unwrap();
    assert_eq!(result.tool_calls, 1);
    assert_eq!(result.output, "reply@1.0.0");
    let result = agent
        .run(RunRequest::new("skills", "skill-demo"))
        .await
        .unwrap();
    assert_eq!(result.tool_calls, 2);
    assert_eq!(result.output, "reply@1.0.0");
    assert!(agent.close().await.is_success());
    assert!(manager.usage().blockers("demo.pack").is_empty());
}
#[async_trait]
impl Plugin for Layout {
    fn id(&self) -> &str {
        "layout"
    }
    fn dependencies(&self) -> Vec<String> {
        vec!["demo.reducer".into()]
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let reducer = context
            .services()
            .extension_from::<dyn ContextReducer>("demo.reducer")
            .ok_or_else(|| PluginError::message("missing remote reducer"))?;
        let mut history = ContextBlockConfig::new(
            HISTORY_BLOCK,
            BlockBudget {
                max_ratio_bps: Some(6000),
                max_messages: Some(1),
                max_tokens: None,
            },
        );
        history.reducer = reducer;
        context.provide(ContextLayoutService(ContextLayout(vec![
            ContextBlockConfig::new(INSTRUCTIONS_BLOCK, BlockBudget::ratio(2000)),
            history,
            ContextBlockConfig::new("remote", BlockBudget::ratio(2000)),
        ])))
    }
}

#[tokio::test]
async fn context_discovery_budget_estimation_reduction_memory_and_output_are_remote() {
    let directory = tempfile::tempdir().unwrap();
    let manager = CapabilityPackManager::new(directory.path()).unwrap();
    install(&manager, "1.0.0");
    let mut pack = manager
        .select(
            "demo.pack",
            &"=1.0.0".parse().unwrap(),
            "context.agent",
            "config.1",
            Value::Null,
        )
        .unwrap();
    pack = pack
        .implementations([
            "demo.chat",
            "demo.state",
            "demo.interactions",
            "demo.conversations",
            "demo.discovery",
            "demo.estimator",
            "demo.selection",
            "demo.context",
            "demo.contributor",
            "demo.reducer",
            "demo.budget",
            "demo.memory",
            "demo.model-middleware",
            "demo.lifecycle",
            "demo.observation",
            "demo.output",
        ])
        .unwrap();
    let agent = builder(pack.clone())
        .plugin_factory(ConfiguredPluginFactory::new(
            "layout",
            vec!["demo.reducer".into()],
            || Ok(Arc::new(Layout)),
        ))
        .build()
        .await
        .unwrap();
    // The observation backend never returns. This verifies non-blocking delivery
    // without treating a busy machine's execution speed as a correctness rule.
    let (handle, mut outputs) = agent
        .start_with_output::<RemoteOutputs>(RunRequest::new("context", "first"))
        .unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(60), agent.wait(handle))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.output, "reply@1.0.0");
    assert!(
        outputs
            .0
            .get_mut("demo.output")
            .unwrap()
            .next()
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        matches!(agent.reconfigure(builder(pack.clone())).await,Err(ReconfigureError::Busy(blockers))if blockers.iter().any(|b|b.label.contains("output_stream")))
    );
    drop(outputs);
    assert_eq!(
        agent
            .run(RunRequest::new("context", "second"))
            .await
            .unwrap()
            .output,
        "reply@1.0.0"
    );
    let value = MemoryWrite {
        id: "fact".into(),
        content: "remote-memory".into(),
        source: MemorySource {
            id: "test".into(),
            version: None,
        },
        expires_at_ms: None,
    };
    let entry = agent.save_memory("private", value, None).await.unwrap();
    assert_eq!(
        agent.memory("private", "fact").await.unwrap().version,
        entry.version
    );
    assert_eq!(
        agent
            .search_memory("private", MemoryQuery::new("remote"))
            .await
            .unwrap()
            .len(),
        1
    );
    agent
        .delete_memory("private", "fact", &entry.version)
        .await
        .unwrap();
    assert!(agent.close().await.is_success());
}

#[tokio::test]
async fn independently_selected_remote_context_assembler_and_workflow_nodes_run() {
    let directory = tempfile::tempdir().unwrap();
    let manager = CapabilityPackManager::new(directory.path()).unwrap();
    install(&manager, "1.0.0");
    let select = |ids: Vec<&str>, scope: &str| {
        manager
            .select(
                "demo.pack",
                &"=1.0.0".parse().unwrap(),
                scope,
                "config.1",
                Value::Null,
            )
            .unwrap()
            .implementations(ids)
            .unwrap()
    };
    let agent = builder(select(
        vec![
            "demo.chat",
            "demo.state",
            "demo.interactions",
            "demo.conversations",
            "demo.assembler",
        ],
        "assembly.agent",
    ))
    .build()
    .await
    .unwrap();
    assert_eq!(
        agent
            .run(RunRequest::new("custom-context", "hello"))
            .await
            .unwrap()
            .output,
        "reply@1.0.0"
    );
    agent.close().await;
    let graph = DagWorkflowDefinition {
        id: "remote-graph".into(),
        version: 1,
        nodes: vec![DagNode::new("one", "remote.echo", json!({"value":1}))],
        edges: vec![],
        max_parallel_nodes: 1,
    };
    let agent = builder(select(
        vec![
            "demo.state",
            "demo.interactions",
            "demo.conversations",
            "demo.node",
            "demo.node-middleware",
        ],
        "workflow.agent",
    ))
    .plugin_factory(ConfiguredPluginFactory::new(
        "framework.loop",
        vec![],
        move || {
            Ok(Arc::new(AgentLoopPlugin::new(
                DagWorkflowLoop::new(graph.clone()).unwrap(),
            )))
        },
    ))
    .build()
    .await
    .unwrap();
    let result = agent
        .run(RunRequest::new("remote-node", "hello"))
        .await
        .unwrap();
    assert_eq!(result.status, RunStatus::Completed);
    assert_eq!(result.structured, Some(json!({"value":1})));
    agent.close().await;
}

fn install(manager: &CapabilityPackManager, version: &str) -> InstalledCapabilityPack {
    install_with(manager, version, |_| {})
}
fn install_with(
    manager: &CapabilityPackManager,
    version: &str,
    modify: impl FnOnce(&mut Value),
) -> InstalledCapabilityPack {
    let source = tempfile::tempdir().unwrap();
    let mut manifest: Value = serde_json::from_str(include_str!(
        "../../../../examples/capability-pack/manifest.json"
    ))
    .unwrap();
    manifest["version"] = json!(version);
    modify(&mut manifest);
    let executable = if cfg!(windows) {
        "bin/backend.exe"
    } else {
        "bin/backend"
    };
    manifest["backend"]["executables"] = json!({platform_key():executable});
    std::fs::create_dir_all(source.path().join("bin")).unwrap();
    std::fs::create_dir_all(source.path().join("ui")).unwrap();
    std::fs::copy(
        env!("CARGO_BIN_EXE_capability-backend"),
        source.path().join(executable),
    )
    .unwrap();
    std::fs::write(
        source.path().join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    std::fs::write(
        source.path().join("ui/index.html"),
        "<p>Independent plugin page</p>",
    )
    .unwrap();
    manager
        .install_bytes(
            &build_capability_archive(source.path(), None).unwrap(),
            InstallPolicy::Development,
        )
        .unwrap()
}
fn selection(
    manager: &CapabilityPackManager,
    version: &str,
    namespace: &str,
    remote_loop: bool,
) -> ManagedCapabilityPack {
    let mut ids = vec![
        "demo.chat",
        "demo.state",
        "demo.interactions",
        "demo.conversations",
        "demo.middleware",
        "demo.custom",
    ];
    if remote_loop {
        ids.push("demo.loop");
    }
    manager
        .select(
            "demo.pack",
            &format!("={version}").parse().unwrap(),
            namespace,
            "configuration.1",
            json!({}),
        )
        .unwrap()
        .implementations(ids)
        .unwrap()
}
fn builder(selection: ManagedCapabilityPack) -> AgentBuilder {
    // Explicitly include the selection through a local parent to test recursive scope binding.
    AgentBuilder::new()
        .default_models(false)
        .capability_pack(CapabilityPackBuilder::new("application", ()).include(selection))
}

#[tokio::test]
async fn every_dedicated_model_operation_and_realtime_lease_crosses_the_sidecar() {
    use eliteforge_agent_capability_dev_host::media;
    let directory = tempfile::tempdir().unwrap();
    let manager = CapabilityPackManager::new(directory.path()).unwrap();
    install(&manager, "1.0.0");
    let pack = manager
        .select(
            "demo.pack",
            &"=1.0.0".parse().unwrap(),
            "media.agent",
            "configuration.1",
            Value::Null,
        )
        .unwrap();
    let pack = pack
        .implementations([
            "demo.chat",
            "demo.state",
            "demo.interactions",
            "demo.conversations",
            "demo.middleware",
            "demo.custom",
            "demo.loop",
            "demo.embedding",
            "demo.rerank",
            "demo.image",
            "demo.video",
            "demo.recognition",
            "demo.synthesis",
            "demo.realtime",
        ])
        .unwrap();
    let agent = builder(pack.clone()).build().await.unwrap();
    assert_eq!(
        agent
            .embed(EmbeddingRequest::texts(["one", "two"]))
            .await
            .unwrap()
            .vectors
            .len(),
        2
    );
    assert_eq!(
        agent
            .rerank(RerankRequest {
                choice: Default::default(),
                query: "one".into(),
                documents: vec!["one".into(), "two".into()],
                top_n: Some(1)
            })
            .await
            .unwrap()
            .results
            .len(),
        1
    );
    assert_eq!(
        agent
            .generate_image(ImageGenerationRequest {
                choice: Default::default(),
                // Exercise binary chunk transport through real stdio and the typed SDK.
                prompt: "image".repeat(2 * 1024 * 1024),
                size: None,
                count: 2,
                quality: None,
                provider_options: Default::default(),
            })
            .await
            .unwrap()
            .images
            .len(),
        2
    );
    let job = agent
        .submit_video(VideoGenerationRequest {
            choice: Default::default(),
            prompt: "video".into(),
            seconds: 1,
            size: None,
            first_frame: None,
            last_frame: None,
            reference_images: Vec::new(),
            reference_videos: Vec::new(),
            reference_audios: Vec::new(),
            provider_options: Default::default(),
        })
        .await
        .unwrap();
    assert!(
        matches!(agent.reconfigure(builder(pack.clone())).await, Err(ReconfigureError::Busy(items)) if items.iter().any(|item|item.label.contains("model_job")))
    );
    let serialized: VideoJob = serde_json::from_value(json!(job)).unwrap();
    assert_eq!(
        agent.query_video(&serialized).await.unwrap().status,
        ModelJobStatus::Completed
    );
    agent.cancel_video(&job).await.unwrap();
    assert!(agent.query_video(&job).await.is_err());
    assert_eq!(
        agent
            .transcribe(SpeechRecognitionRequest {
                choice: Default::default(),
                audio: media("audio/wav"),
                language: None
            })
            .await
            .unwrap()
            .text,
        "heard"
    );
    assert_eq!(
        agent
            .synthesize(SpeechSynthesisRequest {
                choice: Default::default(),
                text: "speech".into(),
                voice: "demo".into(),
                format: "wav".into()
            })
            .await
            .unwrap()
            .audio
            .mime_type,
        "audio/wav"
    );
    let realtime = agent
        .connect_realtime(RealtimeRequest {
            choice: Default::default(),
            input: vec![Modality::Audio],
            output: vec![Modality::Text],
        })
        .await
        .unwrap();
    agent
        .send_realtime(&realtime, RealtimeInput::Commit)
        .await
        .unwrap();
    assert!(
        matches!(agent.next_realtime(&realtime).await.unwrap(),Some(RealtimeEvent::Transcript(text))if text=="realtime")
    );
    assert!(
        matches!(agent.reconfigure(builder(pack.clone())).await,Err(ReconfigureError::Busy(blockers))if blockers.iter().any(|b|b.kind=="realtime"))
    );
    agent.close_realtime(&realtime).await.unwrap();
    agent.reconfigure(builder(pack)).await.unwrap();
    agent.close().await;
    assert!(manager.usage().blockers("demo.pack").is_empty());
}
#[tokio::test]
async fn real_stdio_process_streaming_storage_middleware_and_idle_update() {
    let directory = tempfile::tempdir().unwrap();
    let manager = CapabilityPackManager::new(directory.path()).unwrap();
    let v1 = install(&manager, "1.0.0");
    install(&manager, "2.0.0");
    let agent = builder(selection(&manager, "1.0.0", "agent.one", false))
        .build()
        .await
        .unwrap();
    let clone = agent.clone();
    let instances = agent.execute(ListCapabilityInstances).await.unwrap();
    assert_eq!(instances.len(), 1, "all selected roles share one backend");
    let instance = manager.instance(&instances[0]).unwrap();
    let (page, mut page_events) = PageSession::new(
        instance.clone(),
        "demo.page",
        PageContext {
            theme: "dark".into(),
            language: "zh-CN".into(),
            data: Value::Null,
        },
        Default::default(),
        Arc::new(NoPageHost),
    )
    .unwrap();
    assert_eq!(
        page.call(page.id(), &instance.identity, "echo", json!({"page":true}))
            .await
            .unwrap(),
        json!({"page":true})
    );
    let event = page_events.recv().await.unwrap();
    assert_eq!(event.session, page.id());
    assert_eq!(event.topic, "chart");
    assert_eq!(event.data["source"], "page-only");
    assert!(
        matches!(agent.reconfigure(builder(selection(&manager,"2.0.0","agent.one",false))).await,Err(ReconfigureError::Busy(blockers)) if blockers.iter().any(|b|b.label.contains("page")))
    );
    page.close();
    assert_eq!(
        agent
            .run(RunRequest::new("session", "hello"))
            .await
            .unwrap()
            .output,
        "reply@1.0.0"
    );
    assert_eq!(
        agent
            .execute(RemoteExtensionCall {
                implementation: "demo.custom".into(),
                operation: "echo".into(),
                params: json!(42)
            })
            .await
            .unwrap(),
        json!(42)
    );
    agent
        .reconfigure(builder(selection(&manager, "2.0.0", "agent.one", false)))
        .await
        .unwrap();
    assert_eq!(
        clone
            .run(RunRequest::new("session", "again"))
            .await
            .unwrap()
            .output,
        "reply@2.0.0"
    );
    assert!(
        page.call(page.id(), &instance.identity, "echo", Value::Null)
            .await
            .is_err()
    );
    assert!(instance.is_retired());
    manager.remove("demo.pack", &v1.manifest.version).unwrap();
    assert!(agent.close().await.is_success());
    assert!(manager.running_instances().is_empty());
    assert!(manager.usage().blockers("demo.pack").is_empty());
}
#[tokio::test]
async fn agent_scopes_are_isolated_and_remote_loop_can_call_host_model() {
    let directory = tempfile::tempdir().unwrap();
    let manager = CapabilityPackManager::new(directory.path()).unwrap();
    install(&manager, "1.0.0");
    let pack = selection(&manager, "1.0.0", "application", true);
    let first = builder(pack.clone()).build().await.unwrap();
    let second = builder(pack).build().await.unwrap();
    let a = first.execute(ListCapabilityInstances).await.unwrap();
    let b = second.execute(ListCapabilityInstances).await.unwrap();
    assert_ne!(a[0].scope, b[0].scope);
    assert_eq!(
        first
            .run(RunRequest::new("nested", "call through host"))
            .await
            .unwrap()
            .output,
        "reply@1.0.0"
    );
    first.close().await;
    second.close().await;
    assert!(manager.running_instances().is_empty());
}
#[tokio::test]
async fn process_crash_returns_unknown_write_result_and_requires_explicit_rebuild() {
    let directory = tempfile::tempdir().unwrap();
    let manager = CapabilityPackManager::new(directory.path()).unwrap();
    let pack = install(&manager, "1.0.0");
    let instance = BackendInstance::new(
        manager.clone(),
        pack.clone(),
        "crash.scope",
        "config.1",
        Value::Null,
    )
    .unwrap();
    let implementation = RemoteImplementation {
        instance: instance.clone(),
        declaration: pack
            .manifest
            .implementations
            .iter()
            .find(|i| i.id == "demo.custom")
            .unwrap()
            .clone(),
    };
    assert!(matches!(
        implementation.invoke("crash", Value::Null).await,
        Err(RuntimeError::Indeterminate(_))
    ));
    assert!(implementation.invoke("echo", json!(1)).await.is_err());
    assert_eq!(
        std::fs::read_to_string(
            instance
                .data_directory()
                .unwrap()
                .join("unknown-write.count")
        )
        .unwrap(),
        "1"
    );
    instance.shutdown().await.unwrap();
    assert!(manager.usage().blockers("demo.pack").is_empty());
}
#[test]
fn signed_archives_validate_trust_and_dependency_solver_backtracks() {
    let directory = tempfile::tempdir().unwrap();
    let manager = CapabilityPackManager::new(directory.path()).unwrap();
    let signing = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
    manager
        .trust_key("test", signing.verifying_key().to_bytes())
        .unwrap();
    fn pack(root: &Path, id: &str, version: &str, deps: Value) {
        std::fs::write(root.join("manifest.json"),serde_json::to_vec(&json!({"manifest_version":1,"id":id,"name":id,"version":version,"framework":"^0.1","dependencies":deps})).unwrap()).unwrap();
    }
    let source = tempfile::tempdir().unwrap();
    for (id, version, deps) in [
        ("shared", "1.0.0", json!([])),
        ("shared", "2.0.0", json!([])),
        ("consumer", "1.0.0", json!([{"id":"shared","version":"^1"}])),
        (
            "root",
            "1.0.0",
            json!([{"id":"shared","version":"*"},{"id":"consumer","version":"*"}]),
        ),
    ] {
        pack(source.path(), id, version, deps);
        let bytes = build_capability_archive(source.path(), Some(("test", &signing))).unwrap();
        manager
            .install_bytes(&bytes, InstallPolicy::Signed)
            .unwrap();
    }
    let graph = manager.resolve("root", &"*".parse().unwrap()).unwrap();
    assert_eq!(
        graph
            .iter()
            .find(|p| p.manifest.id == "shared")
            .unwrap()
            .manifest
            .version
            .to_string(),
        "1.0.0"
    );
}

#[tokio::test]
async fn remote_agent_loop_uses_scoped_host_state_and_persisted_interaction_on_resume() {
    let directory = tempfile::tempdir().unwrap();
    let manager = CapabilityPackManager::new(directory.path()).unwrap();
    install(&manager, "1.0.0");
    install(&manager, "2.0.0");
    let agent = builder(selection(&manager, "1.0.0", "custom.wait", true))
        .build()
        .await
        .unwrap();
    let waiting = agent
        .run(RunRequest::new("custom-loop", "remote-wait"))
        .await
        .unwrap();
    assert_eq!(waiting.status, RunStatus::AwaitingInput);
    let pinned = manager.durable_pins().unwrap();
    assert!(!pinned.is_empty());
    assert!(matches!(
        agent
            .reconfigure(builder(selection(&manager, "2.0.0", "custom.wait", true)))
            .await,
        Err(ReconfigureError::Busy(_))
    ));
    let resume = agent
        .answer(
            "custom-loop",
            waiting.interactions[0].id,
            json!("persisted answer"),
        )
        .await
        .unwrap();
    assert_eq!(agent.run(resume).await.unwrap().output, "persisted answer");
    assert!(manager.durable_pins().unwrap().is_empty());
    assert!(agent.close().await.is_success());
}

#[derive(Clone, Default)]
struct ContextStoresProbe {
    summaries:
        Arc<std::sync::Mutex<Option<Arc<dyn eliteforge_agent_context_contracts::SummaryStore>>>>,
    artifacts:
        Arc<std::sync::Mutex<Option<Arc<dyn eliteforge_agent_context_contracts::ArtifactStore>>>>,
}
#[async_trait]
impl Plugin for ContextStoresProbe {
    fn id(&self) -> &str {
        "context.probe"
    }
    fn dependencies(&self) -> Vec<String> {
        vec!["demo.summaries".into(), "demo.artifacts".into()]
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        *self.summaries.lock().unwrap() =
            context.extension::<dyn eliteforge_agent_context_contracts::SummaryStore>();
        *self.artifacts.lock().unwrap() =
            context.extension::<dyn eliteforge_agent_context_contracts::ArtifactStore>();
        Ok(())
    }
}
#[tokio::test]
async fn remote_summary_cas_and_artifact_ranges_survive_restart_with_typed_errors() {
    use eliteforge_agent_context_contracts::*;
    use tokio_util::sync::CancellationToken;
    let directory = tempfile::tempdir().unwrap();
    let manager = CapabilityPackManager::new(directory.path()).unwrap();
    install(&manager, "1.0.0");
    let probe = ContextStoresProbe::default();
    let build = || {
        builder(
            manager
                .select(
                    "demo.pack",
                    &"=1.0.0".parse().unwrap(),
                    "context.storage",
                    "config.1",
                    Value::Null,
                )
                .unwrap()
                .implementations([
                    "demo.chat",
                    "demo.summaries",
                    "demo.artifacts",
                    "demo.conversations",
                    "demo.state",
                    "demo.interactions",
                ])
                .unwrap(),
        )
        .plugin_factory(ConfiguredPluginFactory::from_config(probe.clone()))
    };
    let agent = build().build().await.unwrap();
    let summary = probe.summaries.lock().unwrap().clone().unwrap();
    let artifacts = probe.artifacts.lock().unwrap().clone().unwrap();
    let key = SummaryKey {
        conversation: ConversationKey::new("summary.session"),
        generation: "one".into(),
        block_id: "history".into(),
        policy_version: "1".into(),
        configuration_digest: "a".repeat(64),
    };
    let value = ConversationSummary {
        format_version: 1,
        source_revision: 2,
        covered_messages: 2,
        source_digest: "b".repeat(64),
        summary: "durable remote summary".into(),
    };
    assert!(summary.compare_exchange(&key, None, &value).await.unwrap());
    assert!(!summary.compare_exchange(&key, None, &value).await.unwrap());
    let previous = summary.load(&key).await.unwrap().unwrap();
    assert!(!summary.delete(&key, "stale").await.unwrap());
    let bytes = vec![0xab; 9 * 1024 * 1024];
    let metadata = artifacts
        .put(bytes.clone(), CancellationToken::new())
        .await
        .unwrap();
    let first = artifacts
        .read(&metadata.id, 0, 65536, CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(first.bytes, bytes[..65536]);
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(matches!(
        artifacts.put(vec![1], cancelled).await,
        Err(ArtifactError::Cancelled)
    ));
    assert!(agent.close().await.is_success());
    assert!(summary.load(&key).await.is_err());
    let restored = build().build().await.unwrap();
    let summary = probe.summaries.lock().unwrap().clone().unwrap();
    let artifacts = probe.artifacts.lock().unwrap().clone().unwrap();
    assert_eq!(
        summary.load(&key).await.unwrap().unwrap().version,
        previous.version
    );
    let tail = artifacts
        .read(
            &metadata.id,
            metadata.size_bytes - 128,
            128,
            CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(tail.eof());
    assert_eq!(tail.bytes, vec![0xab; 128]);
    assert!(summary.delete(&key, &previous.version).await.unwrap());
    artifacts
        .remove(&metadata.id, CancellationToken::new())
        .await
        .unwrap();
    assert!(matches!(
        artifacts
            .read(&metadata.id, 0, 1, CancellationToken::new())
            .await,
        Err(ArtifactError::NotFound)
    ));
    assert!(restored.close().await.is_success());
}

#[tokio::test]
async fn backend_receives_exact_plugin_and_page_selection_before_acquiring_resources() {
    let directory = tempfile::tempdir().unwrap();
    let manager = CapabilityPackManager::new(directory.path()).unwrap();
    install(&manager, "1.0.0");
    let select = |enabled| {
        selection(&manager, "1.0.0", "contributions.agent", false)
            .contributions(CapabilityContributionSelection {
                plugins: Some(if enabled {
                    vec!["demo.plugin".into()]
                } else {
                    vec![]
                }),
                pages: Some(vec![]),
                ..Default::default()
            })
            .unwrap()
    };
    let agent = builder(select(false)).build().await.unwrap();
    let instance = agent
        .execute(ListCapabilityInstances)
        .await
        .unwrap()
        .remove(0);
    let instance = manager.instance(&instance).unwrap();
    assert!(
        !instance
            .data_directory()
            .unwrap()
            .join("selected-plugin.marker")
            .exists()
    );
    assert!(
        PageSession::new(
            instance,
            "demo.page",
            PageContext {
                theme: "light".into(),
                language: "en".into(),
                data: Value::Null
            },
            Default::default(),
            Arc::new(NoPageHost)
        )
        .is_err()
    );
    assert!(agent.close().await.is_success());
    let selected = builder(select(true)).build().await.unwrap();
    let instance = selected
        .execute(ListCapabilityInstances)
        .await
        .unwrap()
        .remove(0);
    let instance = manager.instance(&instance).unwrap();
    assert!(
        instance
            .data_directory()
            .unwrap()
            .join("selected-plugin.marker")
            .exists()
    );
    assert!(selected.close().await.is_success());
}
struct StaticSkillLoop;
#[async_trait]
impl AgentLoop for StaticSkillLoop {
    async fn run(
        &self,
        _: &RunRequest,
        run: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let mut text = String::new();
        for provider in &services.context_sources {
            for message in provider
                .contribute(&ContextInput {
                    stage: "test",
                    step: 1,
                    model: &ModelRef::new("test", "test"),
                    budget: ContextBudget {
                        max_tokens: 100_000,
                        max_messages: 4096,
                    },
                    current_input: &Content::default(),
                    history: &ConversationSnapshot::new(
                        ConversationKey::new(&run.session_id),
                        "test",
                        Vec::new(),
                    ),
                    estimator: &ConservativeTokenEstimator,
                    run,
                })
                .await?
                .messages
            {
                text.push_str(&message.content.text());
            }
        }
        for provider in &services.tools {
            let lease = provider.lease(run).await?;
            for tool in &lease.tools {
                if tool.descriptor.call_name == "skill_load" && text.contains("pack-skill") {
                    text.push_str(
                        &tool
                            .executor
                            .call(json!({"name":"pack-skill"}), run)
                            .await
                            .map_err(|e| AgentError::Run(e.to_string()))?
                            .content
                            .text(),
                    );
                }
            }
            run.own_resources(lease.resources);
        }
        Ok(RunResult {
            content: Content::from(text.clone()),
            output: text,
            status: RunStatus::Completed,
            interactions: vec![],
            structured: None,
            usage: Usage::default(),
            steps: 1,
            tool_calls: 1,
            external_session: None,
        })
    }
}
#[tokio::test]
async fn static_skills_and_resources_package_uses_native_runtime_without_a_backend() {
    use eliteforge_agent_official_plugin_skills::SkillsPlugin;
    use eliteforge_agent_official_plugin_storage_local::LocalStoragePlugin;
    let directory = tempfile::tempdir().unwrap();
    let artifacts = tempfile::tempdir_in(directory.path()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(artifacts.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let source = tempfile::tempdir().unwrap();
    let manager = CapabilityPackManager::new(directory.path().join("store")).unwrap();
    std::fs::create_dir_all(source.path().join("skills/pack-skill")).unwrap();
    std::fs::write(source.path().join("skills/pack-skill/SKILL.md"),"---\nname: pack-skill\ndescription: A packaged static skill\n---\n# Static skill body\nUse packaged instructions.").unwrap();
    std::fs::write(source.path().join("facts.txt"), "packaged resource").unwrap();
    std::fs::write(source.path().join("manifest.json"),serde_json::to_vec(&json!({"manifest_version":1,"id":"static.pack","name":"Static","version":"1.0.0","framework":"^0.1","skills":["skills"],"resources":["facts.txt"]})).unwrap()).unwrap();
    manager
        .install_bytes(
            &build_capability_archive(source.path(), None).unwrap(),
            InstallPolicy::Development,
        )
        .unwrap();
    let selection = manager
        .select(
            "static.pack",
            &"=1.0.0".parse().unwrap(),
            "static.agent",
            "config.1",
            Value::Null,
        )
        .unwrap();
    let make = |pack| {
        AgentBuilder::new()
            .default_models(false)
            .capability_pack(pack)
            .plugin_factory(ConfiguredPluginFactory::from_config(
                LocalStoragePlugin::new(directory.path().join("agent.db"), artifacts.path()),
            ))
            .plugin_factory(ConfiguredPluginFactory::from_config(
                SkillsPlugin::sources_only(),
            ))
            .plugin_factory(ConfiguredPluginFactory::new(
                "framework.loop",
                vec![],
                || Ok(Arc::new(AgentLoopPlugin::new(StaticSkillLoop))),
            ))
    };
    let selected = make(selection.clone()).build().await.unwrap();
    assert!(
        selected
            .run(RunRequest::new("static.skill", "hello"))
            .await
            .unwrap()
            .output
            .contains("Static skill body")
    );
    assert!(
        !manager
            .usage()
            .all_blockers()
            .iter()
            .any(|item| item.kind == "runtime")
    );
    assert!(selected.close().await.is_success());
    let excluded = make(
        selection
            .contributions(CapabilityContributionSelection {
                skills: Some(vec![]),
                ..Default::default()
            })
            .unwrap(),
    )
    .build()
    .await
    .unwrap();
    assert!(
        !excluded
            .run(RunRequest::new("excluded.skill", "hello"))
            .await
            .unwrap()
            .output
            .contains("Static skill body")
    );
    assert!(excluded.close().await.is_success());
}

#[tokio::test]
async fn agent_operation_reports_unknown_effect_and_does_not_replay_the_write() {
    let directory = tempfile::tempdir().unwrap();
    let manager = CapabilityPackManager::new(directory.path()).unwrap();
    install(&manager, "1.0.0");
    let agent = builder(selection(&manager, "1.0.0", "unknown.effect", false))
        .build()
        .await
        .unwrap();
    assert!(matches!(
        agent
            .execute(RemoteExtensionCall {
                implementation: "demo.custom".into(),
                operation: "crash".into(),
                params: Value::Null
            })
            .await,
        Err(AgentError::Indeterminate(_))
    ));
    assert!(
        agent
            .execute(RemoteExtensionCall {
                implementation: "demo.custom".into(),
                operation: "echo".into(),
                params: Value::Null
            })
            .await
            .is_err()
    );
    agent.close().await;
}

#[tokio::test]
async fn nested_extension_callback_scopes_share_the_run_call_budget() {
    let directory = tempfile::tempdir().unwrap();
    let manager = CapabilityPackManager::new(directory.path()).unwrap();
    install(&manager, "1.0.0");
    let agent = builder(selection(&manager, "1.0.0", "nested.budget", true))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("nested", "nested-budget");
    request.limits.max_steps = 1;
    assert_eq!(agent.run(request).await.unwrap().output, "quota preserved");
    agent.close().await;
}

struct CompressionDirectiveLoop(Arc<dyn ContextReducer>);
#[async_trait]
impl AgentLoop for CompressionDirectiveLoop {
    async fn run(
        &self,
        request: &RunRequest,
        run: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let directive = match request.input.text().as_str() {
            "block" => Some(ContextCompressionDirective {
                trigger: ContextCompressionTrigger::Block,
                target_tokens: 37,
            }),
            "global" => Some(ContextCompressionDirective {
                trigger: ContextCompressionTrigger::Global,
                target_tokens: 81,
            }),
            _ => None,
        };
        let block = ContextBlock::data("remote", "original remote data");
        let history = ConversationSnapshot::new(
            ConversationKey::new(&run.session_id),
            "remote-test-generation",
            vec![Message::user("current input")],
        );
        let model = ModelRef::new("demo", "offline");
        let messages = self
            .0
            .reduce(&ContextReductionInput {
                block: &block,
                budget: ContextBudget {
                    max_tokens: 512,
                    max_messages: 1,
                },
                model: &model,
                estimator: &ConservativeTokenEstimator,
                history: &history,
                run,
                models: services.models.as_deref().expect("selected SDK models"),
                limits: &request.limits,
                overflow_recoveries: 0,
                compression: directive,
            })
            .await?;
        let content = messages[0].content.clone();
        Ok(RunResult {
            output: content.text(),
            content,
            structured: None,
            interactions: vec![],
            status: RunStatus::Completed,
            usage: run.usage(),
            steps: 1,
            tool_calls: 0,
            external_session: None,
        })
    }
}
struct CompressionDirectivePlugin;
#[async_trait]
impl Plugin for CompressionDirectivePlugin {
    fn id(&self) -> &str {
        "compression-directive-loop"
    }
    fn dependencies(&self) -> Vec<String> {
        vec!["demo.reducer".into()]
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let reducer = context
            .services()
            .extension_from::<dyn ContextReducer>("demo.reducer")
            .ok_or_else(|| PluginError::message("missing remote reducer"))?;
        AgentLoopPlugin::new(CompressionDirectiveLoop(reducer))
            .install(context)
            .await
    }
}
#[tokio::test]
async fn remote_reducer_transports_block_global_targets_and_accepts_legacy_no_directive() {
    let directory = tempfile::tempdir().unwrap();
    let manager = CapabilityPackManager::new(directory.path()).unwrap();
    install(&manager, "1.0.0");
    let pack = manager
        .select(
            "demo.pack",
            &"=1.0.0".parse().unwrap(),
            "compression.agent",
            "configuration.1",
            Value::Null,
        )
        .unwrap()
        .implementations([
            "demo.chat",
            "demo.state",
            "demo.interactions",
            "demo.conversations",
            "demo.reducer",
        ])
        .unwrap();
    let agent = builder(pack)
        .plugin_factory(ConfiguredPluginFactory::new(
            "compression-directive-loop",
            vec!["demo.reducer".into()],
            || Ok(Arc::new(CompressionDirectivePlugin)),
        ))
        .build()
        .await
        .unwrap();
    for (input, expected) in [
        ("block", "remote Block target 37"),
        ("global", "remote Global target 81"),
        ("none", "original remote data"),
    ] {
        let result = agent
            .run(RunRequest::new(format!("compression-{input}"), input))
            .await
            .unwrap();
        assert_eq!(result.output, expected);
    }
    let identity = agent
        .execute(ListCapabilityInstances)
        .await
        .unwrap()
        .remove(0);
    let instance = manager.instance(&identity).unwrap();
    let declaration = instance
        .pack()
        .manifest
        .implementations
        .iter()
        .find(|implementation| implementation.id == "demo.reducer")
        .unwrap()
        .clone();
    let remote = RemoteImplementation {
        instance,
        declaration,
    };
    let legacy = remote.invoke("reduce", json!({"block": ContextBlock::data("remote", "legacy request data"), "budget": ContextBudget { max_tokens: 512, max_messages: 1 }})).await.unwrap();
    let messages: Vec<Message> = serde_json::from_value(legacy).unwrap();
    assert_eq!(messages, vec![Message::user("legacy request data")]);
    assert!(agent.close().await.is_success());
    assert!(manager.usage().blockers("demo.pack").is_empty());
}
