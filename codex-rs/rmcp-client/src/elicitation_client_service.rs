use std::sync::Arc;

use rmcp::RoleClient;
use rmcp::model::ClientInfo;
use rmcp::model::ClientResult;
use rmcp::model::CustomResult;
use rmcp::model::ElicitationAction;
use rmcp::model::Meta;
use rmcp::model::RequestParamsMeta;
use rmcp::model::ServerNotification;
use rmcp::model::ServerRequest;
use rmcp::service::NotificationContext;
use rmcp::service::RequestContext;
use rmcp::service::Service;
use serde::Serialize;
use serde_json::Value;

use crate::logging_client_handler::LoggingClientHandler;
use crate::rmcp_client::Elicitation;
use crate::rmcp_client::ElicitationPauseState;
use crate::rmcp_client::ElicitationResponse;
use crate::rmcp_client::MCP_CHANNEL_NOTIFICATION_METHOD;
use crate::rmcp_client::McpChannelNotificationParams;
use crate::rmcp_client::SendElicitation;
use crate::rmcp_client::SendMcpChannelNotification;

const MCP_PROGRESS_TOKEN_META_KEY: &str = "progressToken";

#[derive(Clone)]
pub(crate) struct ElicitationClientService {
    handler: LoggingClientHandler,
    send_elicitation: Arc<SendElicitation>,
    send_mcp_channel_notification: Arc<SendMcpChannelNotification>,
    pause_state: ElicitationPauseState,
}

impl ElicitationClientService {
    pub(crate) fn new(
        client_info: ClientInfo,
        send_elicitation: SendElicitation,
        send_mcp_channel_notification: SendMcpChannelNotification,
        pause_state: ElicitationPauseState,
    ) -> Self {
        let send_elicitation = Arc::new(send_elicitation);
        Self {
            handler: LoggingClientHandler::new(
                client_info,
                clone_send_elicitation(Arc::clone(&send_elicitation)),
            ),
            send_elicitation,
            send_mcp_channel_notification: Arc::new(send_mcp_channel_notification),
            pause_state,
        }
    }

    async fn create_elicitation(
        &self,
        request: Elicitation,
        context: RequestContext<RoleClient>,
    ) -> Result<ElicitationResponse, rmcp::ErrorData> {
        let RequestContext { id, meta, .. } = context;
        let request = restore_context_meta(request, meta);
        let _pause = self.pause_state.enter();
        (self.send_elicitation)(id, request)
            .await
            .map_err(|err| rmcp::ErrorData::internal_error(err.to_string(), None))
    }

    async fn handle_mcp_channel_notification(&self, notification: &ServerNotification) -> bool {
        let ServerNotification::CustomNotification(notification) = notification else {
            return false;
        };
        if notification.method != MCP_CHANNEL_NOTIFICATION_METHOD {
            return false;
        }

        let params = match notification.params_as::<McpChannelNotificationParams>() {
            Ok(Some(params)) => params,
            Ok(None) => {
                tracing::warn!(
                    "MCP channel notification `{MCP_CHANNEL_NOTIFICATION_METHOD}` missing params"
                );
                return true;
            }
            Err(err) => {
                tracing::warn!(
                    "failed to parse MCP channel notification `{MCP_CHANNEL_NOTIFICATION_METHOD}` params: {err}"
                );
                return true;
            }
        };

        (self.send_mcp_channel_notification)(params).await;
        true
    }
}

fn clone_send_elicitation(send_elicitation: Arc<SendElicitation>) -> SendElicitation {
    Box::new(move |request_id, request| send_elicitation(request_id, request))
}

impl Service<RoleClient> for ElicitationClientService {
    async fn handle_request(
        &self,
        request: ServerRequest,
        context: RequestContext<RoleClient>,
    ) -> Result<ClientResult, rmcp::ErrorData> {
        match request {
            ServerRequest::CreateElicitationRequest(request) => {
                let response = self.create_elicitation(request.params, context).await?;
                // RMCP's typed CreateElicitationResult does not model result-level `_meta`.
                let result = elicitation_response_result(response)?;
                Ok(ClientResult::CustomResult(result))
            }
            request => {
                <LoggingClientHandler as Service<RoleClient>>::handle_request(
                    &self.handler,
                    request,
                    context,
                )
                .await
            }
        }
    }

    async fn handle_notification(
        &self,
        notification: ServerNotification,
        context: NotificationContext<RoleClient>,
    ) -> Result<(), rmcp::ErrorData> {
        if self.handle_mcp_channel_notification(&notification).await {
            return Ok(());
        }

        <LoggingClientHandler as Service<RoleClient>>::handle_notification(
            &self.handler,
            notification,
            context,
        )
        .await
    }

    fn get_info(&self) -> ClientInfo {
        <LoggingClientHandler as Service<RoleClient>>::get_info(&self.handler)
    }
}

fn restore_context_meta(mut request: Elicitation, mut context_meta: Meta) -> Elicitation {
    // RMCP lifts JSON-RPC `_meta` into RequestContext before invoking services.
    context_meta.remove(MCP_PROGRESS_TOKEN_META_KEY);
    if context_meta.is_empty() {
        return request;
    }

    request
        .meta_mut()
        .get_or_insert_with(Meta::new)
        .extend(context_meta);
    request
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateElicitationResultWithMeta {
    action: ElicitationAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<Value>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    meta: Option<Value>,
}

fn elicitation_response_result(
    response: ElicitationResponse,
) -> Result<CustomResult, rmcp::ErrorData> {
    let ElicitationResponse {
        action,
        content,
        meta,
    } = response;
    let result = CreateElicitationResultWithMeta {
        action,
        content,
        meta,
    };

    serde_json::to_value(result)
        .map(CustomResult)
        .map_err(|err| rmcp::ErrorData::internal_error(err.to_string(), None))
}

#[cfg(test)]
mod tests {
    use futures::FutureExt;
    use pretty_assertions::assert_eq;
    use rmcp::ServerHandler;
    use rmcp::model::BooleanSchema;
    use rmcp::model::CreateElicitationRequestParams;
    use rmcp::model::CustomNotification;
    use rmcp::model::ElicitationSchema;
    use rmcp::model::InitializeRequestParams;
    use rmcp::model::PrimitiveSchema;
    use rmcp::model::ServerCapabilities;
    use rmcp::model::ServerInfo;
    use rmcp::model::ServerNotification;
    use rmcp::service::ServiceExt;
    use serde_json::Value;
    use serde_json::json;
    use tokio::sync::mpsc;

    use super::*;

    #[test]
    fn restore_context_meta_adds_elicitation_meta_and_removes_progress_token() {
        let request = restore_context_meta(
            form_request(/*meta*/ None),
            meta(json!({
                "progressToken": "progress-token",
                "persist": ["session", "always"],
            })),
        );

        assert_eq!(
            request,
            form_request(Some(meta(json!({
                "persist": ["session", "always"],
            }))))
        );
    }

    #[test]
    fn elicitation_response_result_serializes_response_meta() {
        let result = rmcp::model::ClientResult::CustomResult(
            elicitation_response_result(ElicitationResponse {
                action: ElicitationAction::Accept,
                content: Some(json!({ "confirmed": true })),
                meta: Some(json!({ "persist": "always" })),
            })
            .expect("elicitation response should serialize"),
        );

        assert_eq!(
            serde_json::to_value(result).expect("client result should serialize"),
            json!({
                "action": "accept",
                "content": { "confirmed": true },
                "_meta": { "persist": "always" },
            })
        );
    }

    #[tokio::test]
    async fn mcp_channel_notification_calls_callback_with_parsed_params() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let service = service_with_channel_sender(Box::new(move |params| {
            let tx = tx.clone();
            async move {
                tx.send(params).expect("receiver should be open");
            }
            .boxed()
        }));

        let consumed = service
            .handle_mcp_channel_notification(&ServerNotification::CustomNotification(
                CustomNotification::new(
                    MCP_CHANNEL_NOTIFICATION_METHOD,
                    Some(json!({
                        "content": "hello",
                        "metadata": { "sender": "alice" },
                        "triggerTurn": false,
                    })),
                ),
            ))
            .await;

        assert_eq!(consumed, true);
        assert_eq!(
            rx.recv().await.expect("callback should send params"),
            McpChannelNotificationParams {
                content: "hello".to_string(),
                metadata: Some(json!({ "sender": "alice" })),
                trigger_turn: false,
            }
        );
        assert_eq!(rx.try_recv().is_err(), true);
    }

    #[tokio::test]
    async fn mcp_channel_notification_accepts_meta_alias_and_defaults_trigger_turn() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let service = service_with_channel_sender(Box::new(move |params| {
            let tx = tx.clone();
            async move {
                tx.send(params).expect("receiver should be open");
            }
            .boxed()
        }));

        let consumed = service
            .handle_mcp_channel_notification(&ServerNotification::CustomNotification(
                CustomNotification::new(
                    MCP_CHANNEL_NOTIFICATION_METHOD,
                    Some(json!({
                        "content": "hello",
                        "meta": { "channel": "support" },
                    })),
                ),
            ))
            .await;

        assert_eq!(consumed, true);
        assert_eq!(
            rx.recv().await.expect("callback should send params"),
            McpChannelNotificationParams {
                content: "hello".to_string(),
                metadata: Some(json!({ "channel": "support" })),
                trigger_turn: true,
            }
        );
    }

    #[tokio::test]
    async fn unknown_mcp_channel_custom_notification_is_not_consumed() {
        let service = service_with_channel_sender(Box::new(|_| {
            async {
                panic!("unknown notifications must not call channel callback");
            }
            .boxed()
        }));

        let consumed = service
            .handle_mcp_channel_notification(&ServerNotification::CustomNotification(
                CustomNotification::new("notifications/example", Some(json!({ "content": "hi" }))),
            ))
            .await;

        assert_eq!(consumed, false);
    }

    #[tokio::test]
    async fn malformed_mcp_channel_notification_is_consumed_without_callback() {
        let service = service_with_channel_sender(Box::new(|_| {
            async {
                panic!("malformed notifications must not call channel callback");
            }
            .boxed()
        }));

        let consumed = service
            .handle_mcp_channel_notification(&ServerNotification::CustomNotification(
                CustomNotification::new(
                    MCP_CHANNEL_NOTIFICATION_METHOD,
                    Some(json!({ "metadata": { "sender": "alice" } })),
                ),
            ))
            .await;

        assert_eq!(consumed, true);
    }

    #[tokio::test]
    async fn mcp_channel_notification_missing_params_is_consumed_without_callback() {
        let service = service_with_channel_sender(Box::new(|_| {
            async {
                panic!("missing params notifications must not call channel callback");
            }
            .boxed()
        }));

        let consumed = service
            .handle_mcp_channel_notification(&ServerNotification::CustomNotification(
                CustomNotification::new(MCP_CHANNEL_NOTIFICATION_METHOD, None),
            ))
            .await;

        assert_eq!(consumed, true);
    }

    #[tokio::test]
    async fn mcp_channel_notification_reaches_callback_through_rmcp_service() -> anyhow::Result<()>
    {
        let (server_transport, client_transport) = tokio::io::duplex(4096);
        tokio::spawn(async move {
            let server = ChannelNotificationServer.serve(server_transport).await?;
            server.waiting().await?;
            anyhow::Ok(())
        });

        let (tx, mut rx) = mpsc::unbounded_channel();
        let service = service_with_channel_sender(Box::new(move |params| {
            let tx = tx.clone();
            async move {
                tx.send(params).expect("receiver should be open");
            }
            .boxed()
        }));
        let client = service.serve(client_transport).await?;

        let params = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
            .await?
            .expect("callback should send params");
        client.cancel().await?;

        assert_eq!(
            params,
            McpChannelNotificationParams {
                content: "service hello".to_string(),
                metadata: Some(json!({ "source": "server" })),
                trigger_turn: true,
            }
        );
        Ok(())
    }

    fn form_request(meta: Option<Meta>) -> CreateElicitationRequestParams {
        CreateElicitationRequestParams::FormElicitationParams {
            meta,
            message: "Confirm?".to_string(),
            requested_schema: ElicitationSchema::builder()
                .required_property("confirmed", PrimitiveSchema::Boolean(BooleanSchema::new()))
                .build()
                .expect("schema should build"),
        }
    }

    fn meta(value: Value) -> Meta {
        let Value::Object(map) = value else {
            panic!("meta must be an object");
        };
        Meta(map)
    }

    fn service_with_channel_sender(
        send_mcp_channel_notification: SendMcpChannelNotification,
    ) -> ElicitationClientService {
        ElicitationClientService::new(
            form_client_info(),
            Box::new(|_, _| async { panic!("elicitation should not be called") }.boxed()),
            send_mcp_channel_notification,
            ElicitationPauseState::new(),
        )
    }

    fn form_client_info() -> ClientInfo {
        InitializeRequestParams {
            meta: None,
            protocol_version: rmcp::model::ProtocolVersion::V_2025_06_18,
            capabilities: Default::default(),
            client_info: rmcp::model::Implementation {
                name: "test-client".to_string(),
                version: "0.0.0".to_string(),
                title: None,
                description: None,
                icons: None,
                website_url: None,
            },
        }
    }

    struct ChannelNotificationServer;

    impl ServerHandler for ChannelNotificationServer {
        fn get_info(&self) -> ServerInfo {
            ServerInfo {
                capabilities: ServerCapabilities::default(),
                ..Default::default()
            }
        }

        async fn on_initialized(
            &self,
            context: rmcp::service::NotificationContext<rmcp::RoleServer>,
        ) {
            let peer = context.peer;
            tokio::spawn(async move {
                peer.send_notification(ServerNotification::CustomNotification(
                    CustomNotification::new(
                        MCP_CHANNEL_NOTIFICATION_METHOD,
                        Some(json!({
                            "content": "service hello",
                            "meta": { "source": "server" },
                        })),
                    ),
                ))
                .await
                .expect("send channel notification");
            });
        }
    }
}
