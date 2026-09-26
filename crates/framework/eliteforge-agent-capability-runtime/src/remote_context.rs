use crate::*;
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::STANDARD};
use eliteforge_agent_context_contracts::*;
use eliteforge_agent_core::ContextError;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_util::sync::CancellationToken;

#[derive(Serialize, Deserialize)]
struct ArtifactFailure {
    code: String,
    message: String,
}
fn artifact_fault(error: ArtifactError) -> RpcFault {
    let code = match &error {
        ArtifactError::Indeterminate(_) => "indeterminate",
        ArtifactError::InvalidId => "invalid_id",
        ArtifactError::NotFound => "not_found",
        ArtifactError::TooLarge => "too_large",
        ArtifactError::InvalidRange => "invalid_range",
        ArtifactError::Cancelled => "cancelled",
        ArtifactError::NotRegularFile => "not_regular_file",
        _ => "failed",
    };
    RpcFault {
        code: -32000,
        message: error.to_string(),
        data: serde_json::to_value(ArtifactFailure {
            code: code.into(),
            message: error.to_string(),
        })
        .ok(),
    }
}
fn artifact_error(error: RuntimeError) -> ArtifactError {
    if let RuntimeError::Indeterminate(message) = error {
        return ArtifactError::Indeterminate(message);
    }
    if let RuntimeError::Fault(fault) = &error
        && let Some(data) = &fault.data
        && let Ok(error) = serde_json::from_value::<ArtifactFailure>(data.clone())
    {
        return match error.code.as_str() {
            "indeterminate" => ArtifactError::Indeterminate(error.message),
            "invalid_id" => ArtifactError::InvalidId,
            "not_found" => ArtifactError::NotFound,
            "too_large" => ArtifactError::TooLarge,
            "invalid_range" => ArtifactError::InvalidRange,
            "cancelled" => ArtifactError::Cancelled,
            "not_regular_file" => ArtifactError::NotRegularFile,
            _ => ArtifactError::Worker(error.message),
        };
    }
    ArtifactError::Worker(error.to_string())
}
fn context_error(error: RuntimeError) -> ContextError {
    if let RuntimeError::Indeterminate(message) = error {
        return ContextError::Indeterminate(message);
    }
    if let RuntimeError::Fault(fault) = &error
        && let Some(data) = &fault.data
        && let Ok(error) = serde_json::from_value(data.clone())
    {
        return error;
    }
    ContextError::Other(error.to_string())
}
#[async_trait]
impl SummaryStore for RemoteImplementation {
    async fn load(&self, key: &SummaryKey) -> Result<Option<VersionedSummary>, ContextError> {
        serde_json::from_value(
            self.invoke("load", json!({"key":key}))
                .await
                .map_err(context_error)?,
        )
        .map_err(|e| ContextError::Other(e.to_string()))
    }
    async fn compare_exchange(
        &self,
        key: &SummaryKey,
        expected: Option<&str>,
        value: &ConversationSummary,
    ) -> Result<bool, ContextError> {
        serde_json::from_value(
            self.invoke(
                "compare_exchange",
                json!({"key":key,"expected":expected,"value":value}),
            )
            .await
            .map_err(context_error)?,
        )
        .map_err(|e| ContextError::Other(e.to_string()))
    }
    async fn delete(&self, key: &SummaryKey, expected: &str) -> Result<bool, ContextError> {
        serde_json::from_value(
            self.invoke("delete", json!({"key":key,"expected":expected}))
                .await
                .map_err(context_error)?,
        )
        .map_err(|e| ContextError::Other(e.to_string()))
    }
}
#[async_trait]
impl ArtifactStore for RemoteImplementation {
    fn max_artifact_bytes(&self) -> usize {
        self.declaration.metadata["max_artifact_bytes"]
            .as_u64()
            .unwrap_or(32 * 1024 * 1024)
            .min(32 * 1024 * 1024) as usize
    }
    fn max_read_bytes(&self) -> usize {
        self.declaration.metadata["max_read_bytes"]
            .as_u64()
            .unwrap_or(64 * 1024)
            .min(self.max_artifact_bytes() as u64) as usize
    }
    async fn put(
        &self,
        bytes: Vec<u8>,
        token: CancellationToken,
    ) -> Result<ArtifactMetadata, ArtifactError> {
        if bytes.len() > self.max_artifact_bytes() {
            return Err(ArtifactError::TooLarge);
        }
        let call = self.invoke("put", json!({"bytes":STANDARD.encode(bytes)}));
        let value = tokio::select! {biased;_=token.cancelled()=>return Err(ArtifactError::Cancelled),value=call=>value.map_err(artifact_error)?};
        serde_json::from_value(value).map_err(|e| ArtifactError::Worker(e.to_string()))
    }
    async fn read(
        &self,
        id: &ArtifactId,
        offset: u64,
        length: usize,
        token: CancellationToken,
    ) -> Result<ArtifactChunk, ArtifactError> {
        if length == 0 || length > self.max_read_bytes() {
            return Err(ArtifactError::InvalidRange);
        }
        let call = self.invoke("read", json!({"id":id,"offset":offset,"length":length}));
        let value = tokio::select! {biased;_=token.cancelled()=>return Err(ArtifactError::Cancelled),value=call=>value.map_err(artifact_error)?};
        let metadata: ArtifactMetadata = serde_json::from_value(value["metadata"].clone())
            .map_err(|e| ArtifactError::Worker(e.to_string()))?;
        let bytes = STANDARD
            .decode(value["bytes"].as_str().unwrap_or_default())
            .map_err(|e| ArtifactError::Worker(e.to_string()))?;
        if metadata.id != *id
            || bytes.len() > length
            || offset.saturating_add(bytes.len() as u64) > metadata.size_bytes
        {
            return Err(ArtifactError::InvalidRange);
        }
        Ok(ArtifactChunk {
            metadata,
            offset,
            bytes,
        })
    }
    async fn remove(&self, id: &ArtifactId, token: CancellationToken) -> Result<(), ArtifactError> {
        tokio::select! {biased;_=token.cancelled()=>Err(ArtifactError::Cancelled),value=self.invoke("remove",json!({"id":id}))=>value.map(|_|()).map_err(artifact_error)}
    }
}
impl TypedBackend {
    pub fn summary_store(
        &mut self,
        id: &str,
        store: std::sync::Arc<dyn SummaryStore>,
    ) -> RuntimeResult<()> {
        for operation in ["load", "compare_exchange", "delete"] {
            let store = store.clone();
            self.method(id, operation, move |params: serde_json::Value, _| {
                let store = store.clone();
                async move {
                    let key: SummaryKey = serde_json::from_value(params["key"].clone())
                        .map_err(|e| RpcFault::new(e.to_string()))?;
                    let result = match operation {
                        "load" => {
                            serde_json::to_value(store.load(&key).await.map_err(|e| RpcFault {
                                code: -32000,
                                message: e.to_string(),
                                data: serde_json::to_value(&e).ok(),
                            })?)
                        }
                        "compare_exchange" => {
                            let value = serde_json::from_value(params["value"].clone())
                                .map_err(|e| RpcFault::new(e.to_string()))?;
                            serde_json::to_value(
                                store
                                    .compare_exchange(&key, params["expected"].as_str(), &value)
                                    .await
                                    .map_err(|e| RpcFault {
                                        code: -32000,
                                        message: e.to_string(),
                                        data: serde_json::to_value(&e).ok(),
                                    })?,
                            )
                        }
                        _ => serde_json::to_value(
                            store
                                .delete(
                                    &key,
                                    params["expected"].as_str().ok_or_else(|| {
                                        RpcFault::new("expected version required")
                                    })?,
                                )
                                .await
                                .map_err(|e| RpcFault {
                                    code: -32000,
                                    message: e.to_string(),
                                    data: serde_json::to_value(&e).ok(),
                                })?,
                        ),
                    };
                    result.map_err(|e| RpcFault::new(e.to_string()))
                }
            })?;
        }
        Ok(())
    }
    pub fn artifact_store(
        &mut self,
        id: &str,
        store: std::sync::Arc<dyn ArtifactStore>,
    ) -> RuntimeResult<()> {
        for operation in ["put", "read", "remove"] {
            let store = store.clone();
            self.method(id,operation,move|params:serde_json::Value,_|{let store=store.clone();async move{
                let token=CancellationToken::new();
                match operation {
                    "put"=>{let bytes=STANDARD.decode(params["bytes"].as_str().unwrap_or_default()).map_err(|e|RpcFault::new(e.to_string()))?;serde_json::to_value(store.put(bytes,token).await.map_err(artifact_fault)?).map_err(|e|RpcFault::new(e.to_string()))},
                    "read"=>{let id=serde_json::from_value(params["id"].clone()).map_err(|e|RpcFault::new(e.to_string()))?;let offset=params["offset"].as_u64().ok_or_else(||RpcFault::new("offset required"))?;let length=usize::try_from(params["length"].as_u64().ok_or_else(||RpcFault::new("length required"))?).map_err(|e|RpcFault::new(e.to_string()))?;let chunk=store.read(&id,offset,length,token).await.map_err(artifact_fault)?;Ok(json!({"metadata":chunk.metadata,"bytes":STANDARD.encode(chunk.bytes)}))},
                    _=>{let id=serde_json::from_value(params["id"].clone()).map_err(|e|RpcFault::new(e.to_string()))?;store.remove(&id,token).await.map_err(artifact_fault)?;Ok(serde_json::Value::Null)}
                }
            }})?;
        }
        Ok(())
    }
}
