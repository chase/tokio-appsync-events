use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Subscription message for the AppSync real-time protocol
#[derive(Serialize)]
pub struct SubscriptionMessage {
    /// Message type
    #[serde(rename = "type")]
    pub message_type: &'static str,

    /// Message ID
    pub id: String,

    /// Channel/topic to subscribe to
    pub channel: String,

    /// Events to filter on (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events: Option<[String; 1]>,

    /// Authorization headers
    pub authorization: HashMap<String, String>,

    /// Payload data
    pub payload: SubscriptionPayload,
}

/// Payload for subscription messages
#[derive(Serialize)]
pub struct SubscriptionPayload {
    /// Channel/topic
    pub channel: String,

    /// Events to filter on (optional)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events: Option<[String; 1]>,

    /// Extensions for the payload
    pub extensions: Extensions,
}

/// Extensions for authorization
#[derive(Serialize)]
pub struct Extensions {
    /// Authorization headers
    pub authorization: HashMap<String, String>,
}

#[derive(Deserialize)]
pub struct HandshakeError {
    #[serde(default)]
    pub message: Option<String>,
    #[serde(rename = "errorType")]
    pub error_type: Option<String>,
}

#[derive(Deserialize)]
pub struct PublishError {
    #[serde(rename = "errorType")]
    pub error_type: String,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
pub enum ConnectionPayload {
    /// Connection handshake success
    #[serde(rename = "connection_ack")]
    Ack {
        #[serde(rename = "connectionTimeoutMs")]
        connection_timeout_ms: Option<u64>,
    },

    /// Connection error
    #[serde(rename = "connection_error")]
    Error {
        errors: Vec<HandshakeError>,
    },
}

#[derive(Deserialize)]
#[serde(tag = "type")]
pub enum MessagePayload {

    /// Connection keep alive
    #[serde(rename = "ka")]
    KeepAlive,

    /// Subscription success
    #[serde(rename = "subscribe_success")]
    SubscribeAck {
        /// Subscription ID
        id: String,
    },

    /// Publish success
    #[serde(rename = "publish_success")]
    PublishAck {
        /// Publish ID
        id: String,
    },

    /// Publish error
    #[serde(rename = "publish_error")]
    PublishError {
        /// Publish ID
        id: String,

        errors: [PublishError; 1],
    },

    /// Error
    #[serde(rename = "error")]
    Error {
        /// Subscription ID
        id: String,
    },

    /// Data payload
    #[serde(rename = "data")]
    Data {
        /// Subscription ID
        id: String,

        /// JSON Payload
        #[serde(rename = "event")]
        payload: String,
    },
}
