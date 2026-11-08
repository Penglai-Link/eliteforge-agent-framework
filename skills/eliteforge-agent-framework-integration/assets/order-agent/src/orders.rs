use async_trait::async_trait;
use eliteforge_agent_core::*;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};

pub struct ShopConfig {
    pub name: String,
    // 演示数据。生产环境改为宿主已经限定账号/租户范围的查询服务。
    pub orders: Arc<BTreeMap<String, String>>,
}

pub fn shop_pack(config: ShopConfig) -> CapabilityPackBuilder<ShopConfig> {
    CapabilityPackBuilder::new("shop", config)
}

pub struct ShopInstructions(String);

impl ShopInstructions {
    fn from_config(config: &ShopConfig) -> Self {
        Self(format!(
            "你是{}的客服。查询订单时必须调用 lookup_order，不猜测订单状态。用中文回答。",
            config.name,
        ))
    }
}

#[capability(
    pack = "shop", id = "shop.instructions",
    config = ShopConfig, factory = ShopInstructions::from_config
)]
#[async_trait]
impl ContextContributor for ShopInstructions {
    fn kind(&self) -> ContextSourceKind {
        ContextSourceKind::Instruction
    }
    fn block_id(&self) -> &str {
        "shop.instructions"
    }
    async fn contribute(&self, _: &ContextInput<'_>) -> Result<ContextBlock, AgentError> {
        Ok(ContextBlock::instructions(
            self.block_id(),
            vec![Message::system(self.0.clone())],
        ))
    }
}

struct LookupOrder(Arc<BTreeMap<String, String>>);

#[async_trait]
impl Tool for LookupOrder {
    async fn call(&self, arguments: Value, context: &RunContext) -> Result<ToolOutput, ToolError> {
        if context.is_cancelled() {
            return Err(ToolError::Failed("订单查询已取消".into()));
        }
        let id = arguments["order_id"]
            .as_str()
            .ok_or_else(|| ToolError::Failed("order_id 必须是字符串".into()))?;
        Ok(match self.0.get(id) {
            Some(status) => ToolOutput::text(format!("订单 {id}：{status}")),
            None => ToolOutput::error(format!("没有找到订单 {id}")),
        })
    }
}

#[capability_factory(pack = "shop", id = "shop.orders", config = ShopConfig)]
fn order_tools(config: &ShopConfig) -> ToolBindingPlugin {
    ToolBindingPlugin::new(
        "shop.orders",
        ToolBinding::new(
            ToolDescriptor::new(
                "shop.orders.lookup",
                "lookup_order",
                "根据订单号查询订单状态",
                json!({
                    "type": "object",
                    "properties": {"order_id": {"type": "string"}},
                    "required": ["order_id"],
                    "additionalProperties": false
                }),
            )
            .with_operation_kind(ToolOperationKind::Query),
            LookupOrder(config.orders.clone()),
        ),
    )
}
