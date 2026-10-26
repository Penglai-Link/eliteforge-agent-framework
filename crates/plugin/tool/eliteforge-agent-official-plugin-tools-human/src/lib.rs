//! Structured user questions. The host renders the payload and submits an answer;
//! no network request or waiting task is held open by this tool.
use async_trait::async_trait;
use eliteforge_agent_core::*;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AskUserInput {
    pub intro: String,
    pub questions: Vec<QuestionInput>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionInput {
    pub question: String,
    pub options: Vec<OptionInput>,
    #[serde(default = "enabled")]
    pub allow_free_text: bool,
    #[serde(default = "enabled")]
    pub allow_skip: bool,
}
fn enabled() -> bool {
    true
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OptionInput {
    pub label: String,
    #[serde(default)]
    pub description: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserQuestion {
    pub id: RunId,
    pub question: String,
    pub options: Vec<UserOption>,
    pub allow_free_text: bool,
    pub allow_skip: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserOption {
    pub id: RunId,
    pub label: String,
    pub description: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AskUserPresentation {
    pub kind: String,
    pub intro: String,
    pub questions: Vec<UserQuestion>,
}

pub struct AskUserTool;
impl AskUserTool {
    pub fn descriptor() -> ToolDescriptor {
        let text = |max| json!({"type":"string","minLength":1,"maxLength":max,"pattern":"\\S"});
        ToolDescriptor::new(
            "human.ask_user",
            "ask_user",
            "Ask the user 1-3 short questions with choices. This ends the current run. Use the answer to plan the next step.",
            json!({
                "type":"object","additionalProperties":false,"required":["intro","questions"],"properties":{
                    "intro":text(500),"questions":{"type":"array","minItems":1,"maxItems":3,"items":{
                        "type":"object","additionalProperties":false,"required":["question","options"],"properties":{
                            "question":text(500),"allow_free_text":{"type":"boolean"},"allow_skip":{"type":"boolean"},
                            "options":{"type":"array","minItems":2,"maxItems":5,"items":{
                                "type":"object","additionalProperties":false,"required":["label"],"properties":{
                                    "label":text(120),"description":{"anyOf":[text(240),{"type":"null"}]}
                                }
                            }}
                        }
                    }}
                }
            }),
        ).with_single_input()
    }
    pub fn binding() -> ToolBinding {
        ToolBinding::new(Self::descriptor(), Self)
    }
    pub fn request(input: AskUserInput) -> Result<InteractionRequest, AgentError> {
        let value = serde_json::to_value(&input)
            .map_err(|_| AgentError::Run("invalid questions".into()))?;
        if !compile_schema(&Self::descriptor().input_schema)?.is_valid(&value) {
            return Err(AgentError::InvalidConfiguration(
                "questions do not match ask_user limits".into(),
            ));
        }
        let questions: Vec<_> = input
            .questions
            .into_iter()
            .map(|q| UserQuestion {
                id: RunId::new_v4(),
                question: q.question,
                options: q
                    .options
                    .into_iter()
                    .map(|o| UserOption {
                        id: RunId::new_v4(),
                        label: o.label,
                        description: o.description,
                    })
                    .collect(),
                allow_free_text: q.allow_free_text,
                allow_skip: q.allow_skip,
            })
            .collect();
        let mut properties = serde_json::Map::new();
        for question in &questions {
            let mut choices = vec![
                json!({"type":"object","required":["option_id"],"additionalProperties":false,
                "properties":{"option_id":{"enum":question.options.iter().map(|o| o.id.to_string()).collect::<Vec<_>>()}}}),
            ];
            if question.allow_free_text {
                choices.push(json!({"type":"object","required":["text"],"additionalProperties":false,
                "properties":{"text":{"type":"string","minLength":1,"maxLength":4000,"pattern":"\\S"}}}));
            }
            if question.allow_skip {
                choices.push(json!({"type":"object","required":["skip"],"additionalProperties":false,"properties":{"skip":{"const":true}}}));
            }
            properties.insert(question.id.to_string(), json!({"oneOf":choices}));
        }
        let schema = json!({"type":"object","required":["answers"],"additionalProperties":false,"properties":{
            "answers":{"type":"object","additionalProperties":false,"required":properties.keys().collect::<Vec<_>>(),"properties":properties}
        }});
        let mut request = InteractionRequest::input(&input.intro, schema)?;
        request.presentation = Some(json!(AskUserPresentation {
            kind: "ask_user".into(),
            intro: input.intro,
            questions
        }));
        Ok(request)
    }
}
#[async_trait]
impl Tool for AskUserTool {
    async fn call(&self, arguments: Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        let input = serde_json::from_value(arguments)
            .map_err(|_| ToolError::Failed("invalid ask_user arguments".into()))?;
        let request = Self::request(input)
            .map_err(|_| ToolError::Failed("invalid ask_user questions".into()))?;
        let mut output = ToolOutput::text(request.prompt.text());
        output.structured = request.presentation.clone();
        Ok(output.request_input(request))
    }
}

pub struct HumanInteractionPlugin;
#[async_trait]
impl Plugin for HumanInteractionPlugin {
    fn id(&self) -> &str {
        "tools.human"
    }
    async fn install(&self, context: &mut PluginContext<'_>) -> Result<(), PluginError> {
        let provider: Arc<dyn ToolProvider> =
            Arc::new(StaticToolProvider::new(vec![AskUserTool::binding()]));
        context.contribute(ToolProviderService(provider.clone()))?;
        context.contribute_extension(provider)
    }
}
