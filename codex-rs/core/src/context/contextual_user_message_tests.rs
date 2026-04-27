use super::*;
use crate::context::ContextualUserFragment;
use codex_protocol::items::HookPromptFragment;
use codex_protocol::items::build_hook_prompt_message;
use codex_protocol::mcp_channel::InboundMcpChannelMessage;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;
use serde_json::json;

#[test]
fn detects_environment_context_fragment() {
    assert!(is_contextual_user_fragment(&ContentItem::InputText {
        text: "<environment_context>\n<cwd>/tmp</cwd>\n</environment_context>".to_string(),
    }));
}

#[test]
fn detects_agents_instructions_fragment() {
    assert!(is_contextual_user_fragment(&ContentItem::InputText {
        text: "# AGENTS.md instructions for /tmp\n\n<INSTRUCTIONS>\nbody\n</INSTRUCTIONS>"
            .to_string(),
    }));
}

#[test]
fn detects_subagent_notification_fragment_case_insensitively() {
    assert!(SubagentNotification::matches_text(
        "<SUBAGENT_NOTIFICATION>{}</subagent_notification>"
    ));
}

#[test]
fn detects_mcp_channel_message_fragment_case_insensitively() {
    assert!(McpChannelMessage::matches_text(
        "<MCP_CHANNEL_MESSAGE>{}</mcp_channel_message>"
    ));
}

#[test]
fn recognizes_mcp_channel_message_as_contextual_and_memory_visible() {
    let text = McpChannelMessage {
        message: InboundMcpChannelMessage {
            server_name: "slack".to_string(),
            content: "Reply needed".to_string(),
            metadata: Some(json!({"thread": "abc"})),
            received_at: 1_726_000_001,
            trigger_turn: false,
        },
    }
    .render();
    let content_item = ContentItem::InputText { text };

    assert!(is_contextual_user_fragment(&content_item));
    assert!(!is_memory_excluded_contextual_user_fragment(&content_item));
}

#[test]
fn ignores_regular_user_text() {
    assert!(!is_contextual_user_fragment(&ContentItem::InputText {
        text: "hello".to_string(),
    }));
}

#[test]
fn classifies_memory_excluded_fragments() {
    let cases = [
        (
            "# AGENTS.md instructions for /tmp\n\n<INSTRUCTIONS>\nbody\n</INSTRUCTIONS>",
            true,
        ),
        (
            "<skill>\n<name>demo</name>\n<path>skills/demo/SKILL.md</path>\nbody\n</skill>",
            true,
        ),
        (
            "<environment_context>\n<cwd>/tmp</cwd>\n</environment_context>",
            false,
        ),
        (
            "<subagent_notification>{\"agent_id\":\"a\",\"status\":\"completed\"}</subagent_notification>",
            false,
        ),
        (
            "<mcp_channel_message>{\"server_name\":\"slack\",\"content\":\"hello\",\"metadata\":null,\"received_at\":1726000001}</mcp_channel_message>",
            false,
        ),
    ];

    for (text, expected) in cases {
        assert_eq!(
            is_memory_excluded_contextual_user_fragment(&ContentItem::InputText {
                text: text.to_string(),
            }),
            expected,
            "{text}",
        );
    }
}

#[test]
fn detects_hook_prompt_fragment_and_roundtrips_escaping() {
    let message = build_hook_prompt_message(&[HookPromptFragment::from_single_hook(
        r#"Retry with "waves" & <tides>"#,
        "hook-run-1",
    )])
    .expect("hook prompt message");

    let ResponseItem::Message { content, .. } = message else {
        panic!("expected hook prompt response item");
    };

    let [content_item] = content.as_slice() else {
        panic!("expected a single content item");
    };

    assert!(is_contextual_user_fragment(content_item));

    let ContentItem::InputText { text } = content_item else {
        panic!("expected input text content item");
    };
    let parsed = parse_visible_hook_prompt_message(/*id*/ None, content.as_slice())
        .expect("visible hook prompt");
    assert_eq!(
        parsed.fragments,
        vec![HookPromptFragment {
            text: r#"Retry with "waves" & <tides>"#.to_string(),
            hook_run_id: "hook-run-1".to_string(),
        }],
    );
    assert!(!text.contains("&quot;waves&quot; & <tides>"));
}
