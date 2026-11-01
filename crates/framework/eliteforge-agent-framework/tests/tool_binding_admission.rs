use async_trait::async_trait;
use eliteforge_agent_framework::*;
use eliteforge_agent_test_support::TestBuilder;
use serde_json::json;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

struct Endpoint(&'static str, Arc<AtomicUsize>);
#[async_trait]
impl Tool for Endpoint {
    async fn call(&self, _: serde_json::Value, _: &RunContext) -> Result<ToolOutput, ToolError> {
        self.1.fetch_add(1, Ordering::SeqCst);
        Ok(ToolOutput::text(self.0))
    }
}
struct Provider {
    closed: Arc<AtomicUsize>,
    native: Arc<AtomicUsize>,
    product: Arc<AtomicUsize>,
}
#[async_trait]
impl ToolProvider for Provider {
    async fn lease(&self, _: &RunContext) -> Result<ToolLease, AgentError> {
        let closed = self.closed.clone();
        Ok(ToolLease::new(vec![
            ToolBinding::new(
                ToolDescriptor::new("native.load", "load", "native", json!({"type":"object"})),
                Endpoint("native", self.native.clone()),
            )
            .with_category(ToolCategory::localized("fixture.native", "原生", "Native")),
            ToolBinding::new(
                ToolDescriptor::new("product.load", "load", "product", json!({"type":"object"})),
                Endpoint("product", self.product.clone()),
            )
            .with_category(ToolCategory::localized(
                "fixture.product",
                "产品",
                "Product",
            )),
        ])
        .with_cleanup(move || {
            closed.fetch_add(1, Ordering::SeqCst);
        }))
    }
}
struct Policy {
    pending: bool,
}
#[async_trait]
impl ToolPolicy for Policy {
    async fn admit_binding(
        &self,
        binding: &ToolBinding,
        _: &RunContext,
    ) -> Result<ToolBindingAdmission, AgentError> {
        assert_eq!(binding.source.plugin_id.as_deref(), Some("admission.tools"));
        assert!(
            binding
                .category
                .as_ref()
                .is_some_and(|category| category.code.starts_with("fixture."))
        );
        if self.pending {
            std::future::pending::<()>().await;
        }
        Ok(if binding.descriptor.id == "native.load" {
            ToolBindingAdmission::Exclude
        } else {
            ToolBindingAdmission::Allow
        })
    }
    async fn evaluate(
        &self,
        _: &ToolInvocationContext<'_>,
    ) -> Result<ToolPolicyDecision, AgentError> {
        Ok(ToolPolicyDecision::Allow)
    }
}
#[derive(Clone)]
struct Model(Arc<Mutex<Vec<ChatRequest>>>);
#[async_trait]
impl ChatModel for Model {
    async fn generate(
        &self,
        _: &str,
        request: ChatRequest,
        _: &dyn ChatStream,
    ) -> Result<ChatResponse, ModelError> {
        let mut requests = self.0.lock().unwrap();
        let first = requests.is_empty();
        requests.push(request);
        Ok(if first {
            ChatResponse::tool_calls(vec![ToolCall::new("call", "load", json!({}))])
        } else {
            ChatResponse::text("done")
        })
    }
}
async fn run(
    pending: bool,
) -> (
    Result<RunResult, AgentError>,
    usize,
    usize,
    usize,
    Arc<Mutex<Vec<ChatRequest>>>,
) {
    let closed = Arc::new(AtomicUsize::new(0));
    let native = Arc::new(AtomicUsize::new(0));
    let product = Arc::new(AtomicUsize::new(0));
    let requests = Arc::new(Mutex::new(vec![]));
    let agent = TestBuilder::default()
        .plugin(ChatModelPlugin::new(Model(requests.clone())))
        .plugin(ToolProviderPlugin::new(
            "admission.tools",
            Provider {
                closed: closed.clone(),
                native: native.clone(),
                product: product.clone(),
            },
        ))
        .plugin(ToolPolicyPlugin::new(
            "admission.policy",
            Policy { pending },
        ))
        .build()
        .await
        .unwrap();
    let mut request = RunRequest::new("admission", "execute");
    if pending {
        request.limits.run_timeout = Duration::from_millis(40);
        request.limits.cleanup_timeout = Duration::from_millis(20);
    }
    let result = agent.run(request).await;
    assert!(agent.framework().close().await.is_success());
    (
        result,
        closed.load(Ordering::SeqCst),
        native.load(Ordering::SeqCst),
        product.load(Ordering::SeqCst),
        requests,
    )
}
#[tokio::test]
async fn exclusion_selects_original_product_binding_before_duplicate_detection_and_retains_lease_cleanup()
 {
    let (result, closed, native, product, requests) = run(false).await;
    result.unwrap();
    assert_eq!((closed, native, product), (1, 0, 1));
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].tools.len(), 1);
    assert_eq!(requests[0].tools[0].id, "product.load");
    let tools_json = serde_json::to_string(&requests[0].tools).unwrap();
    assert!(!tools_json.contains("fixture.product"));
    assert!(!tools_json.contains("admission.tools"));
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|m| m.role == Role::Tool && m.content.text() == "product")
    );
}
#[tokio::test]
async fn pending_catalog_policy_is_deadline_bounded_and_keeps_original_lease_cleanup() {
    let (result, closed, native, product, requests) = run(true).await;
    assert!(result.is_err());
    assert_eq!((closed, native, product), (1, 0, 0));
    assert!(requests.lock().unwrap().is_empty());
}

#[test]
fn static_provider_exposes_categories_without_leasing_tools() {
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = StaticToolProvider::new(vec![
        ToolBinding::new(
            ToolDescriptor::new("example.one", "example_one", "one", json!({})),
            Endpoint("one", calls.clone()),
        )
        .with_category(ToolCategory::localized("example.read", "读取", "Read")),
        ToolBinding::new(
            ToolDescriptor::new("example.two", "example_two", "two", json!({})),
            Endpoint("two", calls.clone()),
        )
        .with_category(ToolCategory::localized("example.write", "写入", "Write")),
    ]);
    let catalog = provider.catalog_metadata().unwrap();
    assert_eq!(catalog.len(), 2);
    assert_eq!(catalog[0].category.as_ref().unwrap().code, "example.read");
    assert_eq!(catalog[1].category.as_ref().unwrap().code, "example.write");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}
