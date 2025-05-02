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
    const CHANNEL: &str = "default/test-channel";

    // Get static AWS SDK Config
    let aws_config = get_aws_config().await;

    // Create the client using the new builder with IAM auth
    let mut client = AppSyncEventsClientBuilder::new(APPSYNC_REALTIME_HOST, aws_config)
        .with_iam_auth()?;

    // Connect the client (replaces initialize)
    client.connect().await?;

    println!("Subscribing to channel '{}'...", CHANNEL);

    // Subscribe to an event channel, specifying the expected data type
    let mut subscription = client
        .subscribe::<MyEventData>(CHANNEL)
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

    // Wait for the event handling task to finish
    let _ = event_task.await;

    // Close the client
    client.close().await?;
    println!("Client closed");

    Ok(())
} 