use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct InboundMcpChannelMessage {
    pub server_name: String,
    pub content: String,
    pub metadata: Option<serde_json::Value>,
    pub received_at: i64,
    pub trigger_turn: bool,
}
