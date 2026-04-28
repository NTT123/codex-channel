#!/usr/bin/env python3
"""Small stdio MCP server that sends Codex inbound channel notifications.

This intentionally avoids third-party dependencies so it can be used directly
from a local Codex config. It implements enough MCP JSON-RPC over newline-
delimited stdio for manual testing:

- initialize
- notifications/initialized
- tools/list
- tools/call for a `send_channel_message` test tool
"""

from __future__ import annotations

import json
import os
import sys
import threading
import time
from typing import Any


PROTOCOL_VERSION = "2025-06-18"
CHANNEL_CAPABILITY = "codex/channel"
CHANNEL_NOTIFICATION_METHOD = "notifications/codex/channel"

DEFAULT_CONTENT = "Inbound message from the example MCP channel server"
DEFAULT_META = {
    "channel": "example",
    "messageId": "example-1",
    "sender": "mcp-channel-server",
}

_stdout_lock = threading.Lock()


def log(message: str) -> None:
    sys.stderr.write(f"[mcp-channel-example] {message}\n")
    sys.stderr.flush()


def env_bool(name: str, default: bool) -> bool:
    value = os.environ.get(name)
    if value is None:
        return default
    return value.strip().lower() not in {"0", "false", "no", "off"}


def env_float(name: str, default: float) -> float:
    value = os.environ.get(name)
    if value is None:
        return default
    try:
        return float(value)
    except ValueError:
        log(f"invalid {name}={value!r}; using {default}")
        return default


def env_json_object(name: str, default: dict[str, Any]) -> dict[str, Any]:
    value = os.environ.get(name)
    if value is None:
        return dict(default)
    try:
        parsed = json.loads(value)
    except json.JSONDecodeError as exc:
        log(f"invalid JSON in {name}: {exc}; using default")
        return dict(default)
    if not isinstance(parsed, dict):
        log(f"{name} must be a JSON object; using default")
        return dict(default)
    return parsed


def send(message: dict[str, Any]) -> None:
    with _stdout_lock:
        sys.stdout.write(json.dumps(message, separators=(",", ":")) + "\n")
        sys.stdout.flush()


def send_response(request_id: Any, result: dict[str, Any]) -> None:
    send({"jsonrpc": "2.0", "id": request_id, "result": result})


def send_error(request_id: Any, code: int, message: str) -> None:
    send({"jsonrpc": "2.0", "id": request_id, "error": {"code": code, "message": message}})


def send_channel_notification(
    *,
    content: str,
    meta: dict[str, Any],
    trigger_turn: bool,
) -> None:
    send(
        {
            "jsonrpc": "2.0",
            "method": CHANNEL_NOTIFICATION_METHOD,
            "params": {
                "content": content,
                "meta": meta,
                "triggerTurn": trigger_turn,
            },
        }
    )
    log(f"sent channel notification triggerTurn={trigger_turn}: {content!r}")


def configured_channel_message() -> tuple[str, dict[str, Any], bool]:
    content = os.environ.get("CODEX_MCP_CHANNEL_CONTENT", DEFAULT_CONTENT)
    meta = env_json_object("CODEX_MCP_CHANNEL_META", DEFAULT_META)
    trigger_turn = env_bool("CODEX_MCP_CHANNEL_TRIGGER_TURN", True)
    return content, meta, trigger_turn


def maybe_send_on_init(client_advertised_channel: bool) -> None:
    if not client_advertised_channel:
        # Sub-agent sessions do not advertise codex/channel because only the
        # root session subscribes. Stay running for tools/list etc., but skip
        # the push so we don't fire notifications nobody will read.
        log("client did not advertise codex/channel; skipping send-on-init")
        return
    if not env_bool("CODEX_MCP_CHANNEL_SEND_ON_INIT", True):
        log("send-on-init disabled")
        return

    delay_seconds = env_float("CODEX_MCP_CHANNEL_DELAY_SECONDS", 1.0)
    content, meta, trigger_turn = configured_channel_message()

    def worker() -> None:
        if delay_seconds > 0:
            time.sleep(delay_seconds)
        send_channel_notification(
            content=content,
            meta=meta,
            trigger_turn=trigger_turn,
        )

    threading.Thread(target=worker, daemon=True).start()


def initialize_result() -> dict[str, Any]:
    return {
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {
            "extensions": {CHANNEL_CAPABILITY: {}},
            "tools": {"listChanged": True},
        },
        "serverInfo": {
            "name": "codex-mcp-channel-example",
            "version": "0.1.0",
        },
    }


def client_supports_channel(params: dict[str, Any]) -> bool:
    capabilities = params.get("capabilities") or {}
    if not isinstance(capabilities, dict):
        return False
    extensions = capabilities.get("extensions") or {}
    if not isinstance(extensions, dict):
        return False
    return CHANNEL_CAPABILITY in extensions


def tools_result() -> dict[str, Any]:
    return {
        "tools": [
            {
                "name": "send_channel_message",
                "description": "Send a Codex inbound MCP channel notification.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "content": {
                            "type": "string",
                            "description": "Message content to send to Codex.",
                        },
                        "triggerTurn": {
                            "type": "boolean",
                            "description": "Whether the notification should wake an idle session.",
                        },
                        "meta": {
                            "type": "object",
                            "description": "Optional metadata included as the channel message metadata.",
                            "additionalProperties": True,
                        },
                    },
                    "required": ["content"],
                    "additionalProperties": False,
                },
            }
        ]
    }


def handle_tool_call(request_id: Any, params: dict[str, Any]) -> None:
    if params.get("name") != "send_channel_message":
        send_error(request_id, -32602, "unknown tool")
        return

    args = params.get("arguments") or {}
    if not isinstance(args, dict):
        send_error(request_id, -32602, "arguments must be an object")
        return

    content = args.get("content", DEFAULT_CONTENT)
    if not isinstance(content, str):
        send_error(request_id, -32602, "content must be a string")
        return

    meta = args.get("meta", dict(DEFAULT_META))
    if not isinstance(meta, dict):
        send_error(request_id, -32602, "meta must be an object")
        return

    trigger_turn = args.get("triggerTurn", True)
    if not isinstance(trigger_turn, bool):
        send_error(request_id, -32602, "triggerTurn must be a boolean")
        return

    send_channel_notification(
        content=content,
        meta=meta,
        trigger_turn=trigger_turn,
    )
    send_response(
        request_id,
        {
            "content": [
                {
                    "type": "text",
                    "text": "Sent Codex MCP channel notification.",
                }
            ],
            "structuredContent": {
                "content": content,
                "meta": meta,
                "triggerTurn": trigger_turn,
            },
            "isError": False,
        },
    )


def main() -> None:
    log("started")
    client_advertised_channel = False
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue

        try:
            message = json.loads(line)
        except json.JSONDecodeError as exc:
            log(f"invalid JSON-RPC message: {exc}")
            continue

        method = message.get("method")
        request_id = message.get("id")
        params = message.get("params") or {}

        if method == "initialize":
            client_advertised_channel = client_supports_channel(params)
            send_response(request_id, initialize_result())
        elif method == "notifications/initialized":
            maybe_send_on_init(client_advertised_channel)
        elif method == "tools/list":
            send_response(request_id, tools_result())
        elif method == "tools/call":
            if not isinstance(params, dict):
                send_error(request_id, -32602, "params must be an object")
            else:
                handle_tool_call(request_id, params)
        elif request_id is not None:
            send_error(request_id, -32601, "method not found")

    log("stdin closed")


if __name__ == "__main__":
    main()
