use codex_protocol::mcp_channel::InboundMcpChannelMessage;

use super::ContextualUserFragment;

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct McpChannelMessage {
    pub(crate) message: InboundMcpChannelMessage,
}

impl ContextualUserFragment for McpChannelMessage {
    const ROLE: &'static str = "user";
    const START_MARKER: &'static str = "<mcp_channel_message>";
    const END_MARKER: &'static str = "</mcp_channel_message>";

    fn body(&self) -> String {
        format!(
            "\n{}\n",
            serde_json::json!({
                "server_name": &self.message.server_name,
                "content": &self.message.content,
                "metadata": &self.message.metadata,
                "received_at": self.message.received_at,
            })
        )
    }
}

#[cfg(test)]
mod tests {
    use codex_protocol::models::ContentItem;
    use codex_protocol::models::ResponseItem;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;

    fn channel_message() -> InboundMcpChannelMessage {
        InboundMcpChannelMessage {
            server_name: "linear".to_string(),
            content: "New message in triage".to_string(),
            metadata: Some(json!({
                "channel": "triage",
                "messageId": "msg-1",
            })),
            received_at: 1_726_000_000,
            trigger_turn: true,
        }
    }

    #[test]
    fn renders_model_visible_channel_payload_without_trigger_turn() {
        let rendered = McpChannelMessage {
            message: channel_message(),
        }
        .render();

        assert!(rendered.starts_with("<mcp_channel_message>\n"));
        assert!(rendered.ends_with("\n</mcp_channel_message>"));
        assert!(!rendered.contains("trigger_turn"));

        let json_body = rendered
            .trim_start_matches("<mcp_channel_message>")
            .trim_end_matches("</mcp_channel_message>")
            .trim();
        let parsed: serde_json::Value = serde_json::from_str(json_body).expect("valid JSON");
        assert_eq!(
            parsed,
            json!({
                "server_name": "linear",
                "content": "New message in triage",
                "metadata": {
                    "channel": "triage",
                    "messageId": "msg-1",
                },
                "received_at": 1_726_000_000,
            })
        );
    }

    #[test]
    fn renders_as_user_role_input_text() {
        let item: ResponseItem = ContextualUserFragment::into(McpChannelMessage {
            message: channel_message(),
        });

        assert_eq!(
            item,
            ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: McpChannelMessage {
                        message: channel_message(),
                    }
                    .render(),
                }],
                phase: None,
            }
        );
    }
}
