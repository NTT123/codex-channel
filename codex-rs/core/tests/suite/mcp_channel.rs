#![cfg(not(target_os = "windows"))]

use std::collections::HashMap;
use std::time::Duration;

use codex_config::types::McpServerConfig;
use codex_config::types::McpServerTransportConfig;
use codex_protocol::models::ContentItem;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::Op;
use core_test_support::responses;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tempfile::tempdir;

const SERVER_NAME: &str = "channel_server";
const START_MARKER: &str = "<mcp_channel_message>";
const END_MARKER: &str = "</mcp_channel_message>";
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_POLL_INTERVAL: Duration = Duration::from_millis(50);

fn insert_mcp_channel_server(config: &mut codex_core::config::Config, script_path: String) {
    let mut servers = config.mcp_servers.get().clone();
    servers.insert(
        SERVER_NAME.to_string(),
        McpServerConfig {
            transport: McpServerTransportConfig::Stdio {
                command: "python3".to_string(),
                args: vec![script_path],
                env: None,
                env_vars: Vec::new(),
                cwd: None,
            },
            experimental_environment: None,
            enabled: true,
            required: false,
            supports_parallel_tool_calls: false,
            disabled_reason: None,
            startup_timeout_sec: Some(STARTUP_TIMEOUT),
            tool_timeout_sec: Some(STARTUP_TIMEOUT),
            default_tools_approval_mode: None,
            enabled_tools: None,
            disabled_tools: None,
            scopes: None,
            oauth_resource: None,
            tools: HashMap::new(),
        },
    );
    if let Err(err) = config.mcp_servers.set(servers) {
        panic!("test MCP server config should be accepted: {err}");
    }
}

async fn wait_for_single_request(mock: &responses::ResponseMock) -> ResponsesRequest {
    let deadline = tokio::time::Instant::now() + REQUEST_TIMEOUT;
    loop {
        let requests = mock.requests();
        if requests.len() == 1 {
            let Some(request) = requests.into_iter().next() else {
                unreachable!("length checked above");
            };
            return request;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for one Responses request, got {}",
            requests.len()
        );
        tokio::time::sleep(REQUEST_POLL_INTERVAL).await;
    }
}

fn channel_message_input_text(request: &ResponsesRequest) -> String {
    let matching_messages: Vec<Value> = request
        .input()
        .into_iter()
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("message"))
        .filter(|item| item.get("role").and_then(Value::as_str) == Some("user"))
        .filter(|item| {
            item.get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|span| {
                    span.get("type").and_then(Value::as_str) == Some("input_text")
                        && span
                            .get("text")
                            .and_then(Value::as_str)
                            .is_some_and(|text| text.contains(START_MARKER))
                })
        })
        .collect();
    assert_eq!(matching_messages.len(), 1);

    let Some(content) = matching_messages[0]
        .get("content")
        .and_then(Value::as_array)
    else {
        panic!("message content should be an array");
    };
    assert_eq!(content.len(), 1);
    assert_eq!(
        content[0].get("type").and_then(Value::as_str),
        Some("input_text")
    );
    let Some(text) = content[0].get("text").and_then(Value::as_str) else {
        panic!("channel fragment should be input_text");
    };
    text.to_string()
}

fn parse_channel_payload(fragment: &str) -> anyhow::Result<Value> {
    assert!(fragment.starts_with(START_MARKER));
    assert!(fragment.ends_with(END_MARKER));

    let body = fragment
        .trim_start_matches(START_MARKER)
        .trim_end_matches(END_MARKER)
        .trim();
    Ok(serde_json::from_str(body)?)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mcp_channel_notification_from_session_server_reaches_model_request() -> anyhow::Result<()>
{
    let temp = tempdir()?;
    let server_script_path = temp.path().join("mcp_channel_stdio_server.py");
    std::fs::write(&server_script_path, MCP_CHANNEL_STDIO_SERVER_SCRIPT)?;

    let server = responses::start_mock_server().await;
    let response_mock = mount_sse_once(
        &server,
        responses::sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;

    let script_path = server_script_path.to_string_lossy().into_owned();
    let mut builder = test_codex().with_config(move |config| {
        insert_mcp_channel_server(config, script_path);
    });
    let test = builder.build(&server).await?;

    wait_for_event(&test.codex, |event| {
        matches!(event, codex_protocol::protocol::EventMsg::TurnComplete(_))
    })
    .await;

    let request = wait_for_single_request(&response_mock).await;
    let fragment = channel_message_input_text(&request);
    let payload = parse_channel_payload(&fragment)?;

    assert_eq!(payload["server_name"], SERVER_NAME);
    assert_eq!(payload["content"], "wake from session stdio server");
    assert_eq!(
        payload["metadata"],
        json!({
            "channel": "alerts",
            "messageId": "msg-1",
            "sender": "stdio-test",
        })
    );
    assert!(payload["received_at"].as_i64().is_some());
    assert_eq!(payload.get("trigger_turn"), None);
    assert_eq!(payload.get("triggerTurn"), None);
    assert!(
        InterAgentCommunication::from_message_content(&[ContentItem::InputText { text: fragment }])
            .is_none(),
        "MCP channel fragments must not parse as inter-agent envelopes"
    );
    assert!(
        !request
            .input()
            .iter()
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("message"))
            .filter(|item| item.get("role").and_then(Value::as_str) == Some("assistant"))
            .filter_map(|item| item.get("content").and_then(Value::as_array))
            .flatten()
            .filter_map(|span| span.get("text").and_then(Value::as_str))
            .any(|text| text.contains(START_MARKER)),
        "MCP channel message must be user-role context, not assistant output"
    );

    test.codex.submit(Op::Shutdown {}).await?;
    Ok(())
}

const MCP_CHANNEL_STDIO_SERVER_SCRIPT: &str = r#"
import json
import sys


def send(message):
    sys.stdout.write(json.dumps(message, separators=(",", ":")) + "\n")
    sys.stdout.flush()


for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    request_id = message.get("id")
    if method == "initialize":
        send({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {
                "protocolVersion": "2025-06-18",
                "capabilities": {
                    "extensions": {"codex/channel": {}},
                    "tools": {"listChanged": True}
                },
                "serverInfo": {
                    "name": "mcp-channel-session-test",
                    "version": "0.0.0"
                }
            }
        })
    elif method == "notifications/initialized":
        send({
            "jsonrpc": "2.0",
            "method": "notifications/codex/channel",
            "params": {
                "content": "wake from session stdio server",
                "meta": {
                    "channel": "alerts",
                    "messageId": "msg-1",
                    "sender": "stdio-test"
                },
                "triggerTurn": True
            }
        })
    elif method == "tools/list":
        send({
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {
                "tools": []
            }
        })
    elif request_id is not None:
        send({
            "jsonrpc": "2.0",
            "id": request_id,
            "error": {
                "code": -32601,
                "message": "method not found"
            }
        })
"#;
