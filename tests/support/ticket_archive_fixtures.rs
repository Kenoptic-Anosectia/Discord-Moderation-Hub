// Pair-work scaffold supplied by Directorate. Implemented by evolve.
use serde_json::json;
use serenity::all::{Message, MessageId, Timestamp};

/// Generate synthetic Discord message history in pages of at most 100 messages.
pub fn message_pages(count: usize) -> Vec<Vec<Message>> {
    let mut pages = Vec::new();
    for chunk in message_batch(count).chunks(100) {
        pages.push(chunk.to_vec());
    }
    pages
}

/// Build `count` deterministic synthetic messages with strictly increasing ids.
fn message_batch(count: usize) -> Vec<Message> {
    let base = Timestamp::parse("2026-01-01T00:00:00Z").unwrap();
    let base_secs = base.unix_timestamp();
    (0..count)
        .map(|i| {
            let mut message = Message::default();
            message.id = MessageId::new(10_000 + i as u64);
            message.channel_id = serenity::all::ChannelId::new(1);
            message.author.id = serenity::all::UserId::new(1);
            message.author.name = format!("user_{}", i);
            message.content = format!("synthetic message number {}", i);
            message.timestamp = Timestamp::from_unix_timestamp(base_secs + i as i64).unwrap();
            message
        })
        .collect()
}

/// Synthetic attachment: Serenity `Attachment` metadata plus its payload bytes.
#[derive(Clone)]
pub struct AssetFixture {
    pub attachment: serenity::all::Attachment,
    pub bytes: Vec<u8>,
}

/// Four synthetic assets (no network or filesystem access): two distinct ids sharing
/// `same.txt`, one `../outside.txt`, one `C:\outside.txt`.
pub fn asset_fixtures() -> Vec<AssetFixture> {
    let synthetic = b"synthetic attachment payload".to_vec();
    let synthetic_b = b"synthetic attachment payload two".to_vec();
    let outside = b"outside payload".to_vec();
    let drive_outside = b"drive outside payload".to_vec();

    let make = |id: u64, filename: &str, bytes: Vec<u8>| AssetFixture {
        attachment: serde_json::from_value(json!({
            "id": id.to_string(),
            "filename": filename,
            "proxy_url": "https://example.invalid/proxy",
            "size": bytes.len(),
            "url": "https://example.invalid/asset"
        }))
        .unwrap(),
        bytes,
    };

    vec![
        make(100, "same.txt", synthetic),
        make(101, "same.txt", synthetic_b),
        make(102, "../outside.txt", outside),
        make(103, r"C:\outside.txt", drive_outside),
    ]
}
