use async_trait::async_trait;
use eliteforge_agent_capability_runtime::{
    BackendCallContext, BackendResource, BackendServer, RpcFault, TypedBackend,
};
use eliteforge_agent_core::{ToolCatalogEntry, ToolDescriptor, ToolOperationKind, ToolOutput};
use serde_json::{Value, json};
use std::sync::Arc;

const PROVIDER: &str = "__PACK_ID__.tools";
const TOOL_ID: &str = "__PACK_ID__.echo";

struct ToolLease;

#[async_trait]
impl BackendResource for ToolLease {
    async fn call(&self, _: &str, _: Value, _: BackendCallContext) -> Result<Value, RpcFault> {
        Err(RpcFault::new("unsupported resource operation"))
    }

    async fn close(&self) -> Result<(), RpcFault> {
        Ok(())
    }
}

fn catalog() -> Vec<ToolCatalogEntry> {
    vec![ToolCatalogEntry {
        descriptor: ToolDescriptor::new(
            TOOL_ID,
            "__TOOL_NAME__",
            "Echo the supplied message without side effects",
            json!({"type":"object","properties":{"message":{"type":"string"}},"required":["message"],"additionalProperties":false}),
        )
        .with_operation_kind(ToolOperationKind::Query),
        domain_code: Some("__PACK_ID__".into()),
        category: None,
    }]
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.as_slice() == ["catalog"] {
        println!("{}", serde_json::to_string(&catalog())?);
        return Ok(());
    }
    if !args.is_empty() {
        return Err("usage: capability-backend [catalog]".into());
    }
    let manifest = serde_json::from_str(include_str!("../../manifest.json"))?;
    let mut backend = TypedBackend::default();
    backend.method(PROVIDER, "lease", |_: Value, context| async move {
        let handle = context.register_resource(Arc::new(ToolLease))?;
        let tools: Vec<_> = catalog()
            .into_iter()
            .map(|entry| {
                json!({"descriptor":entry.descriptor,"domain_code":entry.domain_code,"category":entry.category,"exposed":true,"parallel_safe":true})
            })
            .collect();
        Ok(json!({"handle":handle,"tools":tools}))
    })?;
    backend.method(PROVIDER, "call", |params: Value, _| async move {
        if params["tool"] != TOOL_ID {
            return Err(RpcFault::new("unknown tool"));
        }
        let message = params["arguments"]["message"]
            .as_str()
            .ok_or_else(|| RpcFault::new("message must be a string"))?;
        Ok(ToolOutput::text(message))
    })?;
    BackendServer::new(manifest, Arc::new(backend))?
        .serve()
        .await?;
    Ok(())
}
