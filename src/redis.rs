use anyhow::Result;
use redis::aio::{MultiplexedConnection, PubSub};
use redis::{AsyncCommands, Client};
use serde::de::DeserializeOwned;

/// Subscribe to a Redis channel and return both PubSub and a publishing connection
pub async fn subscribe(
    client: &Client,
    channel: &str,
) -> Result<(PubSub, MultiplexedConnection)> {
    let mut pubsub = client.get_async_pubsub().await?;
    pubsub.subscribe(channel).await?;
    let conn = client.get_multiplexed_async_connection().await?;
    Ok((pubsub, conn))
}

/// Parse a Redis message payload as JSON
pub fn parse<T: DeserializeOwned>(msg: &redis::Msg) -> Result<T> {
    let payload: String = msg.get_payload()?;
    Ok(serde_json::from_str(&payload)?)
}

/// Publish a JSON value to a Redis channel
pub async fn publish(
    conn: &mut MultiplexedConnection,
    channel: &str,
    value: serde_json::Value,
) -> Result<()> {
    conn.publish::<_, _, ()>(channel, value.to_string()).await?;
    Ok(())
}
