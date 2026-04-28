#!/usr/bin/env python3
# /// script
# requires-python = ">=3.10"
# dependencies = [
#   "slack_sdk>=3.27",
# ]
# ///
"""Slack-aware MCP server that forwards channel messages as Codex inbound channel notifications.

Run via uv (ephemeral env with deps auto-installed; no global pip install needed):

    uv run --script /path/to/slack_mcp_server.py

Reads two env vars:
- SLACK_BOT_TOKEN: Bot user OAuth token (xoxb-...). Used for the WebClient and to
  identify the bot's own user id so we can skip its own messages.
- SLACK_APP_TOKEN: App-level token with `connections:write` scope (xapp-...). Used
  by socket mode to receive events.

If the connecting client does not advertise the `codex/channel` capability
(e.g., a Codex sub-agent), this server skips opening the Slack socket-mode
connection but still exposes its tools so the client can post outbound
messages. See `mcp_channel_server.md` for the rationale behind not refusing
`initialize` outright.
"""

from __future__ import annotations

import json
import os
import sys
import threading
import urllib.request
from typing import Any

from slack_sdk import WebClient
from slack_sdk.socket_mode import SocketModeClient
from slack_sdk.socket_mode.request import SocketModeRequest
from slack_sdk.socket_mode.response import SocketModeResponse


PROTOCOL_VERSION = "2025-06-18"
CHANNEL_CAPABILITY = "codex/channel"
CHANNEL_NOTIFICATION_METHOD = "notifications/codex/channel"
TOOL_SEND_MESSAGE = "send_slack_message"
TOOL_API_CALL = "slack_api_call"
TOOL_DOWNLOAD_FILE = "slack_download_file"
DEFAULT_DOWNLOAD_MAX_BYTES = 5_000_000

_stdout_lock = threading.Lock()
_socket_client: SocketModeClient | None = None
_web_client: WebClient | None = None
_bot_user_id: str | None = None


def log(message: str) -> None:
    sys.stderr.write(f"[slack-mcp] {message}\n")
    sys.stderr.flush()


def send(message: dict[str, Any]) -> None:
    with _stdout_lock:
        sys.stdout.write(json.dumps(message, separators=(",", ":")) + "\n")
        sys.stdout.flush()


def send_response(request_id: Any, result: dict[str, Any]) -> None:
    send({"jsonrpc": "2.0", "id": request_id, "result": result})


def send_error(request_id: Any, code: int, message: str) -> None:
    send(
        {
            "jsonrpc": "2.0",
            "id": request_id,
            "error": {"code": code, "message": message},
        }
    )


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


def client_supports_channel(params: dict[str, Any]) -> bool:
    capabilities = params.get("capabilities") or {}
    if not isinstance(capabilities, dict):
        return False
    extensions = capabilities.get("extensions") or {}
    if not isinstance(extensions, dict):
        return False
    return CHANNEL_CAPABILITY in extensions


def initialize_result() -> dict[str, Any]:
    return {
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {
            "extensions": {CHANNEL_CAPABILITY: {}},
            "tools": {"listChanged": True},
        },
        "serverInfo": {
            "name": "codex-mcp-slack-example",
            "version": "0.1.0",
        },
    }


def tools_result() -> dict[str, Any]:
    return {
        "tools": [
            {
                "name": TOOL_SEND_MESSAGE,
                "description": (
                    "Send a Slack message as the bot. Use this to reply to "
                    "inbound Slack messages that arrive via "
                    "mcp_channel_message from this server. The notification's "
                    "metadata is the raw Slack event, so it carries blocks, "
                    "attachments, files, and any other Slack fields.\n\n"
                    "Recipe to reply to an inbound message:\n"
                    "  - channel  = inbound metadata.channel\n"
                    "  - thread_ts = inbound metadata.thread_ts if present, "
                    "else inbound metadata.ts (replies in-thread to the "
                    "message you received).\n"
                    "  - Omit thread_ts to post a fresh top-level message."
                ),
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "channel": {
                            "type": "string",
                            "description": (
                                "Slack channel ID (Cxxxx for public, Gxxxx "
                                "for private, Dxxxx for DM) or a #channel name."
                            ),
                        },
                        "text": {
                            "type": "string",
                            "description": "Message text. Plain text or Slack mrkdwn.",
                        },
                        "thread_ts": {
                            "type": "string",
                            "description": (
                                "Parent message timestamp. Set to reply "
                                "in-thread; omit for a top-level channel post."
                            ),
                        },
                    },
                    "required": ["channel", "text"],
                    "additionalProperties": False,
                },
            },
            {
                "name": TOOL_DOWNLOAD_FILE,
                "description": (
                    "Download a Slack file to a local path. Inbound "
                    "messages list attached files under metadata.files; "
                    "pass the entry's url_private as `url` and choose a "
                    "destination via `path`. File downloads aren't Web "
                    "API methods (they require an authenticated GET to "
                    f"files.slack.com), so use this rather than "
                    f"`{TOOL_API_CALL}`. After download, open the path "
                    "with `view_image` (for images) or shell tools (for "
                    "anything else). Default cap "
                    f"{DEFAULT_DOWNLOAD_MAX_BYTES} bytes; override with "
                    "max_bytes. Requires files:read on the bot token."
                ),
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "url": {
                            "type": "string",
                            "description": (
                                "url_private or url_private_download from "
                                "a Slack file object."
                            ),
                        },
                        "path": {
                            "type": "string",
                            "description": (
                                "Destination file path. The parent "
                                "directory must exist; an existing file "
                                "at this path will be overwritten."
                            ),
                        },
                        "max_bytes": {
                            "type": "integer",
                            "description": (
                                "Maximum bytes to write. `truncated: "
                                "true` is set if the file is larger. "
                                f"Default {DEFAULT_DOWNLOAD_MAX_BYTES}."
                            ),
                            "minimum": 1,
                        },
                    },
                    "required": ["url", "path"],
                    "additionalProperties": False,
                },
            },
            {
                "name": TOOL_API_CALL,
                "description": (
                    "Invoke any Slack Web API method "
                    "(https://api.slack.com/methods) — e.g. "
                    "conversations.history, reactions.add, files.upload, "
                    "chat.update. Use this for anything not covered by "
                    f"`{TOOL_SEND_MESSAGE}` or `{TOOL_DOWNLOAD_FILE}`. Pass "
                    "a small `limit` for paginated reads to keep responses "
                    "compact."
                ),
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "method": {
                            "type": "string",
                            "description": (
                                "Slack Web API method, e.g. "
                                "'conversations.history' or 'reactions.add'. "
                                "Dotted form is canonical; underscored "
                                "('conversations_history') also works."
                            ),
                        },
                        "arguments": {
                            "type": "object",
                            "description": (
                                "Arguments for the method as a JSON object, "
                                "e.g. {\"channel\": \"C123\", \"limit\": 10}. "
                                "Optional — pass {} for methods that take "
                                "no arguments."
                            ),
                            "additionalProperties": True,
                        },
                    },
                    "required": ["method"],
                    "additionalProperties": False,
                },
            },
        ]
    }


def handle_tool_call(request_id: Any, params: dict[str, Any]) -> None:
    name = params.get("name")
    if _web_client is None:
        send_error(request_id, -32603, "Slack client not initialized")
        return
    args = params.get("arguments") or {}
    if not isinstance(args, dict):
        send_error(request_id, -32602, "arguments must be an object")
        return

    if name == TOOL_SEND_MESSAGE:
        handle_send_message(request_id, args)
    elif name == TOOL_API_CALL:
        handle_api_call(request_id, args)
    elif name == TOOL_DOWNLOAD_FILE:
        handle_download_file(request_id, args)
    else:
        send_error(request_id, -32601, f"unknown tool: {name}")


def handle_send_message(request_id: Any, args: dict[str, Any]) -> None:
    channel = args.get("channel")
    text = args.get("text")
    thread_ts = args.get("thread_ts")
    if not isinstance(channel, str) or not isinstance(text, str):
        send_error(request_id, -32602, "channel and text must be strings")
        return

    kwargs: dict[str, Any] = {"channel": channel, "text": text}
    if isinstance(thread_ts, str):
        kwargs["thread_ts"] = thread_ts

    try:
        response = _web_client.chat_postMessage(**kwargs)
    except Exception as exc:  # noqa: BLE001
        send_error(request_id, -32000, f"Slack API error: {exc}")
        return

    send_response(
        request_id,
        {
            "content": [
                {
                    "type": "text",
                    "text": (
                        f"Posted to {response.get('channel')} as ts={response.get('ts')}"
                    ),
                }
            ],
            "structuredContent": {
                "channel": response.get("channel"),
                "ts": response.get("ts"),
            },
            "isError": False,
        },
    )


def handle_api_call(request_id: Any, args: dict[str, Any]) -> None:
    method = args.get("method")
    if not isinstance(method, str) or not method:
        send_error(request_id, -32602, "method must be a non-empty string")
        return

    method_args = args.get("arguments")
    if method_args is None:
        method_args = {}
    if not isinstance(method_args, dict):
        send_error(request_id, -32602, "arguments must be an object")
        return

    # Slack SDK exposes API methods as snake_case attributes (e.g.
    # `conversations_history`); accept the canonical dotted form too.
    # Reject leading-underscore lookups so we don't expose private helpers
    # like `_perform_urllib_http_request` to callers.
    attr = method.replace(".", "_")
    fn = getattr(_web_client, attr, None) if not attr.startswith("_") else None
    if not callable(fn):
        send_error(request_id, -32602, f"unknown Slack API method: {method}")
        return

    try:
        response = fn(**method_args)
    except Exception as exc:  # noqa: BLE001
        send_error(request_id, -32000, f"Slack API error: {exc}")
        return

    data = response.data if isinstance(response.data, dict) else {}
    ok = bool(data.get("ok"))
    summary = f"ok ({method})" if ok else f"error: {data.get('error', 'unknown')}"

    send_response(
        request_id,
        {
            "content": [{"type": "text", "text": summary}],
            "structuredContent": data,
            "isError": not ok,
        },
    )


def handle_download_file(request_id: Any, args: dict[str, Any]) -> None:
    url = args.get("url")
    if not isinstance(url, str) or not url:
        send_error(request_id, -32602, "url must be a non-empty string")
        return

    path = args.get("path")
    if not isinstance(path, str) or not path:
        send_error(request_id, -32602, "path must be a non-empty string")
        return

    max_bytes = args.get("max_bytes", DEFAULT_DOWNLOAD_MAX_BYTES)
    if not isinstance(max_bytes, int) or max_bytes <= 0:
        send_error(request_id, -32602, "max_bytes must be a positive integer")
        return

    # Slack file URLs (`url_private`, `url_private_download`) live on
    # files.slack.com and require the bot token as a Bearer header — they're
    # not Web API methods. Reuse the SDK's token but make the GET ourselves.
    request = urllib.request.Request(
        url,
        headers={"Authorization": f"Bearer {_web_client.token}"},
    )
    written = 0
    truncated = False
    try:
        with urllib.request.urlopen(request, timeout=30) as response, open(
            path, "wb"
        ) as out:
            content_type = response.headers.get(
                "Content-Type", "application/octet-stream"
            )
            remaining = max_bytes
            while remaining > 0:
                chunk = response.read(min(64 * 1024, remaining))
                if not chunk:
                    break
                out.write(chunk)
                written += len(chunk)
                remaining -= len(chunk)
            # Probe one more byte to detect truncation without buffering it.
            if response.read(1):
                truncated = True
    except Exception as exc:  # noqa: BLE001
        send_error(request_id, -32000, f"Slack file download error: {exc}")
        return

    summary = f"saved {written} bytes ({content_type}) to {path}"
    if truncated:
        summary += f"; truncated at max_bytes={max_bytes}"
    send_response(
        request_id,
        {
            "content": [{"type": "text", "text": summary}],
            "structuredContent": {
                "url": url,
                "path": path,
                "content_type": content_type,
                "size_bytes": written,
                "truncated": truncated,
            },
            "isError": False,
        },
    )


def handle_socket_event(client: SocketModeClient, req: SocketModeRequest) -> None:
    client.send_socket_mode_response(SocketModeResponse(envelope_id=req.envelope_id))

    if req.type != "events_api":
        return
    event = (req.payload or {}).get("event") or {}
    if event.get("type") != "message":
        return
    # Skip our bot's own posts and edit/delete events. Relax this set if the
    # model should react to edits/deletes.
    if event.get("subtype") in {"bot_message", "message_changed", "message_deleted"}:
        return
    user = event.get("user")
    if user and _bot_user_id and user == _bot_user_id:
        return

    text = event.get("text") or ""
    log(
        f"forwarding slack message channel={event.get('channel')} "
        f"user={user} ts={event.get('ts')}"
    )
    # Forward the raw Slack event as metadata so the model sees every field
    # Slack provided (blocks, attachments, files, edited markers, etc.).
    send_channel_notification(
        content=text or "(non-text Slack event; see metadata)",
        meta={"source": "slack", **event},
        trigger_turn=True,
    )


def init_slack_web_client() -> bool:
    global _web_client, _bot_user_id
    bot_token = os.environ.get("SLACK_BOT_TOKEN")
    if not bot_token:
        log("missing SLACK_BOT_TOKEN; send_slack_message will be unavailable")
        return False

    _web_client = WebClient(token=bot_token)
    try:
        auth = _web_client.auth_test()
        _bot_user_id = auth.get("user_id")
        log(
            f"authenticated as {auth.get('user')} ({_bot_user_id}) "
            f"in team {auth.get('team')}"
        )
    except Exception as exc:  # noqa: BLE001
        log(f"slack auth_test failed: {exc}")
        # Reset so the `_web_client is None` guard in handle_tool_call surfaces
        # a clear "not initialized" error instead of a misleading Slack API
        # error from a non-functional client.
        _web_client = None
        return False
    return True


def start_slack_socket_mode() -> None:
    global _socket_client
    if not init_slack_web_client():
        return
    app_token = os.environ.get("SLACK_APP_TOKEN")
    if not app_token:
        log("missing SLACK_APP_TOKEN; cannot start socket mode")
        return

    _socket_client = SocketModeClient(app_token=app_token, web_client=_web_client)
    _socket_client.socket_mode_request_listeners.append(handle_socket_event)
    try:
        _socket_client.connect()
        log("slack socket mode connected")
    except Exception as exc:  # noqa: BLE001
        log(f"slack socket mode connect failed: {exc}")


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
            if client_advertised_channel:
                threading.Thread(target=start_slack_socket_mode, daemon=True).start()
            else:
                # Sub-agent (or any client without codex/channel) — keep tools
                # available for outbound posting, but skip the inbound socket
                # since notifications would have nowhere to land.
                log("client did not advertise codex/channel; not starting Slack socket")
                init_slack_web_client()
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
