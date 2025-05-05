// SPDX-FileCopyrightText: 2025 Chase Colman
// SPDX-License-Identifier: MPL-2.0

use std::{
    collections::{HashMap, HashSet},
    net::SocketAddr,
    process,
    sync::{atomic::{AtomicBool, Ordering}, Arc},
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use clap::Parser;
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::{
    net::{TcpListener, TcpStream},
    select,
    sync::{Notify, mpsc},
    time::interval,
};
use tokio_websockets::{Message, ServerBuilder, WebSocketStream};
use tracing::{debug, error, info, instrument, trace, warn};
use tracing_subscriber::fmt::format::FmtSpan;
use uuid::Uuid;

const WS_PROTOCOL_NAME: &str = "aws-appsync-event-ws";
const DEFAULT_KEEP_ALIVE_TIMEOUT_MS: u64 = 300_000; // 5 minutes
const KEEP_ALIVE_INTERVAL_S: u64 = 30; // Send keep-alive every 30s

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Port to listen on
    #[arg(short, long, default_value_t = 8080)]
    port: u16,

    /// Disconnect clients after a number of seconds (optional)
    #[arg(long)]
    disconnect_after: Option<u64>,

    /// Stop the server after a number of seconds (optional)
    #[arg(long)]
    stop_after: Option<u64>,

    /// Probability (0.0 to 1.0) of entering downtime after a connection
    #[arg(long, default_value_t = 0.0)]
    downtime_probability: f64,

    /// Maximum duration in seconds for random downtime
    #[arg(long, default_value_t = 10)]
    downtime_max_secs: u64,
}

// Renamed for clarity
#[derive(Deserialize, Debug)]
struct IncomingMessage {
    #[serde(rename = "type")]
    message_type: String,
    id: String,
    #[serde(default)]
    channel: String,
    #[serde(default)]
    events: Option<Vec<String>>,
}

#[derive(Serialize, Deserialize, Debug)]
struct AuthHeaderPayload {
    host: String,
    #[serde(flatten)]
    other_headers: HashMap<String, String>,
}

// Sends (subscription_id, payload_string)
type ConnectionSender = mpsc::Sender<(String, String)>;

// subscription_id -> (channel_name, sender_to_connection_task)
#[derive(Debug, Clone, Default)]
struct ServerState {
    subscriptions: Arc<DashMap<String, (String, ConnectionSender)>>,
}

impl ServerState {
    fn add_subscription(&self, sub_id: String, channel: String, sender: ConnectionSender) {
        self.subscriptions.insert(sub_id, (channel, sender));
    }

    fn remove_subscription(&self, sub_id: &str) {
        self.subscriptions.remove(sub_id);
    }

    fn remove_subscriptions(&self, sub_ids: HashSet<String>) {
        self.subscriptions.retain(|k, _v| !sub_ids.contains(k));
    }

    async fn publish(&self, channel: &str, payload: String) {
        for entry in self.subscriptions.iter() {
            let sub_id = entry.key();
            let (subscribed_channel, sender) = entry.value();

            if subscribed_channel == channel {
                let message_tuple = (sub_id.clone(), payload.clone());
                if let Err(e) = sender.try_send(message_tuple) {
                    // Error likely means the receiving task's channel is closed/full
                    warn!(
                        sub_id = %sub_id,
                        error = %e,
                        "Failed to send published message. (Task likely terminated)",
                    );
                    // Optionally: Could remove the subscription here if send fails repeatedly
                }
            }
        }
    }
}

#[tokio::main]
#[instrument]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_span_events(FmtSpan::NONE)
        .without_time()
        .with_target(false)
        .init();

    info!("Starting mock AppSync server...");

    let args = Args::parse();
    let in_docker = std::env::var("IN_DOCKER").map_or(false, |v| v == "true");
    let ip_addr = if in_docker {
        [0, 0, 0, 0]
    } else {
        [127, 0, 0, 1]
    };
    let addr = SocketAddr::from((ip_addr, args.port));
    let listener = TcpListener::bind(&addr).await?;
    info!(address = %addr, "Listening on address");

    let running = Arc::new(AtomicBool::new(true));
    let server_state = ServerState::default();

    setup_stop_timer(&args, running.clone());

    info!("Starting main accept loop...");

    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.unwrap();
        info!("Received Ctrl+C, shutting down...");
        process::exit(0);
    });

    while let Ok((stream, peer_addr)) = listener.accept().await {
        handle_single_accept(stream, peer_addr, &args, server_state.clone()).await;
    }

    warn!("Server accept loop exited unexpectedly.");

    Ok(())
}

/// Sets up a Tokio task to stop the server after a specified duration, if configured.
fn setup_stop_timer(args: &Args, running: Arc<AtomicBool>) {
    if let Some(stop_secs) = args.stop_after {
        let running_clone = running.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(stop_secs)).await;
            info!("Stop timer elapsed, shutting down.");
            running_clone.store(false, Ordering::SeqCst);
        });
    }
}

/// Handles a single accepted TCP connection: spawns the connection handler
/// and optionally simulates server downtime after accepting.
async fn handle_single_accept(
    stream: TcpStream,
    peer_addr: SocketAddr,
    args: &Args,
    state: ServerState,
) {
    debug!(peer = %peer_addr, "Accepted TCP connection. Spawning task.");

    let disconnect_after = args.disconnect_after.map(Duration::from_secs);
    let state_clone_for_handler = state.clone();
    tokio::spawn(async move {
        let conn_id = Uuid::new_v4();
        debug!(connection_id = %conn_id, "Task spawned. Calling handle_connection.");
        if let Err(e) =
            handle_connection(stream, disconnect_after, state_clone_for_handler, conn_id).await
        {
            error!(connection_id = %conn_id, error = %e, "Connection handler error");
        } else {
            info!(connection_id = %conn_id, "Connection closed cleanly.");
        }
    });

    if args.downtime_probability > 0.0
        && args.downtime_max_secs > 0
        && rand::random_bool(args.downtime_probability)
    {
        let downtime_secs = rand::random_range(1..=args.downtime_max_secs);
        let downtime_duration = Duration::from_secs(downtime_secs);
        warn!(duration = ?downtime_duration, "Entering simulated server downtime (pausing accept loop)...");
        tokio::time::sleep(downtime_duration).await;
        warn!("Simulated server downtime finished. Resuming accept loop.");
    }
}

#[instrument(skip(stream, state, disconnect_after), fields(conn_id = %conn_id))]
async fn handle_connection(
    stream: TcpStream,
    disconnect_after: Option<Duration>,
    state: ServerState,
    conn_id: Uuid,
) -> Result<(), Box<dyn std::error::Error>> {
    debug!("Entered handle_connection");
    let mut ws_stream = accept_websocket(stream).await?;
    info!("WebSocket handshake successful");

    let keep_alive_timeout_ms = DEFAULT_KEEP_ALIVE_TIMEOUT_MS;

    let init_msg = match tokio::time::timeout(Duration::from_secs(10), ws_stream.next()).await {
        Ok(Some(Ok(msg))) if msg.is_text() => msg,
        Ok(Some(Ok(_))) => {
            warn!("Expected text message for connection_init");
            return Err("Expected text message for connection_init".into());
        }
        Ok(Some(Err(e))) => {
            error!(error = %e, "WebSocket error during connection_init");
            return Err(Box::new(e));
        }
        Ok(None) | Err(_) => {
            warn!("Timeout or closed during connection_init");
            return Err("Timeout waiting for connection_init".into());
        }
    };
    let init_text = init_msg.as_text().unwrap_or("");
    if !init_text.contains(r#""type":"connection_init""#) {
        warn!(received = %init_text, "Invalid connection_init message");
        let error_payload = json!({
            "type": "connection_error",
            "errors": [{"errorType": "InvalidHandshake", "message": "Expected connection_init"}]
        });
        ws_stream
            .send(Message::text(error_payload.to_string()))
            .await?;
        return Err("Invalid connection_init message".into());
    }
    let ack_payload = json!({
        "type": "connection_ack",
        "connectionTimeoutMs": keep_alive_timeout_ms
    });
    ws_stream
        .send(Message::text(serde_json::to_string(&ack_payload)?))
        .await?;
    info!(timeout_ms = keep_alive_timeout_ms, "Sent connection_ack");

    let (conn_tx, mut conn_rx) = mpsc::channel::<(String, String)>(32);
    let mut local_subscription_ids: HashSet<String> = HashSet::new();

    let disconnect_signal = Arc::new(Notify::new());
    if let Some(duration) = disconnect_after {
        let signal = disconnect_signal.clone();
        tokio::spawn(async move {
            tokio::time::sleep(duration).await;
            info!("Disconnect timer elapsed, closing connection.");
            signal.notify_one();
        });
    }

    let mut keep_alive_interval = interval(Duration::from_secs(KEEP_ALIVE_INTERVAL_S));

    loop {
        select! {
            _ = disconnect_signal.notified() => {
                info!("Received disconnect signal.");
                break;
            }

            _ = keep_alive_interval.tick() => {
                let ka_payload = json!({"": "ka"});
                trace!("Sending keep-alive ping");
                if let Err(e) = ws_stream.send(Message::text(serde_json::to_string(&ka_payload)?)).await {
                     error!(error = %e, "Failed to send keep-alive");
                     break; // Assume connection is dead
                }
            }

            msg_option = ws_stream.next() => {
                match msg_option {
                    Some(Ok(message)) => {
                        if message.is_close() {
                             info!("Received close frame");
                             break;
                         }
                        if !message.is_text() {
                             debug!("Received non-text message, ignoring");
                             continue;
                         }
                        let text = message.as_text().unwrap();
                         debug!(message = %text, "Received message");

                        match process_incoming_message(text, &state, &conn_tx, &mut local_subscription_ids, &mut ws_stream).await {
                            Ok(_) => {}
                            Err(ProcessError::Fatal(e)) => {
                                error!(error = %e, "Fatal error processing message, closing connection.");
                                break;
                            }
                            Err(ProcessError::Parse(e)) => {
                                error!(error = %e, raw_message = %text, "Non-fatal message processing error");
                            }
                        }
                    }
                    Some(Err(e)) => {
                         error!(error = %e, "WebSocket error");
                         break;
                     }
                    None => {
                         info!("WebSocket stream closed by peer");
                         break;
                     }
                }
            }

            Some((received_sub_id, published_payload)) = conn_rx.recv() => {
                 debug!(sub_id = %received_sub_id, "Received published message to forward");
                 let data_msg = json!({
                     "type": "data",
                     "id": received_sub_id,
                     "event": published_payload
                 });
                 if let Err(e) = ws_stream.send(Message::text(serde_json::to_string(&data_msg)?)).await {
                     error!(error = %e, "Failed to forward published data");
                     break;
                 }
                 trace!(sub_id = %received_sub_id, "Forwarded published data");
             }
        }
    }

    info!("Cleaning up connection...");
    state.remove_subscriptions(local_subscription_ids);
    info!("Connection cleanup complete.");

    let _ = ws_stream.close().await;
    Ok(())
}

/// Enum to categorize errors during message processing.
enum ProcessError {
    /// Error that requires closing the connection (e.g., failed WebSocket write).
    Fatal(Box<dyn std::error::Error + Send + Sync>),
    /// Error in parsing or handling the message content, connection might stay open.
    Parse(Box<dyn std::error::Error + Send + Sync>),
}

// Implement From for easier error conversion
impl<E: std::error::Error + Send + Sync + 'static> From<E> for ProcessError {
    fn from(err: E) -> Self {
        // Default to Parse error, specific cases can create Fatal explicitly
        ProcessError::Parse(Box::new(err))
    }
}

/// Processes a single incoming text WebSocket message.
#[instrument(skip(state, conn_tx, local_subscription_ids, ws_stream, text))]
async fn process_incoming_message(
    text: &str,
    state: &ServerState,
    conn_tx: &ConnectionSender,
    local_subscription_ids: &mut HashSet<String>,
    ws_stream: &mut WebSocketStream<TcpStream>,
) -> Result<(), ProcessError> {
    match serde_json::from_str::<IncomingMessage>(text) {
        Ok(parsed_msg) => {
            debug!(message_type = %parsed_msg.message_type, id = %parsed_msg.id, "Processing parsed message");

            match parsed_msg.message_type.as_str() {
                "subscribe" => {
                    let sub_id = parsed_msg.id.clone();

                    if local_subscription_ids.contains(&sub_id) {
                        warn!(sub_id = %sub_id, "Subscription ID already exists.");
                        // Optionally send an error back to client, but don't terminate connection
                        // For now, just log and continue.
                        return Ok(());
                    }

                    state.add_subscription(
                        sub_id.clone(),
                        parsed_msg.channel.clone(),
                        conn_tx.clone(),
                    );
                    local_subscription_ids.insert(sub_id.clone());

                    let ack = json!({"type": "subscribe_success", "id": sub_id});
                    ws_stream
                        .send(Message::text(serde_json::to_string(&ack)?))
                        .await
                        .map_err(|e| ProcessError::Fatal(Box::new(e)))?; // Send error is fatal
                    info!(sub_id = %sub_id, "Sent subscribe_success");
                    Ok(())
                }
                "publish" => {
                    if let Some(events) = parsed_msg.events {
                        for event_payload in events {
                            trace!(channel = %parsed_msg.channel, payload = %event_payload, "Publishing event");
                            state.publish(&parsed_msg.channel, event_payload).await;
                        }
                    } else {
                        warn!(id = %parsed_msg.id, "Publish message missing 'events' field.");
                    }

                    let ack = json!({"type": "publish_success", "id": parsed_msg.id});
                    ws_stream
                        .send(Message::text(serde_json::to_string(&ack)?))
                        .await
                        .map_err(|e| ProcessError::Fatal(Box::new(e)))?; // Send error is fatal
                    info!(id = %parsed_msg.id, "Sent publish_success");
                    Ok(())
                }
                "unsubscribe" => {
                    let sub_id_to_remove = parsed_msg.id;
                    if local_subscription_ids.remove(&sub_id_to_remove) {
                        state.remove_subscription(&sub_id_to_remove);
                        info!(sub_id = %sub_id_to_remove, "Processed unsubscribe");
                    } else {
                        warn!(sub_id = %sub_id_to_remove, "Received unsubscribe for unknown id");
                    }
                    Ok(())
                }
                _ => {
                    warn!(type = %parsed_msg.message_type, id = %parsed_msg.id, "Received unknown message type");
                    Ok(())
                }
            }
        }
        Err(e) => {
            // Failed to parse the incoming JSON. This is a Parse error, not Fatal.
            Err(ProcessError::Parse(Box::new(e)))
        }
    }
}

#[instrument(skip(stream))]
async fn accept_websocket(
    stream: TcpStream,
) -> Result<WebSocketStream<TcpStream>, Box<dyn std::error::Error>> {
    debug!("Entered accept_websocket");

    let (request, ws_stream) = ServerBuilder::new().accept(stream).await?;
    debug!("ServerBuilder::accept succeeded (base handshake OK). Now parsing headers.");

    let req = &request;
    debug!("Checking headers...");
    let header_value_result = req
        .headers()
        .get("Sec-WebSocket-Protocol")
        .map(|v| v.to_str());

    let protocol_str = match header_value_result {
        Some(Ok(s)) => {
            debug!(value = %s, "Found Sec-WebSocket-Protocol header value");
            s
        }
        Some(Err(e)) => {
            error!(error = %e, "Sec-WebSocket-Protocol header is not valid UTF-8");
            return Err("Invalid Sec-WebSocket-Protocol header encoding".into());
        }
        None => {
            debug!("No Sec-WebSocket-Protocol header found.");
            // Treat as empty if header missing
            ""
        }
    };

    let protocols: Vec<&str> = protocol_str
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    debug!(protocols = ?protocols, "Parsed protocols");

    let has_appsync_proto = protocols.iter().any(|&p| p == WS_PROTOCOL_NAME);
    debug!(protocol = %WS_PROTOCOL_NAME, result = has_appsync_proto, "Checking for AppSync protocol");

    if !has_appsync_proto {
        warn!(protocol = %WS_PROTOCOL_NAME, "Missing required protocol.");
        return Err(format!("Missing required '{}' protocol", WS_PROTOCOL_NAME).into());
    }
    info!(protocol = %WS_PROTOCOL_NAME, "Found required protocol");

    let auth_header = protocols
        .iter()
        .find(|&&p| p.starts_with("header-"))
        .copied();
    debug!(auth_header = ?auth_header, "Found auth header part");

    if let Some(auth_p) = auth_header {
        info!("Found auth protocol header");
        if let Some(encoded_auth) = auth_p.strip_prefix("header-") {
            match URL_SAFE_NO_PAD.decode(encoded_auth) {
                Ok(decoded_bytes) => {
                    let auth_json = String::from_utf8_lossy(&decoded_bytes);
                    trace!(json = %auth_json, "Decoded auth header");
                    match serde_json::from_str::<AuthHeaderPayload>(&auth_json) {
                        Ok(auth_payload) => {
                            debug!(payload = ?auth_payload, "Successfully parsed auth payload");
                            // TODO: Add actual auth validation logic here
                        }
                        Err(e) => {
                            error!(error = %e, json = %auth_json, "Failed to parse auth header JSON");
                            return Err("Invalid auth header format".into());
                        }
                    }
                }
                Err(e) => {
                    error!(error = %e, header = %auth_p, "Failed to decode base64 auth header");
                    return Err("Invalid base64 encoding in auth header".into());
                }
            }
        } else {
            // This case should be impossible if find succeeded and starts_with worked
            error!(header = %auth_p, "Auth header found but strip_prefix failed");
            return Err("Malformed auth header protocol".into());
        }
    } else {
        info!("No auth protocol header found (may be okay depending on auth type)");
    }

    Ok(ws_stream)
}
