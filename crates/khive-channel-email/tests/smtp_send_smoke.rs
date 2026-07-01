use khive_channel::{Channel, ChannelEnvelope};
use khive_channel_email::{EmailChannel, EmailChannelConfig};
use std::time::SystemTime;

#[tokio::test]
#[ignore = "live network + real credentials; run manually with --ignored"]
async fn smtp_send_to_maintainer_smoke() {
    let config = EmailChannelConfig::from_env().expect("email config must load from env");
    let channel = EmailChannel::from_env().expect("email channel must build from env");

    let ts = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let envelope = ChannelEnvelope::new(
        format!("email:{}", config.mailbox),
        format!("email:{}", config.maintainer_addresses[0]),
        format!("khive email channel smoke test — sent at unix {ts}"),
    )
    .with_subject("khive email channel smoke test");

    channel.send(envelope).await.expect("send must succeed");
}
