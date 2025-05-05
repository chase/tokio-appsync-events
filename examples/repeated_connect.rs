// SPDX-FileCopyrightText: 2025 Chase Colman
// SPDX-License-Identifier: MPL-2.0

use aws_config::BehaviorVersion;
use aws_config::SdkConfig;
use futures_util::StreamExt;
use serde::Deserialize;
use serde::Serialize;
use std::process;
use std::time::Duration;
use tokio::sync::OnceCell;
use tokio::time::sleep;

use tokio_appsync_events::AppSyncEventsClientBuilder;

#[derive(Debug, Serialize, Deserialize)]
struct PublishData {
    sequence: u64,
    timestamp: String,
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
    const CHANNEL: &str = "default/repeater-channel";
    const PUBLISH_INTERVAL_SECS: u64 = 1;

    println!("Repeated Connect/Publish Example");
    println!("Press Ctrl+C to stop.");

    let aws_config = get_aws_config().await;

    let mut client = AppSyncEventsClientBuilder::new(APPSYNC_REALTIME_HOST, aws_config)
        .with_endpoint_url("ws://127.0.0.1:9521") // this allows you to override the default endpoint, for example, to connect to localhost
        .with_api_key_auth(API_KEY)?;

    println!("Attempting initial connection...");
    client.connect().await?;
    println!("Initial connection successful.");

    println!(
        "Subscribing to channel '{}' to keep connection alive...",
        CHANNEL
    );
    let mut subscription = client.subscribe::<PublishData>(CHANNEL).await?;
    println!("Subscription successful (ID: {}).", subscription.id());

    tokio::spawn(async move {
        while let Some(event_data) = subscription.next().await {
            println!("Received event data: {:?}", event_data);
        }
        println!("Subscription stream ended.");
    });

    let close_client = client.clone();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.unwrap();
        println!("Received Ctrl+C, shutting down...");
        println!("Closing client...");
        let _ = close_client.close().await;
        process::exit(0);
    });

    let mut sequence_number: u64 = 0;

    println!("Publishing to channel: {}", CHANNEL);
    loop {
        sleep(Duration::from_secs(PUBLISH_INTERVAL_SECS)).await;
        sequence_number += 1;
        let payload = PublishData {
            sequence: sequence_number,
            timestamp: chrono::Utc::now().to_rfc3339(),
        };

        println!("Publishing message #{}...", sequence_number);

        if let Err(e) = client.publish(CHANNEL, &payload).await {
            println!(
                "Publish #{} error: {}. Client will attempt reconnect/retry on next iteration if needed.",
                sequence_number, e
            );
        } else {
            println!("Publish #{} successful.", sequence_number);
        }
    }
}
