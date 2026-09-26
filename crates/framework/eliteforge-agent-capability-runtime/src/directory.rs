use crate::*;
use async_trait::async_trait;
use eliteforge_agent_core::*;
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};
tokio::task_local! { static DEPTH:usize; }
pub struct RemoteExtensionDirectory(pub BTreeMap<String, RemoteImplementation>);
impl RemoteExtensionDirectory {
    pub fn new(members: impl IntoIterator<Item = RemoteImplementation>) -> Arc<Self> {
        Arc::new(Self(
            members
                .into_iter()
                .map(|m| (m.declaration.id.clone(), m))
                .collect(),
        ))
    }
}
#[async_trait]
impl ExtensionRouter for RemoteExtensionDirectory {
    async fn call(
        &self,
        source: &str,
        target: &str,
        operation: &str,
        params: Value,
        run: &RunContext,
        services: &AgentServices,
    ) -> Result<Value, AgentError> {
        run.check_cancelled()?;
        let source = self.0.get(source).ok_or_else(|| {
            AgentError::InvalidConfiguration("unknown calling implementation".into())
        })?;
        let target = self.0.get(target).ok_or_else(|| {
            AgentError::InvalidConfiguration("target implementation not selected".into())
        })?;
        if !source
            .declaration
            .dependencies
            .contains(&target.declaration.id)
        {
            return Err(AgentError::InvalidConfiguration(
                "calling implementation did not declare target dependency".into(),
            ));
        }
        if source.instance.pack().manifest.id != target.instance.pack().manifest.id
            && !source
                .instance
                .pack()
                .manifest
                .dependencies
                .iter()
                .any(|dependency| dependency.id == target.instance.pack().manifest.id)
        {
            return Err(AgentError::InvalidConfiguration(
                "calling package did not declare target package dependency".into(),
            ));
        }
        if target.declaration.extension.starts_with("framework.") {
            return Err(AgentError::InvalidConfiguration(
                "Framework contracts must be called through their policy-enforcing Host API".into(),
            ));
        }
        let depth = DEPTH.try_with(|depth| *depth).unwrap_or(0);
        if depth >= 16 {
            return Err(AgentError::Run(
                "nested extension call limit exceeded".into(),
            ));
        }
        DEPTH
            .scope(depth + 1, async {
                let mut host = crate::host::RunHost::new(
                    run,
                    Some(services),
                    &target.instance.pack().manifest.permissions,
                    &target.declaration.id,
                );
                target
                    .invoke_scoped(operation, params, &mut host)
                    .await
                    .map_err(|e| match e {
                        RuntimeError::Indeterminate(message) => AgentError::Indeterminate(message),
                        error => AgentError::Run(error.to_string()),
                    })
            })
            .await
    }
}
