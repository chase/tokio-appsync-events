use aws_config::BehaviorVersion;
use aws_config::SdkConfig;
use futures_util::StreamExt;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::OnceCell;
use tokio::time::Duration;

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
    // Get static AWS SDK Config
    let aws_config = get_aws_config().await;
    let appsync_app_id = "your-appsync-app-id";
    let api_key = "your-api-key";

    // Create the client using the new builder
    let mut client = AppSyncEventsClientBuilder::new(appsync_app_id, aws_config)
        .with_api_key_auth(api_key)?;

    // Connect the client (replaces initialize)
    client.connect().await?;

    println!("Subscribing to channel 'test-channel'...");

    // Subscribe to an event channel, specifying the expected data type
    let mut subscription = client
        .subscribe::<MyEventData>("test-channel")
        .await?;

    println!("Subscribed with ID: {}", subscription.id());

    // Spawn a task to handle events by iterating the subscription stream
    let event_task = tokio::spawn(async move {
        while let Some(event_data) = subscription.next().await {
            println!("Received event data: {:?}", event_data);
            // No manual parsing needed here, the stream handles deserialization
        }
        println!("Subscription stream ended.");
    });

    // Give some time for the subscription to establish before publishing
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Publish to the same channel
    println!("Publishing to channel 'test-channel'...");
    let payload = MyEventData {
        message: "Hello from publisher!".to_string(),
    };
    client.publish("test-channel", payload).await?;

    // Wait for a moment to receive events
    tokio::time::sleep(Duration::from_secs(10)).await;

    // Close the client
    client.close().await?;

    println!("Client closed");

    // Wait for the event handling task to finish
    let _ = event_task.await;

    Ok(())
} 