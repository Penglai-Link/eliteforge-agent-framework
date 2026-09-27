use std::sync::Arc;

use async_trait::async_trait;
use eliteforge_agent_core::{
    RunContext, SkillDiagnostic, Tool, ToolBinding, ToolDescriptor, ToolError, ToolOutput,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::SkillsRuntime;

pub(crate) fn bindings(
    runtime: Arc<SkillsRuntime>,
    skills_enabled: bool,
    diagnostics: bool,
) -> Vec<ToolBinding> {
    let mut tools = Vec::new();
    if skills_enabled {
        tools.extend([
            ToolBinding::new(
                load_descriptor(),
                SkillLoadTool {
                    runtime: runtime.clone(),
                },
            ),
            ToolBinding::new(
                read_resource_descriptor(),
                SkillReadResourceTool {
                    runtime: runtime.clone(),
                },
            ),
        ]);
    }
    if diagnostics {
        tools.push(ToolBinding::new(
            diagnostics_descriptor(),
            SkillDiagnosticsTool { runtime },
        ));
    }
    tools
}

fn load_descriptor() -> ToolDescriptor {
    ToolDescriptor::new(
        "skills.load",
        "skill_load",
        "Load one enabled skill's complete SKILL.md instructions and its resource file list. Load a skill before following its instructions; this never executes scripts.",
        json!({
            "type":"object", "additionalProperties":false, "required":["name"],
            "properties":{"name":{"type":"string", "minLength":1}}
        }),
    )
}

fn read_resource_descriptor() -> ToolDescriptor {
    ToolDescriptor::new(
        "skills.read_resource",
        "skill_read_resource",
        "Read one UTF-8 resource listed by an enabled skill. Use skill_load first to see available resource paths.",
        json!({
            "type":"object", "additionalProperties":false, "required":["name", "path"],
            "properties":{
                "name":{"type":"string", "minLength":1},
                "path":{"type":"string", "minLength":1}
            }
        }),
    )
}

fn diagnostics_descriptor() -> ToolDescriptor {
    ToolDescriptor::new(
        "skills.diagnostics",
        "skill_diagnostics",
        "Read non-fatal discovery problems for configured skill directories.",
        json!({"type":"object", "additionalProperties":false}),
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillNameInput {
    name: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillResourceInput {
    name: String,
    path: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SkillDiagnosticsOutput {
    diagnostics: Vec<SkillDiagnostic>,
}

struct SkillLoadTool {
    runtime: Arc<SkillsRuntime>,
}

#[async_trait]
impl Tool for SkillLoadTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let input = parse::<SkillNameInput>(arguments)?;
        let detail = self
            .runtime
            .load(context, &input.name)
            .await
            .map_err(tool_error)?;
        json_output(detail)
    }
}

struct SkillReadResourceTool {
    runtime: Arc<SkillsRuntime>,
}

#[async_trait]
impl Tool for SkillReadResourceTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        let input = parse::<SkillResourceInput>(arguments)?;
        let resource = self
            .runtime
            .read_resource(context, &input.name, &input.path)
            .await
            .map_err(tool_error)?;
        json_output(resource)
    }
}

struct SkillDiagnosticsTool {
    runtime: Arc<SkillsRuntime>,
}

#[async_trait]
impl Tool for SkillDiagnosticsTool {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        parse::<serde_json::Map<String, Value>>(arguments)?;
        let diagnostics = self
            .runtime
            .diagnostics(context)
            .await
            .map_err(tool_error)?;
        json_output(SkillDiagnosticsOutput { diagnostics })
    }
}

fn parse<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T, ToolError> {
    serde_json::from_value(value)
        .map_err(|error| ToolError::Failed(format!("tool input is invalid: {error}")))
}

fn tool_error(error: eliteforge_agent_core::AgentError) -> ToolError {
    ToolError::Failed(error.to_string())
}

fn json_output(value: impl Serialize) -> Result<ToolOutput, ToolError> {
    let structured = serde_json::to_value(value)
        .map_err(|error| ToolError::Failed(format!("tool output serialization failed: {error}")))?;
    let content = serde_json::to_string(&structured)
        .map_err(|error| ToolError::Failed(format!("tool output serialization failed: {error}")))?;
    let mut output = ToolOutput::text(content);
    output.structured = Some(structured);
    Ok(output)
}
