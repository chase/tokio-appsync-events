use std::marker::PhantomData;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use aws_config::SdkConfig;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use dashmap::DashMap;
use futures_util::sink::SinkExt;
use futures_util::stream::Stream;
use http::{HeaderName, HeaderValue, Uri};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::json;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::StreamExt;
use tokio_stream::wrappers::ReceiverStream;
use tokio_websockets::{ClientBuilder, MaybeTlsStream, Message, WebSocketStream};
use uuid::Uuid;

use crate::auth::AuthType;
use crate::error::{Error, Result};
use crate::message::MessagePayload;
use crate::message::{ConnectionPayload, SubscriptionMessage, SubscriptionPayload};

/// Default WebSocket protocol for AppSync Events API
const WS_PROTOCOL_NAME: &str = "aws-appsync-event-ws";

const MINUTES: u64 = 60;

/// Connection init timeout
const CONNECTION_INIT_TIMEOUT: Duration = Duration::from_secs(10);

/// Default keep alive timeout
const DEFAULT_KEEP_ALIVE_TIMEOUT: Duration = Duration::from_secs(5 * MINUTES);

/// Default keep alive heartbeat interval in milliseconds
const DEFAULT_KEEP_ALIVE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

/// Maximum delay for retries
const MAX_DELAY: Duration = Duration::from_secs(5);

/// WebSocket client type
type ClientSocket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

/// Subscription to an AppSync event channel
pub struct Subscription<T: DeserializeOwned + Unpin> {
    id: String,
    unsub_sender: mpsc::Sender<String>,
    receiver: ReceiverStream<String>,
    _marker: PhantomData<T>,
}

impl<T: DeserializeOwned + Unpin> Subscription<T> {
    /// Create a new subscription with the given ID and unsubscribe sender
    fn new(
        id: String,
        unsub_sender: mpsc::Sender<String>,
        event_receiver: mpsc::Receiver<String>,
    ) -> Self {
        Self {
            id,
            unsub_sender,
            receiver: ReceiverStream::new(event_receiver),
            _marker: PhantomData,
        }
    }

    /// Get the subscription ID
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl<T: DeserializeOwned + Unpin> Drop for Subscription<T> {
    fn drop(&mut self) {
        self.receiver.close();

        // When the subscription is dropped, send the unsubscribe message without blocking
        let id = self.id.clone();
        let unsub_sender = self.unsub_sender.clone();

        tokio::spawn(async move {
            let _ = unsub_sender.send(id).await;
        });
    }
}

impl<T: DeserializeOwned + Unpin> Stream for Subscription<T> {
    type Item = T;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match core::task::ready!(Pin::new(&mut self.receiver).poll_next(cx)) {
                Some(payload) => {
                    if let Ok(item) = serde_json::from_str::<T>(&payload) {
                        return Poll::Ready(Some(item));
                    }
                }
                None => return Poll::Ready(None),
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, self.receiver.size_hint().1)
    }
}

/// Connection state of the WebSocket
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum ConnectionState {
    /// Initial state
    Initial,

    /// Connecting to the WebSocket
    Connecting,

    /// Connected and ready to send/receive messages
    Connected,

    /// Connection disrupted, attempting to reconnect
    Disrupted,

    /// Disconnected
    Disconnected,

    /// Unrecoverable error
    UnrecoverableError,
}

/// Command sent to the WebSocket manager
enum WebSocketCommand {
    /// Subscribe to a channel
    Subscribe {
        id: String,
        channel: String,
        result_sender: oneshot::Sender<Result<mpsc::Receiver<String>>>,
    },

    /// Publish to a channel
    Publish {
        channel: String,
        event: String,
        result_sender: oneshot::Sender<Result<()>>,
    },

    /// Unsubscribe from a channel
    Unsubscribe { id: String },

    /// Close the connection
    Close,
}

struct SocketSubscription {
    channel: String,
    sender: mpsc::Sender<String>,
    ack_sender: Option<oneshot::Sender<Result<()>>>,
}

/// Internal state of the WebSocket manager
struct WebSocketState {
    /// Connection state
    connection_state: AtomicU8,

    /// Last keep alive timestamp
    keep_alive_timestamp: RwLock<Instant>,

    /// Keep alive timeout
    keep_alive_timeout: RwLock<Duration>,

    /// Subscriptions
    subscriptions: DashMap<String, SocketSubscription>,
}

impl WebSocketState {
    /// Create a new WebSocket state
    fn new() -> Self {
        Self {
            connection_state: AtomicU8::new(ConnectionState::Initial as u8),
            keep_alive_timestamp: RwLock::new(Instant::now()),
            keep_alive_timeout: RwLock::new(DEFAULT_KEEP_ALIVE_TIMEOUT),
            subscriptions: DashMap::new(),
        }
    }

    fn has_connection_state(&self, state: ConnectionState) -> bool {
        self.connection_state.load(Ordering::Acquire) == state as u8
    }

    fn has_one_of_connection_states(&self, states: &[ConnectionState]) -> bool {
        let current_state = self.connection_state.load(Ordering::Acquire);
        states.iter().any(|s| current_state == *s as u8)
    }

    fn set_connection_state(&self, state: ConnectionState) {
        self.connection_state.store(state as u8, Ordering::Release);
    }

    fn set_keep_alive_timeout(&self, timeout: Duration) {
        if let Ok(mut guard) = self.keep_alive_timeout.write() {
            *guard = timeout;
        }
    }

    fn timed_out(&self) -> bool {
        match self.keep_alive_timestamp.read() {
            Ok(guard) => {
                let timeout = self
                    .keep_alive_timeout
                    .read()
                    .map_or(DEFAULT_KEEP_ALIVE_TIMEOUT, |g| *g);
                guard.elapsed() > timeout
            }
            Err(_) => false,
        }
    }

    fn update_keep_alive_timestamp(&self) {
        if let Ok(mut guard) = self.keep_alive_timestamp.write() {
            *guard = Instant::now()
        }
    }
}

/// Builder for the AppSync Events client
pub struct AppSyncEventsClientBuilder<'a> {
    /// AppSync app ID
    app_id: String,

    /// AWS SDK config
    config: &'a SdkConfig,

    /// Region
    region: &'a str,

    /// Authentication type
    auth_type: Option<AuthType<'a>>,
}

impl<'a> AppSyncEventsClientBuilder<'a> {
    /// Create a new builder with the given endpoint and region
    pub fn new(app_id: impl Into<String>, config: &'a SdkConfig) -> Self {
        let region = config.region().map_or("us-east-1", |r| r.as_ref());
        Self {
            app_id: app_id.into(),
            config,
            auth_type: None,
            region,
        }
    }

    /// Set the IAM authentication provider
    pub async fn with_iam_auth(mut self) -> Result<AppSyncEventsClient<'a>> {
        self.auth_type = Some(
            AuthType::new_iam(self.app_id.clone(), self.region.to_string(), self.config).await,
        );
        self.build()
    }

    /// Set the Lambda authentication provider
    pub fn with_lambda_auth(mut self, token: impl Into<String>) -> Result<AppSyncEventsClient<'a>> {
        self.auth_type = Some(AuthType::new_lambda(token.into()));
        self.build()
    }

    /// Set the API Key authentication provider
    pub fn with_api_key_auth(mut self, key: impl Into<String>) -> Result<AppSyncEventsClient<'a>> {
        self.auth_type = Some(AuthType::ApiKey {
            app_id: self.app_id.clone(),
            region: self.region.to_string(),
            key: key.into(),
        });
        self.build()
    }

    /// Build the client
    fn build(self) -> Result<AppSyncEventsClient<'a>> {
        let auth_type = self.auth_type.ok_or_else(|| {
            Error::Authentication("Authentication provider is required".to_string())
        })?;

        Ok(AppSyncEventsClient {
            realtime_endpoint: crate::url::events_realtime(self.app_id.as_str(), self.region),
            auth_type,
            cmd_sender: None,
            allow_no_subscriptions: true,
        })
    }
}

/// Client for AWS AppSync Real-Time Events
pub struct AppSyncEventsClient<'a> {
    /// Realtime endpoint
    realtime_endpoint: String,

    /// Authentication provider
    auth_type: AuthType<'a>,

    /// Command sender to the WebSocket manager
    cmd_sender: Option<mpsc::Sender<WebSocketCommand>>,

    /// Whether to allow no active subscriptions
    allow_no_subscriptions: bool,
}

/// WebSocket manager loop
async fn websocket_manager_loop(
    ws_endpoint: String,
    auth_type: AuthType<'_>,
    mut cmd_receiver: mpsc::Receiver<WebSocketCommand>,
    allow_no_subscriptions: bool,
    initial_connection: ClientSocket,
    state: Arc<WebSocketState>,
) {
    // Jitter factor for exponential backoff
    const JITTER_FACTOR: u64 = 100;

    let mut websocket: Option<ClientSocket> = Some(initial_connection);
    let mut reconnect_attempts = 0;

    if websocket.is_some() {
        state.set_connection_state(ConnectionState::Connected);
        state.update_keep_alive_timestamp();
    }

    let keep_alive_state = state.clone();

    // Keep-alive monitor
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(DEFAULT_KEEP_ALIVE_HEARTBEAT_INTERVAL).await;

            if keep_alive_state.has_connection_state(ConnectionState::Connected)
                && keep_alive_state.timed_out()
            {
                keep_alive_state.set_connection_state(ConnectionState::Disrupted);
            }
        }
    });

    loop {
        // If the connection is disrupted, reconnect
        if state.has_connection_state(ConnectionState::Disrupted) {
            websocket.take();

            state.set_connection_state(ConnectionState::Disconnected);

            let backoff = std::cmp::min(
                Duration::from_millis(
                    100 * (1 << reconnect_attempts) + rand::random_range(0..JITTER_FACTOR),
                ),
                MAX_DELAY,
            );

            tokio::time::sleep(backoff).await;
            reconnect_attempts += 1;

            match connect_websocket(&ws_endpoint, &auth_type, state.clone()).await {
                Ok(ws) => {
                    websocket = Some(ws);
                    reconnect_attempts = 0;
                    for subscription in state.subscriptions.iter() {
                        let _ = send_subscribe(
                            subscription.key().clone(),
                            subscription.value().channel.clone(),
                            websocket.as_mut().unwrap(),
                            &auth_type,
                        )
                        .await;
                    }
                }
                // Most of these errors are not recoverable
                Err(Error::WebSocket(e)) => match e {
                    tokio_websockets::Error::AlreadyClosed => {
                        state.set_connection_state(ConnectionState::Disrupted);
                        continue;
                    }
                    tokio_websockets::Error::Io(_) => {
                        state.set_connection_state(ConnectionState::Disrupted);
                        continue;
                    }
                    _ => {
                        state.set_connection_state(ConnectionState::UnrecoverableError);
                        return;
                    }
                },
                Err(_) => {
                    state.set_connection_state(ConnectionState::Disrupted);
                    continue;
                }
            }
        }

        // If there is no WebSocket and the connection is not disrupted, connect
        if websocket.is_none()
            && !state.has_one_of_connection_states(&[
                ConnectionState::Disrupted,
                ConnectionState::UnrecoverableError,
            ])
        {
            state.set_connection_state(ConnectionState::Connecting);

            match connect_websocket(&ws_endpoint, &auth_type, state.clone()).await {
                Ok(ws) => {
                    websocket = Some(ws);
                    reconnect_attempts = 0;
                }
                // Most of these errors are not recoverable
                Err(Error::WebSocket(e)) => match e {
                    tokio_websockets::Error::AlreadyClosed => continue,
                    tokio_websockets::Error::Io(_) => continue,
                    _ => {
                        state.set_connection_state(ConnectionState::UnrecoverableError);
                        return;
                    }
                },
                Err(_) => {
                    state.set_connection_state(ConnectionState::Disrupted);
                    continue;
                }
            }
        }

        if !allow_no_subscriptions && state.subscriptions.is_empty() && websocket.is_some() {
            websocket.take();
            state.set_connection_state(ConnectionState::Disconnected);
            continue;
        }

        if let Some(ws) = websocket.as_mut() {
            let ws_next = ws.next();
            tokio::pin!(ws_next);

            let cmd_next = cmd_receiver.recv();
            tokio::pin!(cmd_next);

            match futures_util::future::select(ws_next, cmd_next).await {
                futures_util::future::Either::Left((msg_option, _)) => match msg_option {
                    Some(Ok(message)) => {
                        handle_websocket_message(message, state.clone()).await;
                    }
                    _ => {
                        state.set_connection_state(ConnectionState::Disrupted);
                    }
                },
                futures_util::future::Either::Right((cmd_option, _)) => match cmd_option {
                    Some(WebSocketCommand::Subscribe {
                        id,
                        channel,
                        result_sender,
                    }) => {
                        let (receiver, ack_receiver) =
                            handle_subscribe_command(id, channel, ws, &auth_type, state.clone())
                                .await;
                        // the task handling websocket_manager_loop is the same as the one sending the
                        // subscribe command and websocket_message_handler which receives the ack, so it needs
                        // to be handled in a separate task to prevent blocking
                        tokio::spawn(async move {
                            match ack_receiver.await {
                                Ok(Ok(())) => {
                                    let _ = result_sender.send(Ok(receiver));
                                }
                                Ok(Err(e)) => {
                                    let _ = result_sender.send(Err(e));
                                }
                                // ack_sender was dropped
                                Err(_) => {
                                    let _ = result_sender.send(Err(Error::Other(
                                        "Subscription acknowledgment channel closed".to_string(),
                                    )));
                                }
                            }
                        });
                    }
                    Some(WebSocketCommand::Publish {
                        channel,
                        event,
                        result_sender,
                    }) => {
                        let result = handle_publish_command(channel, event, ws, &auth_type).await;
                        let _ = result_sender.send(result);
                    }
                    Some(WebSocketCommand::Unsubscribe { id }) => {
                        let _ = handle_unsubscribe_command(id, ws, state.clone()).await;
                    }
                    Some(WebSocketCommand::Close) => {
                        websocket.take();
                        state.set_connection_state(ConnectionState::Disconnected);
                        return;
                    }
                    None => {
                        state.set_connection_state(ConnectionState::Disconnected);
                        return;
                    }
                },
            }
        } else {
            // If we don't have a WebSocket, just handle commands (mostly errors or close)
            // Check connection state before processing commands when disconnected
            if state.has_one_of_connection_states(&[
                ConnectionState::Disconnected,
                ConnectionState::Initial,
            ]) {
                if let Some(cmd) = cmd_receiver.recv().await {
                    match cmd {
                        WebSocketCommand::Close => {
                            return;
                        }
                        WebSocketCommand::Subscribe { result_sender, .. } => {
                            let _ =
                                result_sender.send(Err(Error::Other("Not connected".to_string())));
                        }
                        WebSocketCommand::Publish { result_sender, .. } => {
                            let _ =
                                result_sender.send(Err(Error::Other("Not connected".to_string())));
                        }
                        WebSocketCommand::Unsubscribe { .. } => {}
                    }
                } else {
                    return;
                }
            }
        }
    }
}

/// Connect to the AppSync WebSocket endpoint and perform handshake
async fn connect_websocket(
    ws_endpoint: &str,
    auth_type: &AuthType<'_>,
    state: Arc<WebSocketState>,
) -> Result<ClientSocket> {
    // Empty payload on connect
    let payload: &'static str = "{}";
    let connection_init: &'static str = "{\"type\":\"connection_init\"}";
    let header_name = HeaderName::from_static("sec-websocket-protocol");

    let auth_headers = auth_type.get_auth_headers(payload).await?;

    let auth_headers_json = serde_json::to_string(&auth_headers)?;
    let encoded_auth = URL_SAFE_NO_PAD.encode(&auth_headers_json);

    let auth_protocol = format!("header-{}", encoded_auth);

    let header_value_str = format!("{}, {}", WS_PROTOCOL_NAME, auth_protocol);

    let (mut ws_stream, _response) = ClientBuilder::from_uri(
        Uri::from_str(ws_endpoint)
            .map_err(|e| Error::Other(format!("Failed to parse endpoint: {}", e)))?,
    )
    .add_header(
        header_name,
        HeaderValue::from_str(&header_value_str)
            .map_err(|e| Error::Other(format!("Failed to add protocol header: {}", e)))?,
    )?
    .connect()
    .await?;

    state.set_connection_state(ConnectionState::Connecting);

    ws_stream.send(Message::text(connection_init)).await?;

    let ack_timeout = tokio::time::timeout(
        CONNECTION_INIT_TIMEOUT,
        wait_for_connection_ack(&mut ws_stream),
    )
    .await;

    match ack_timeout {
        // Connection established
        Ok(Ok(keep_alive_timeout)) => {
            state.set_connection_state(ConnectionState::Connected);
            state.set_keep_alive_timeout(keep_alive_timeout);
            state.update_keep_alive_timestamp();
            Ok(ws_stream)
        }
        // Error during connection handshake
        Ok(Err(err)) => {
            state.set_connection_state(ConnectionState::Disconnected);
            Err(err)
        }
        // Timeout
        Err(_) => {
            state.set_connection_state(ConnectionState::Disconnected);
            Err(Error::ConnectionTimeout)
        }
    }
}

/// Wait for connection acknowledgment during the handshake
async fn wait_for_connection_ack(client: &mut ClientSocket) -> Result<Duration> {
    while let Some(msg_result) = client.next().await {
        let msg = match msg_result {
            Ok(msg) => msg,
            Err(e) => return Err(e.into()),
        };
        let text = match msg.as_text() {
            Some(text) => text,
            None => continue,
        };
        let event_msg: ConnectionPayload = match serde_json::from_str(text) {
            Ok(msg) => msg,
            Err(_) => continue,
        };

        match event_msg {
            ConnectionPayload::Ack {
                connection_timeout_ms,
            } => {
                return Ok(connection_timeout_ms
                    .map_or(DEFAULT_KEEP_ALIVE_TIMEOUT, Duration::from_millis));
            }
            ConnectionPayload::Error { errors } => {
                if let Some(errors) = errors {
                    return Err(Error::HandshakeError(errors[0].message.clone()));
                }
                return Err(Error::HandshakeError("unknown".to_string()));
            }
        }
    }

    // If the loop exits, it means the connection closed before ack was received
    Err(Error::HandshakeError(
        "Connection closed during handshake".to_string(),
    ))
}

async fn handle_websocket_message(message: Message, state: Arc<WebSocketState>) {
    // Ignore malformed messages
    let text = match message.as_text() {
        Some(text) => text,
        None => return,
    };
    let event_msg: MessagePayload = match serde_json::from_str(text) {
        Ok(msg) => msg,
        Err(_) => return,
    };

    match event_msg {
        MessagePayload::KeepAlive => {
            state.update_keep_alive_timestamp();
        }
        MessagePayload::Data { id, payload } => {
            state.update_keep_alive_timestamp();

            if let Some(subscription) = state.subscriptions.get(&id) {
                let _ = subscription.sender.try_send(payload);
            }
        }
        MessagePayload::SubscribeAck { id } => {
            if let Some(mut subscription) = state.subscriptions.get_mut(&id) {
                if let Some(ack_sender) = subscription.ack_sender.take() {
                    let _ = ack_sender.send(Ok(()));
                }
            }
        }
        MessagePayload::Error { id } => {
            if let Some((_, mut subscription)) = state.subscriptions.remove(&id) {
                if let Some(ack_sender) = subscription.ack_sender.take() {
                    let _ = ack_sender.send(Err(Error::Other("Failed to subscribe".to_string())));
                }
            }
        }
        _ => {}
    }
}

async fn send_subscribe(
    id: String,
    channel: String,
    client: &mut ClientSocket,
    auth_type: &AuthType<'_>,
) -> Result<()> {
    let serialized_data = json!({
        "channel": &channel,
    })
    .to_string();

    let auth_headers = auth_type.get_auth_headers(&serialized_data).await?;

    let subscription_msg = SubscriptionMessage {
        message_type: "subscribe",
        id,
        channel: channel.clone(),
        events: None,
        authorization: auth_headers.clone(),
        payload: SubscriptionPayload {
            channel,
            events: None,
            extensions: crate::message::Extensions {
                authorization: auth_headers,
            },
        },
    };

    let subscription_msg_json = serde_json::to_string(&subscription_msg)?;
    client.send(Message::text(subscription_msg_json)).await?;

    Ok(())
}

async fn handle_subscribe_command(
    id: String,
    channel: String,
    client: &mut ClientSocket,
    auth_type: &AuthType<'_>,
    state: Arc<WebSocketState>,
) -> (mpsc::Receiver<String>, oneshot::Receiver<Result<()>>) {
    let (sender, receiver) = mpsc::channel(32);
    let (ack_sender, ack_receiver) = oneshot::channel();
    state.subscriptions.insert(
        id.clone(),
        SocketSubscription {
            channel: channel.clone(),
            sender,
            ack_sender: Some(ack_sender),
        },
    );

    let _ = send_subscribe(id, channel, client, auth_type).await;
    (receiver, ack_receiver)
}

async fn handle_publish_command(
    channel: String,
    event: String,
    client: &mut ClientSocket,
    auth_type: &AuthType<'_>,
) -> Result<()> {
    let id = Uuid::new_v4().to_string();

    // event API expects an array of JSON strings
    let events = [event];

    let serialized_data = json!({
        "channel": &channel,
        "events": &events
    })
    .to_string();

    let auth_headers = auth_type.get_auth_headers(&serialized_data).await?;

    let publish_msg = SubscriptionMessage {
        message_type: "publish",
        id,
        channel: channel.clone(),
        events: Some(events.clone()),
        authorization: auth_headers.clone(),
        payload: SubscriptionPayload {
            channel,
            events: Some(events),
            extensions: crate::message::Extensions {
                authorization: auth_headers,
            },
        },
    };

    let publish_msg_json = serde_json::to_string(&publish_msg)?;
    client.send(Message::text(publish_msg_json)).await?;

    Ok(())
}

async fn handle_unsubscribe_command(
    id: String,
    client: &mut ClientSocket,
    state: Arc<WebSocketState>,
) -> Result<()> {
    let _ = state.subscriptions.remove(&id);

    let unsubscribe_msg = json!({
        "type": "unsubscribe",
        "id": id
    });

    client
        .send(Message::text(unsubscribe_msg.to_string()))
        .await?;

    Ok(())
}

impl AppSyncEventsClient<'_> {
    /// Initialize the client, starting the WebSocket manager
    pub async fn connect(&mut self) -> Result<()>
    where
        Self: 'static,
    {
        if self.cmd_sender.is_some() {
            return Ok(());
        }

        let (cmd_sender, cmd_receiver) = mpsc::channel::<WebSocketCommand>(32);
        self.cmd_sender = Some(cmd_sender);

        let ws_endpoint = self.realtime_endpoint.clone();
        let auth_type = self.auth_type.clone();
        let allow_no_subscriptions = self.allow_no_subscriptions;

        let state = Arc::new(WebSocketState::new());
        let initial_connection = connect_websocket(&ws_endpoint, &auth_type, state.clone()).await?;

        tokio::spawn(async move {
            websocket_manager_loop(
                ws_endpoint,
                auth_type,
                cmd_receiver,
                allow_no_subscriptions,
                initial_connection,
                state,
            )
            .await;
        });

        Ok(())
    }

    /// Subscribe to an event channel
    pub async fn subscribe<T>(&self, channel: impl Into<String>) -> Result<Subscription<T>>
    where
        T: DeserializeOwned + Unpin,
    {
        let channel = channel.into();

        if self.cmd_sender.is_none() {
            return Err(Error::Other("Client not initialized".to_string()));
        }

        let cmd_sender = self.cmd_sender.as_ref().unwrap();
        let (result_sender, result_receiver) = oneshot::channel();

        let id = Uuid::new_v4().to_string();

        cmd_sender
            .send(WebSocketCommand::Subscribe {
                id: id.clone(),
                channel,
                result_sender,
            })
            .await
            .map_err(|_| Error::Other("Failed to send subscribe command".to_string()))?;

        let event_receiver = result_receiver
            .await
            .map_err(|_| Error::Other("Failed to receive subscription result".to_string()))??;

        let (unsub_sender, mut unsub_receiver) = mpsc::channel::<String>(1);

        let cmd_sender = cmd_sender.clone();
        tokio::spawn(async move {
            if let Some(id) = unsub_receiver.recv().await {
                let _ = cmd_sender.send(WebSocketCommand::Unsubscribe { id }).await;
            }
        });

        let subscription = Subscription::new(id, unsub_sender, event_receiver);

        Ok(subscription)
    }

    /// Publish to an event channel
    pub async fn publish<T>(&self, channel: impl Into<String>, event: T) -> Result<()>
    where
        T: Serialize,
    {
        let channel = channel.into();

        let cmd_sender = if let Some(cmd_sender) = self.cmd_sender.as_ref() {
            cmd_sender
        } else {
            return Err(Error::Other("Client not initialized".to_string()));
        };

        let (result_sender, result_receiver) = oneshot::channel();

        let event_json = serde_json::to_string(&event)?;

        cmd_sender
            .send(WebSocketCommand::Publish {
                channel,
                event: event_json,
                result_sender,
            })
            .await
            .map_err(|_| Error::Other("Failed to send publish command".to_string()))?;

        result_receiver
            .await
            .map_err(|_| Error::Other("Failed to receive publish result".to_string()))??;

        Ok(())
    }

    /// Close the client
    pub async fn close(&self) -> Result<()> {
        if let Some(cmd_sender) = self.cmd_sender.as_ref() {
            cmd_sender
                .send(WebSocketCommand::Close)
                .await
                .map_err(|_| Error::Other("Failed to send close command".to_string()))?;
        }

        Ok(())
    }

    /// Close socket if there are no active subscriptions
    pub fn close_if_no_active_subscription(&self) {
        if let Some(cmd_sender) = &self.cmd_sender {
            let cmd_sender = cmd_sender.clone();
            tokio::spawn(async move {
                let _ = cmd_sender.send(WebSocketCommand::Close).await;
            });
        }
    }
}
