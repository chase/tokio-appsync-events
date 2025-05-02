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
    // Get static AWS SDK Config
    let aws_config = get_aws_config().await;
    let appsync_app_id = "your-appsync-app-id";
    let api_key = "your-api-key";
    let channel = "default/test-channel";

    // Create the client using the new builder
    let mut client = AppSyncEventsClientBuilder::new(appsync_app_id, aws_config)
        .with_api_key_auth(api_key)?;

    // Connect the client (replaces initialize)
    client.connect().await?;

    println!("Subscribing to channel '{}'...", channel);

    // Subscribe to an event channel, specifying the expected data type
    let mut subscription = client
        .subscribe::<MyEventData>(channel)
        .await?;

    println!("Subscribed with ID: {}", subscription.id());

    // Spawn a task to handle events by iterating the subscription stream
    let event_task = tokio::spawn(async move {
        let Some(event_data) = subscription.next().await else {
            println!("Subscription stream ended.");
            return;
        };
        println!("Received event data: {:?}", event_data);
    });

    // Publish to the same channel
    println!("Publishing to channel '{}'...", channel);
    let payload = MyEventData {
        message: "Hello from publisher!".to_string(),
    };
    client.publish(channel, payload).await?;

    // Wait for the event handling task to finish
    let _ = event_task.await;

    // Close the client
    client.close().await?;
    println!("Client closed");

    Ok(())
} 