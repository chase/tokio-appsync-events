// SPDX-FileCopyrightText: 2025 Chase Colman
// SPDX-License-Identifier: MPL-2.0

use aws_config::BehaviorVersion;
use aws_config::SdkConfig;
use futures_util::StreamExt;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::OnceCell;
use tokio::task::JoinSet;

use tokio_appsync_events::AppSyncEventsClientBuilder;

#[derive(Debug, Deserialize, Serialize)]
struct MyEventData {
    message: String,
    channel: String,
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
    const APPSYNC_REALTIME_HOST: &str =
        "**************************.appsync-realtime-api.us-east-1.amazonaws.com";
    const API_KEY: &str = "da2-**************************";
    const NUM_CHANNELS: usize = 10;
    let channels = (0..NUM_CHANNELS).map(|i| format!("default/channel{}", i)).collect::<Vec<_>>();
    let aws_config = get_aws_config().await;

    let mut client = AppSyncEventsClientBuilder::new(APPSYNC_REALTIME_HOST, aws_config)
        .with_endpoint_url("ws://127.0.0.1:8080")
        .with_api_key_auth(API_KEY)?;

    client.connect().await?;
    println!("Client connected.");

    let mut sub_tasks = JoinSet::new();

    for channel_name in channels.clone() {
        let client = client.clone();
        sub_tasks.spawn(async move {
            println!("Task for channel {}: waiting for event...", channel_name);
            let Ok(mut subscription) = client.subscribe::<MyEventData>(&channel_name).await else {
                eprintln!("Failed to subscribe to channel '{}'", channel_name);
                return;
            };
            let sub_id = subscription.id().to_string();
            println!("Subscribed to {} with ID: {}", channel_name, sub_id);

            let Some(event_data) = subscription.next().await else {
                println!("Channel {}: Subscription stream ended without an event.", channel_name);
                return;
            };
            println!("Channel {}: Received event data: {:?}", channel_name, event_data);
            if event_data.channel != channel_name.to_string() {
                eprintln!(
                    "Channel {}: Mismatch! Event is for channel '{}' but received on subscription for '{}'. Data: {:?}",
                    channel_name, event_data.channel, channel_name, event_data
                );
            }
        });
    }

    for channel_name in channels {
        let client = client.clone();
        tokio::spawn(async move {
            println!("Publishing to channel '{}'...", &channel_name);
            client
                .publish(
                    &channel_name,
                    MyEventData {
                        message: format!("Hello from publisher to {}!", channel_name),
                        channel: channel_name.to_string(),
                    },
                )
                .await
                .unwrap();
            println!("Published to {}", channel_name);
        });
    }

    println!("Waiting for all subscription events...");
    while let Some(result) = sub_tasks.join_next().await {
        if let Err(join_error) = result {
            eprintln!("A subscription task failed to execute: {:?}", join_error);
        }
    }

    client.close().await?;
    println!("Client closed");

    Ok(())
}
