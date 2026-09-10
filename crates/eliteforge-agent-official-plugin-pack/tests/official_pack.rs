use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_official_plugin_pack::{
    EliteForgeOfficialPluginPack, agui::AgUiStream, human::AskUserPresentation,
    mcp::DiscoverMcpTools, model::GenaiPlugin, storage::SqliteStoragePlugin,
};
use serde_json::{Value, json};

struct AskThenReply;
#[async_trait]
impl ChatModel for AskThenReply {
    async fn generate(
        &self,
        _: &str,
        request: ChatRequest,
        stream: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        assert!(request.tools.iter().any(|tool| tool.id == "human.ask_user"));
        if request
            .messages
            .iter()
            .any(|message| message.role == Role::Tool)
        {
            stream.text_delta("已按你的选择完成。").await;
            Ok(ChatResponse::text("已按你的选择完成。"))
        } else {
            Ok(ChatResponse::tool_calls(vec![ToolCall::new(
                "ask-1",
                "ask_user",
                json!({
                    "intro": "选择语言", "questions": [{"question": "使用哪种语言？",
                        "options": [{"label": "中文"}, {"label": "英文"}]}]
                }),
            )]))
        }
    }
}

async fn collect(agent: &Agent, request: RunRequest) -> (RunResult, Vec<Value>) {
    let (handle, mut output) = agent.start_with_output::<AgUiStream>(request).unwrap();
    let receive = async {
        let mut events = Vec::new();
        while let Some(event) = output.recv().await {
            events.push(serde_json::to_value(event).unwrap());
        }
        events
    };
    let (result, events) = tokio::join!(agent.wait(handle), receive);
    assert_eq!(events.first().unwrap()["type"], "RUN_STARTED");
    assert_eq!(events.last().unwrap()["type"], "RUN_FINISHED");
    assert!(events.iter().all(|event| event["type"] != "RUN_ERROR"));
    (result.unwrap(), events)
}

#[tokio::test]
async fn official_pack_supports_agent_output_questions_and_persistent_resume() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("agent.sqlite");
    let pack = || {
        EliteForgeOfficialPluginPack::new(&path)
            .without_skills()
            .model(ChatModelPlugin::new(AskThenReply))
    };
    let builder = AgentBuilder::new()
        .default_models(false)
        .plugin_pack(pack());
    assert!(
        !path.exists(),
        "configuring the pack must not open the database"
    );
    let agent = builder.build().await.unwrap();
    assert!(
        agent
            .execute(DiscoverMcpTools {
                plugin_id: "tools.mcp".into()
            })
            .await
            .unwrap()
            .is_empty()
    );
    let (result, _) = collect(&agent, RunRequest::new("chat", "写一段说明")).await;
    assert_eq!(result.status, RunStatus::AwaitingInput);
    assert_eq!(agent.pending_interactions("chat").await.unwrap().len(), 1);
    let interaction = result.interactions[0].clone();
    assert!(agent.close().await.is_success());
    drop(agent);

    let agent = AgentBuilder::new()
        .default_models(false)
        .plugin_pack(pack())
        .build()
        .await
        .unwrap();
    assert_eq!(
        agent.pending_interactions("chat").await.unwrap()[0].request,
        interaction
    );
    let presentation: AskUserPresentation =
        serde_json::from_value(interaction.presentation.unwrap()).unwrap();
    let question = &presentation.questions[0];
    let resume = agent
        .answer(
            "chat",
            interaction.id,
            json!({"answers": {
                question.id.to_string(): {"option_id": question.options[0].id}
            }}),
        )
        .await
        .unwrap();
    let (result, events) = collect(&agent, resume).await;
    assert_eq!(result.output, "已按你的选择完成。");
    assert!(
        events
            .iter()
            .any(|event| event["type"] == "TEXT_MESSAGE_CONTENT")
    );
    assert!(agent.pending_interactions("chat").await.unwrap().is_empty());
    let history = agent
        .conversation(&ConversationKey::new("chat"))
        .await
        .unwrap();
    assert!(
        history
            .messages
            .iter()
            .any(|message| message.content.text() == result.output)
    );
    assert!(agent.close().await.is_success());
    assert!(matches!(
        agent.pending_interactions("chat").await,
        Err(AgentError::Closed)
    ));
    assert!(matches!(
        agent
            .execute(DiscoverMcpTools {
                plugin_id: "tools.mcp".into()
            })
            .await,
        Err(AgentError::Closed)
    ));
}

#[tokio::test]
async fn pack_installs_explicit_genai_even_when_framework_auto_models_are_disabled() {
    let directory = tempfile::tempdir().unwrap();
    let agent = AgentBuilder::new()
        .default_models(false)
        .plugin_pack(
            EliteForgeOfficialPluginPack::new(directory.path().join("agent.sqlite"))
                .model(GenaiPlugin::new("configured-chat").connection("gateway")),
        )
        .build()
        .await
        .unwrap();
    let models = agent.models(ModelOperation::Chat);
    assert_eq!(models.len(), 1);
    assert_eq!(models[0], ModelRef::new("gateway", "configured-chat"));
    assert!(agent.close().await.is_success());
}

#[test]
fn official_pack_registers_skills_by_default_and_can_disable_them() {
    let directory = tempfile::tempdir().unwrap();
    let plugin_ids = |pack: EliteForgeOfficialPluginPack| {
        pack.into_plugins()
            .into_iter()
            .map(|plugin| plugin.id().to_owned())
            .collect::<Vec<_>>()
    };

    assert!(
        plugin_ids(EliteForgeOfficialPluginPack::new(
            directory.path().join("default.sqlite"),
        ))
        .iter()
        .any(|id| id == "skills.default")
    );
    assert!(
        !plugin_ids(
            EliteForgeOfficialPluginPack::new(directory.path().join("disabled.sqlite"))
                .without_skills(),
        )
        .iter()
        .any(|id| id == "skills.default")
    );
}

struct PlainReply;
#[async_trait]
impl ChatModel for PlainReply {
    async fn generate(
        &self,
        _: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        assert!(request.tools.is_empty());
        Ok(ChatResponse::text("plain"))
    }
}

#[tokio::test]
async fn defaults_can_be_explicitly_replaced_or_disabled() {
    let directory = tempfile::tempdir().unwrap();
    let original = directory.path().join("original.sqlite");
    let replacement = directory.path().join("replacement.sqlite");
    let agent = AgentBuilder::new()
        .default_models(false)
        .plugin_pack(
            EliteForgeOfficialPluginPack::new(&original)
                .without_model()
                .without_agui()
                .human_interaction(false)
                .without_skills()
                .storage(SqliteStoragePlugin::new(&replacement)),
        )
        .plugin(ChatModelPlugin::new(PlainReply))
        .build()
        .await
        .unwrap();
    assert!(!original.exists());
    assert!(replacement.exists());
    assert!(
        agent
            .start_with_output::<AgUiStream>(RunRequest::new("chat", "missing output"))
            .is_err()
    );
    assert_eq!(
        agent
            .run(RunRequest::new("chat", "hello"))
            .await
            .unwrap()
            .output,
        "plain"
    );
    assert!(agent.close().await.is_success());
}
