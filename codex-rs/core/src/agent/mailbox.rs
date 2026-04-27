use crate::context::ContextualUserFragment;
use crate::context::McpChannelMessage;
use codex_protocol::mcp_channel::InboundMcpChannelMessage;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::InterAgentCommunication;
use std::collections::VecDeque;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use tokio::sync::mpsc;
use tokio::sync::watch;

#[cfg(test)]
use codex_protocol::AgentPath;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum MailboxItem {
    InterAgent(InterAgentCommunication),
    McpChannel(InboundMcpChannelMessage),
}

pub(crate) struct Mailbox {
    tx: mpsc::UnboundedSender<MailboxItem>,
    next_seq: AtomicU64,
    seq_tx: watch::Sender<u64>,
}

pub(crate) struct MailboxReceiver {
    rx: mpsc::UnboundedReceiver<MailboxItem>,
    pending_mails: VecDeque<MailboxItem>,
}

impl MailboxItem {
    pub(crate) fn trigger_turn(&self) -> bool {
        match self {
            MailboxItem::InterAgent(communication) => communication.trigger_turn,
            MailboxItem::McpChannel(message) => message.trigger_turn,
        }
    }

    pub(crate) fn to_response_input_item(&self) -> ResponseInputItem {
        match self {
            MailboxItem::InterAgent(communication) => communication.to_response_input_item(),
            MailboxItem::McpChannel(message) => ResponseInputItem::Message {
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: McpChannelMessage {
                        message: message.clone(),
                    }
                    .render(),
                }],
            },
        }
    }
}

impl From<InterAgentCommunication> for MailboxItem {
    fn from(value: InterAgentCommunication) -> Self {
        Self::InterAgent(value)
    }
}

impl From<InboundMcpChannelMessage> for MailboxItem {
    fn from(value: InboundMcpChannelMessage) -> Self {
        Self::McpChannel(value)
    }
}

impl Mailbox {
    pub(crate) fn new() -> (Self, MailboxReceiver) {
        let (tx, rx) = mpsc::unbounded_channel();
        let (seq_tx, _) = watch::channel(0);
        (
            Self {
                tx,
                next_seq: AtomicU64::new(0),
                seq_tx,
            },
            MailboxReceiver {
                rx,
                pending_mails: VecDeque::new(),
            },
        )
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.seq_tx.subscribe()
    }

    pub(crate) fn send(&self, item: impl Into<MailboxItem>) -> u64 {
        let seq = self.next_seq.fetch_add(1, Ordering::Relaxed) + 1;
        let _ = self.tx.send(item.into());
        self.seq_tx.send_replace(seq);
        seq
    }
}

impl MailboxReceiver {
    fn sync_pending_mails(&mut self) {
        while let Ok(mail) = self.rx.try_recv() {
            self.pending_mails.push_back(mail);
        }
    }

    pub(crate) fn has_pending(&mut self) -> bool {
        self.sync_pending_mails();
        !self.pending_mails.is_empty()
    }

    pub(crate) fn has_pending_trigger_turn(&mut self) -> bool {
        self.sync_pending_mails();
        self.pending_mails.iter().any(MailboxItem::trigger_turn)
    }

    pub(crate) fn drain(&mut self) -> Vec<MailboxItem> {
        self.sync_pending_mails();
        self.pending_mails.drain(..).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn make_mail(
        author: AgentPath,
        recipient: AgentPath,
        content: &str,
        trigger_turn: bool,
    ) -> InterAgentCommunication {
        InterAgentCommunication::new(
            author,
            recipient,
            Vec::new(),
            content.to_string(),
            trigger_turn,
        )
    }

    fn make_channel_message(content: &str, trigger_turn: bool) -> InboundMcpChannelMessage {
        InboundMcpChannelMessage {
            server_name: "linear".to_string(),
            content: content.to_string(),
            metadata: None,
            received_at: 1_726_000_001,
            trigger_turn,
        }
    }

    #[tokio::test]
    async fn mailbox_assigns_monotonic_sequence_numbers() {
        let (mailbox, _receiver) = Mailbox::new();
        let mut seq_rx = mailbox.subscribe();

        let seq_a = mailbox.send(make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "one",
            /*trigger_turn*/ false,
        ));
        let seq_b = mailbox.send(make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "two",
            /*trigger_turn*/ false,
        ));

        seq_rx.changed().await.expect("first seq update");
        assert_eq!(*seq_rx.borrow(), seq_b);
        assert_eq!(seq_a, 1);
        assert_eq!(seq_b, 2);
    }

    #[tokio::test]
    async fn mailbox_drains_in_delivery_order() {
        let (mailbox, mut receiver) = Mailbox::new();
        let mail_one = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "one",
            /*trigger_turn*/ false,
        );
        let mail_two = make_mail(
            AgentPath::try_from("/root/worker").expect("agent path"),
            AgentPath::root(),
            "two",
            /*trigger_turn*/ false,
        );

        mailbox.send(mail_one.clone());
        mailbox.send(mail_two.clone());

        assert_eq!(
            receiver.drain(),
            vec![
                MailboxItem::InterAgent(mail_one),
                MailboxItem::InterAgent(mail_two)
            ]
        );
        assert!(!receiver.has_pending());
    }

    #[tokio::test]
    async fn mailbox_drains_mixed_items_in_delivery_order() {
        let (mailbox, mut receiver) = Mailbox::new();
        let mail = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "inter-agent",
            /*trigger_turn*/ false,
        );
        let channel_message = make_channel_message("mcp channel", /*trigger_turn*/ false);

        mailbox.send(mail.clone());
        mailbox.send(channel_message.clone());

        assert_eq!(
            receiver.drain(),
            vec![
                MailboxItem::InterAgent(mail),
                MailboxItem::McpChannel(channel_message)
            ]
        );
    }

    #[tokio::test]
    async fn mailbox_tracks_pending_trigger_turn_mail() {
        let (mailbox, mut receiver) = Mailbox::new();

        mailbox.send(make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "queued",
            /*trigger_turn*/ false,
        ));
        assert!(!receiver.has_pending_trigger_turn());

        mailbox.send(make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "wake",
            /*trigger_turn*/ true,
        ));
        assert!(receiver.has_pending_trigger_turn());
    }

    #[tokio::test]
    async fn mailbox_tracks_pending_trigger_turn_channel_message() {
        let (mailbox, mut receiver) = Mailbox::new();

        mailbox.send(make_channel_message("queued", /*trigger_turn*/ false));
        assert!(!receiver.has_pending_trigger_turn());

        mailbox.send(make_channel_message("wake", /*trigger_turn*/ true));
        assert!(receiver.has_pending_trigger_turn());
    }

    #[test]
    fn inter_agent_item_converts_to_existing_assistant_envelope() {
        let mail = make_mail(
            AgentPath::root(),
            AgentPath::try_from("/root/worker").expect("agent path"),
            "hello",
            /*trigger_turn*/ false,
        );

        assert_eq!(
            MailboxItem::InterAgent(mail.clone()).to_response_input_item(),
            mail.to_response_input_item()
        );
    }

    #[test]
    fn mcp_channel_item_converts_to_user_input_text_fragment() {
        let channel_message = make_channel_message("mcp channel", /*trigger_turn*/ true);

        let item = MailboxItem::McpChannel(channel_message).to_response_input_item();

        let ResponseInputItem::Message { role, content } = item else {
            panic!("expected message input");
        };
        assert_eq!(role, "user");
        let [ContentItem::InputText { text }] = content.as_slice() else {
            panic!("expected one input_text item, got {content:#?}");
        };
        assert!(text.starts_with("<mcp_channel_message>"));
        assert!(text.ends_with("</mcp_channel_message>"));
        assert!(!text.contains("trigger_turn"));
        assert!(InterAgentCommunication::from_message_content(&content).is_none());
    }
}
