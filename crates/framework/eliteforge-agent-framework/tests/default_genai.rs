#![cfg(feature = "genai")]
use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_test_support::*;

struct LocalChat;
#[async_trait]
impl ChatModel for LocalChat {
    async fn generate(
        &self,
        _: &str,
        _: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        Ok(ChatResponse::text("local"))
    }
}
#[test]
fn default_genai_configuration_is_isolated_in_subprocesses() {
    for mode in ["configured", "missing", "custom", "disabled"] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "default_genai_environment_child", "--nocapture"])
            .env("AGENT_TEST_GENAI_MODE", mode)
            .env_remove("MODEL")
            .env_remove("MODEL_ADAPTER")
            .env_remove("MODEL_ENDPOINT")
            .env_remove("MODEL_API_KEY")
            .env_remove("EMBEDDING_MODEL")
            .env("RUST_BACKTRACE", "0")
            .envs(if mode == "configured" {
                vec![("MODEL", "configured-model")]
            } else if mode == "custom" {
                vec![("MODEL_ADAPTER", "deliberately-invalid")]
            } else {
                vec![]
            })
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{mode}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
}
#[tokio::test]
async fn default_genai_environment_child() {
    let Ok(mode) = std::env::var("AGENT_TEST_GENAI_MODE") else {
        return;
    };
    let mut builder = TestBuilder::default();
    if mode == "custom" {
        builder = builder.plugin(ChatModelPlugin::new(LocalChat));
    }
    if mode == "disabled" {
        builder = builder.default_models(false);
    }
    let result = builder.build().await;
    match mode.as_str() {
        "missing" => assert!(
            result
                .err()
                .unwrap()
                .to_string()
                .contains("configure GenaiPlugin")
        ),
        "disabled" => assert!(matches!(
            result,
            Err(AgentError::MissingService("ChatModel"))
        )),
        "configured" => {
            let agent = result.unwrap();
            assert_eq!(
                agent.models(ModelOperation::Chat),
                vec![ModelRef::new("default", "configured-model")]
            );
            agent.close().await;
        }
        "custom" => {
            let agent = result.unwrap();
            assert_eq!(
                agent
                    .run(RunRequest::new("s", "hello"))
                    .await
                    .unwrap()
                    .output,
                "local"
            );
            agent.close().await;
        }
        _ => panic!("unknown test mode"),
    }
}
