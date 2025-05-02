//! A Rust client for AWS AppSync Real-Time Events API
//! 
//! This library provides functionality to connect to AWS AppSync Events endpoints
//! using WebSockets. It supports subscribing to events and publishing events.
//! 
//! Currently supports IAM and Lambda authentication methods.

mod auth;
mod error;
mod message;
mod client;
mod url;

pub use auth::AuthType;
pub use client::{AppSyncEventsClient, AppSyncEventsClientBuilder, Subscription};
pub use error::Error;
pub use message::MessagePayload;