// SPDX-FileCopyrightText: 2025 Chase Colman
// SPDX-License-Identifier: MPL-2.0

use aws_config::BehaviorVersion;
use aws_config::SdkConfig;
use futures_util::StreamExt;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::OnceCell;

use tokio_appsync_events::AppSyncEventsClientBuilder;

#[derive(Debug, Deserialize, Serialize)]
struct MyEventData {
    message: String,
}

static AWS_CONFIG: OnceCell<SdkConfig> = OnceCell::const_new();

async fn get_aws_config() -> &'static SdkConfig {
    AWS_CONFIG
        .get_or_init(|| async {
            aws_config::defaults(BehaviorVersion::latest())
                .region("us-east-1")
                .load()
                .await
        })
        .await
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    const APPSYNC_REALTIME_HOST: &str = "**************************.appsync-realtime-api.us-east-1.amazonaws.com";
    const API_KEY: &str = "da2-**************************";
    const CHANNEL: &str = "default/test-channel";
    let aws_config = get_aws_config().await;

    let mut client = AppSyncEventsClientBuilder::new(APPSYNC_REALTIME_HOST, aws_config)
        .with_endpoint_url("ws://127.0.0.1:8080") // this allows you to override the default endpoint, for example, to connect to localhost
        .with_api_key_auth(API_KEY)?;

    client.connect().await?;

    println!("Subscribing to channel '{}'...", CHANNEL);

    let mut subscription = client
        .subscribe::<MyEventData>(CHANNEL)
        .await?;

    println!("Subscribed with ID: {}", subscription.id());

    let event_task = tokio::spawn(async move {
        let Some(event_data) = subscription.next().await else {
            println!("Subscription stream ended.");
            return;
        };
        println!("Received event data: {:?}", event_data);
    });

    println!("Publishing to channel '{}'...", CHANNEL);
    let payload = MyEventData {
        message: "Hello from publisher!".to_string(),
    };
    client.publish(CHANNEL, payload).await?;

    let _ = event_task.await;

    client.close().await?;
    println!("Client closed");

    Ok(())
} 