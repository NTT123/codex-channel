# MCP Channel Example Server

This example stdio MCP server sends Codex inbound channel notifications with
`notifications/codex/channel`.

Add it to `~/.codex/config.toml`:

```toml
[mcp_servers.channel_example]
command = "python3"
args = ["/path/to/codex-rs/docs/examples/mcp_channel_server.py"]
startup_timeout_sec = 5
tool_timeout_sec = 5
```

On session startup, the server advertises `codex/channel` and sends:

```json
{
  "method": "notifications/codex/channel",
  "params": {
    "content": "Inbound message from the example MCP channel server",
    "meta": {
      "channel": "example",
      "messageId": "example-1",
      "sender": "mcp-channel-server"
    },
    "triggerTurn": true
  }
}
```

Environment overrides:

```toml
[mcp_servers.channel_example.env]
CODEX_MCP_CHANNEL_CONTENT = "hello from the test channel"
CODEX_MCP_CHANNEL_META = "{\"channel\":\"alerts\",\"messageId\":\"manual-1\"}"
CODEX_MCP_CHANNEL_TRIGGER_TURN = "true"
CODEX_MCP_CHANNEL_SEND_ON_INIT = "true"
CODEX_MCP_CHANNEL_DELAY_SECONDS = "1"
```

Set `CODEX_MCP_CHANNEL_TRIGGER_TURN = "false"` to enqueue the message without
waking an idle session. Set `CODEX_MCP_CHANNEL_SEND_ON_INIT = "false"` to stop
the automatic startup notification.

The server also exposes a `send_channel_message` MCP tool. If startup sending is
disabled, ask Codex to call that tool to send another test channel notification.

## Sub-agent behavior

Codex sub-agent sessions do not advertise the `codex/channel` client capability
— only the root session subscribes to inbound notifications. This example
inspects `params.capabilities.extensions` during `initialize` and, if
`codex/channel` is absent, simply skips the startup push (and any future
push triggered by `notifications/initialized`). The server stays connected so
sub-agents can still call tools like `send_channel_message` to push outbound
notifications back to the root.

Avoid the alternative of refusing `initialize` outright: it surfaces as a
startup failure that Codex logs every time a sub-agent spawns, and breaks
sub-agent setup entirely if a user marks the server `required = true`.
