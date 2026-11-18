//! Checks generated ticket history and attachment metadata against Serenity's wire models.
//! All messages, attachment names, and payloads are synthetic; names are data, not paths.
//! No archive writes, downloads, or live Discord operations are exercised.

#[path = "support/ticket_archive_fixtures.rs"]
mod fixtures;

/// A 205-message fixture yields three ordered pages and preserves IDs and content on JSON decode;
/// catches dropped messages, bad page cuts, or incompatible message fixtures.
#[test]
fn generated_history_pages_preserve_every_message_in_numeric_order() {
    let pages = fixtures::message_pages(205);
    assert_eq!(
        pages.iter().map(Vec::len).collect::<Vec<_>>(),
        [100, 100, 5]
    );
    let messages = pages.iter().flatten().collect::<Vec<_>>();
    assert_eq!(messages.len(), 205);
    assert!(messages.windows(2).all(|p| p[0].id.get() < p[1].id.get()));
    for message in messages {
        let wire = serde_json::to_value(message).unwrap();
        let decoded: serenity::all::Message = serde_json::from_value(wire).unwrap();
        assert_eq!(decoded.id, message.id);
        assert_eq!(decoded.content, message.content);
        assert!(!decoded.content.is_empty());
    }
}

/// Zero messages yield no pages; catches emitting an empty page as a sentinel.
#[test]
fn empty_history_generates_no_pages() {
    let pages = fixtures::message_pages(0);
    assert!(pages.is_empty());
}

/// Same-count generation yields equal message fields and JSON values;
/// catches timestamps or fields that vary between fixture runs.
#[test]
fn repeated_generation_is_deterministic() {
    let first = fixtures::message_pages(50);
    let second = fixtures::message_pages(50);
    assert_eq!(first.len(), second.len());
    for (a, b) in first.iter().zip(second.iter()) {
        assert_eq!(a.len(), b.len());
        for (m, n) in a.iter().zip(b.iter()) {
            assert_eq!(m.id, n.id);
            assert_eq!(m.content, n.content);
            assert_eq!(m.author.name, n.author.name);
            assert_eq!(m.timestamp, n.timestamp);
        }
    }
    // Compare full wire values too so changes outside the selected fields are visible.
    let wire_first: Vec<_> = first
        .iter()
        .flatten()
        .map(|m| serde_json::to_value(m).unwrap())
        .collect();
    let wire_second: Vec<_> = second
        .iter()
        .flatten()
        .map(|m| serde_json::to_value(m).unwrap())
        .collect();
    assert_eq!(wire_first, wire_second);
}

/// Checks the four attachment fixtures, their collision and path-shaped cases, and JSON decode;
/// catches losing a negative case or changing its metadata or sample bytes.
#[test]
fn attachment_fixtures_have_expected_assets() {
    let assets = fixtures::asset_fixtures();
    assert_eq!(assets.len(), 4);
    // two distinct IDs sharing original filename same.txt
    let same_txt: Vec<_> = assets
        .iter()
        .filter(|a| a.attachment.filename == "same.txt")
        .collect();
    assert_eq!(same_txt.len(), 2);
    let ids: Vec<_> = same_txt.iter().map(|a| a.attachment.id.get()).collect();
    assert_ne!(ids[0], ids[1]);
    // Keep path-shaped names in metadata so consumers can test treating them as untrusted.
    let outside = assets
        .iter()
        .find(|a| a.attachment.filename == "../outside.txt")
        .expect("../outside.txt");
    let drive_outside = assets
        .iter()
        .find(|a| a.attachment.filename == r"C:\outside.txt")
        .expect("C:\\outside.txt");
    // bytes equal expected synthetic content and size matches
    assert_eq!(outside.bytes, b"outside payload".to_vec());
    assert_eq!(outside.attachment.size, outside.bytes.len() as u32);
    assert_eq!(drive_outside.bytes, b"drive outside payload".to_vec());
    assert_eq!(
        drive_outside.attachment.size,
        drive_outside.bytes.len() as u32
    );
    // round-trip through Serenity serialization
    for asset in &assets {
        let wire = serde_json::to_value(&asset.attachment).unwrap();
        let decoded: serenity::all::Attachment = serde_json::from_value(wire).unwrap();
        assert_eq!(decoded.id, asset.attachment.id);
        assert_eq!(decoded.filename, asset.attachment.filename);
        assert_eq!(decoded.size, asset.attachment.size);
    }
}

/// A synthetic message can carry the first attachment through Serenity JSON;
/// catches a fixture that only round-trips as a standalone attachment.
#[test]
fn attachment_can_attach_to_generated_message() {
    let pages = fixtures::message_pages(1);
    let assets = fixtures::asset_fixtures();
    let message = pages.into_iter().flatten().next().unwrap();
    let mut attached = message.clone();
    attached.attachments = vec![assets[0].attachment.clone()];
    let wire = serde_json::to_value(&attached).unwrap();
    let decoded: serenity::all::Message = serde_json::from_value(wire).unwrap();
    assert_eq!(decoded.attachments.len(), 1);
    assert_eq!(decoded.attachments[0].filename, "same.txt");
}
