//! Builds deterministic Serenity history and attachment fixtures for archive tests.
//! Uses invented IDs, text, bytes, and URLs; never loads real messages or attachment data.
//! Path-like filenames are untrusted display metadata, never paths to read or write.

use serde_json::json;
use serenity::all::{Message, MessageId, Timestamp};

/// Returns ascending synthetic history split at the 100-message page size, or no pages for zero.
/// Intended for fixture-sized counts that fit memory and the timestamp range.
pub fn message_pages(count: usize) -> Vec<Vec<Message>> {
    let mut pages = Vec::new();
    for chunk in message_batch(count).chunks(100) {
        pages.push(chunk.to_vec());
    }
    pages
}

/// Builds messages in one synthetic channel with increasing IDs and one-second timestamps.
/// Uses a fixed epoch for reproducibility; panics if a generated timestamp is out of range.
fn message_batch(count: usize) -> Vec<Message> {
    // Fixed time keeps snapshots reproducible instead of depending on when a test runs.
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
    /// Serenity wire metadata; the original filename is display data, not a safe path.
    pub attachment: serenity::all::Attachment,
    /// In-memory payload whose length matches the fixture's declared attachment size.
    pub bytes: Vec<u8>,
}

/// Returns four local payloads covering shared display names and path-shaped metadata.
/// The URLs are placeholders, not download targets; panics if fixture metadata fails to decode.
pub fn asset_fixtures() -> Vec<AssetFixture> {
    let synthetic = b"synthetic attachment payload".to_vec();
    let synthetic_b = b"synthetic attachment payload two".to_vec();
    let outside = b"outside payload".to_vec();
    let drive_outside = b"drive outside payload".to_vec();

    // Deserialize wire-shaped metadata rather than assuming Serenity's defaults match the payload.
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
