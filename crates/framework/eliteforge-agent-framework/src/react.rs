use crate::*;
use async_trait::async_trait;
use std::{collections::HashMap, sync::Arc};

pub struct ReactAgentLoop;
#[async_trait]
impl AgentLoop for ReactAgentLoop {
    fn requires_model(&self) -> bool {
        true
    }
    fn requires_state(&self) -> bool {
        true
    }
    fn supports_resume(&self) -> bool {
        true
    }
    fn requires_interactions(&self) -> bool {
        true
    }
    fn supports_tool_policy(&self) -> bool {
        true
    }
    fn cooperative_cancellation(&self) -> bool {
        true
    }
    async fn run(
        &self,
        request: &RunRequest,
        context: &RunContext,
        services: &AgentServices,
    ) -> Result<RunResult, AgentError> {
        let definition = if let Some(resume) = &request.resume {
            let checkpoint = services
                .state
                .as_ref()
                .ok_or(AgentError::MissingService("StateStore"))?
                .load(resume.run_id)
                .await?;
            if checkpoint.loop_id != "framework.react" {
                return Err(AgentError::InvalidConfiguration(
                    "resume requires a ReAct execution".into(),
                ));
            }
            let state: WorkflowState = serde_json::from_value(checkpoint.state).map_err(|_| {
                AgentError::State(StateError::Invalid("invalid ReAct checkpoint".into()))
            })?;
            state.definition
        } else {
            WorkflowDefinition {
                id: "framework.react".into(),
                version: 1,
                stages: vec![WorkflowStage::new(&request.model_stage)],
            }
        };
        let mut execution = crate::model_execution::ModelExecutionLoop::new(definition)?;
        execution.conversation = true;
        execution.run(request, context, services).await
    }
}

pub(crate) struct ToolSet {
    pub(crate) by_name: HashMap<String, ToolBinding>,
    id_to_name: HashMap<String, String>,
}

impl ToolSet {
    pub(crate) async fn load(
        providers: &[Arc<dyn ToolProvider>],
        context: &RunContext,
    ) -> Result<Self, AgentError> {
        let mut result = Self {
            by_name: HashMap::new(),
            id_to_name: HashMap::new(),
        };
        for provider in providers {
            let mut lease = provider.lease(context).await?;
            context.own_resources(std::mem::take(&mut lease.resources));
            for binding in &lease.tools {
                let descriptor = &binding.descriptor;
                if descriptor.id.trim().is_empty()
                    || descriptor.call_name.is_empty()
                    || !descriptor
                        .call_name
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                {
                    return Err(AgentError::ToolSetup("tool IDs must be nonempty and call names must use ASCII letters, digits, underscores or hyphens".into()));
                }
                if result.by_name.contains_key(&descriptor.call_name)
                    || result.id_to_name.contains_key(&descriptor.id)
                {
                    return Err(AgentError::ToolSetup(format!(
                        "duplicate tool ID or call name: {} / {}",
                        descriptor.id, descriptor.call_name
                    )));
                }
                result
                    .by_name
                    .insert(descriptor.call_name.clone(), binding.clone());
                result
                    .id_to_name
                    .insert(descriptor.id.clone(), descriptor.call_name.clone());
            }
        }
        Ok(result)
    }

    pub(crate) fn descriptors(&self) -> Vec<ToolDescriptor> {
        let mut tools: Vec<_> = self
            .by_name
            .values()
            .filter(|binding| binding.exposed)
            .map(|binding| binding.descriptor.clone())
            .collect();
        tools.sort_by(|left, right| left.call_name.cmp(&right.call_name));
        tools
    }

    pub(crate) fn set_exposure(&mut self, ids: &[String]) -> Result<(), String> {
        // Validate the whole update before mutating any visible binding.
        if ids.iter().any(|id| {
            !self
                .by_name
                .values()
                .any(|binding| &binding.descriptor.id == id)
        }) {
            return Err("Tool exposure contains an unauthorized ID".into());
        }
        for binding in self.by_name.values_mut() {
            binding.exposed = ids.contains(&binding.descriptor.id);
        }
        Ok(())
    }
    pub(crate) fn apply_exposure(&mut self, ids: &[String]) -> Result<(), String> {
        for id in ids {
            if !self.id_to_name.contains_key(id) {
                return Err(format!("tool {id} was not authorized by a provider"));
            }
        }
        for id in ids {
            let call_name = &self.id_to_name[id];
            self.by_name
                .get_mut(call_name)
                .expect("authorized tool binding")
                .exposed = true;
        }
        Ok(())
    }
}
