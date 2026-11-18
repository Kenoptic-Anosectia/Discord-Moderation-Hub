//! Checks pagination boundaries and stable attachment identities in the fixture factory.
//! The data is synthetic and may include untrusted-looking names; no name is opened as a path.
//! This tests fixture generation, not Discord pagination or production archive handling.

#[path = "support/ticket_archive_fixtures.rs"]
mod fixtures;

/// The 100- and 200-message cases yield full pages only;
/// catches an empty trailing page at an exact page boundary.
#[test]
fn message_pages_at_exact_multiples_of_100() {
    // Exactly 100 => exactly one page of 100, no empty trailing page.
    let page_100 = fixtures::message_pages(100);
    assert_eq!(page_100.len(), 1, "100 messages must be exactly one page");
    assert_eq!(page_100[0].len(), 100);
    assert!(
        page_100.iter().all(|p| !p.is_empty()),
        "no empty trailing page"
    );

    // Exactly 200 => two full pages, no empty trailing page.
    let page_200 = fixtures::message_pages(200);
    assert_eq!(page_200.len(), 2, "200 messages must be exactly two pages");
    for page in &page_200 {
        assert_eq!(page.len(), 100);
        assert!(!page.is_empty(), "no empty trailing page");
    }
}

/// The 99- and 101-message cases retain every expected ID in bounded, nonempty pages;
/// catches dropped or duplicated messages at a page cut.
#[test]
fn message_pages_ids_contiguous_across_boundary() {
    for count in [99usize, 101] {
        let pages = fixtures::message_pages(count);

        // Every page nonempty and at most 100.
        assert!(
            pages.iter().all(|p| !p.is_empty() && p.len() <= 100),
            "each page must be nonempty and at most 100"
        );

        // Total messages equals requested count.
        let total: usize = pages.iter().map(Vec::len).sum();
        assert_eq!(total, count, "total messages must equal requested count");

        // Derive expected IDs in Rust: start at 10000, strictly increasing by 1.
        let expected: Vec<u64> = (0..count).map(|i| 10_000 + i as u64).collect();
        let actual: Vec<u64> = pages.iter().flatten().map(|m| m.id.get()).collect();
        assert_eq!(
            actual, expected,
            "IDs must be contiguous 10000..10000+count"
        );

        // No duplicate or drop: strictly increasing by exactly 1 across the boundary.
        for pair in actual.windows(2) {
            assert_eq!(pair[1] - pair[0], 1, "no duplicate or drop across boundary");
        }
    }
}

/// Assets have unique IDs, accurate lengths, and stable IDs, names, and bytes across calls;
/// catches collapsing attachments with the same display name into one payload.
#[test]
fn asset_fixtures_size_unique_and_deterministic() {
    let assets = fixtures::asset_fixtures();
    assert_eq!(assets.len(), 4, "expected four asset fixtures");

    // Size matches actual byte length, and IDs are unique.
    let mut seen_ids = std::collections::HashSet::new();
    for asset in &assets {
        assert_eq!(
            asset.attachment.size as usize,
            asset.bytes.len(),
            "attachment size must equal actual byte length"
        );
        assert!(
            seen_ids.insert(asset.attachment.id.get()),
            "attachment IDs must be unique"
        );
    }

    // Repeated calls produce identical IDs, names, and bytes.
    let again = fixtures::asset_fixtures();
    assert_eq!(assets.len(), again.len());
    for (a, b) in assets.iter().zip(again.iter()) {
        assert_eq!(a.attachment.id.get(), b.attachment.id.get());
        assert_eq!(a.attachment.filename, b.attachment.filename);
        assert_eq!(a.bytes, b.bytes);
    }

    // The two same.txt assets keep distinct IDs and their own bytes.
    let same_txt: Vec<_> = assets
        .iter()
        .filter(|a| a.attachment.filename == "same.txt")
        .collect();
    assert_eq!(same_txt.len(), 2);
    assert_ne!(
        same_txt[0].attachment.id.get(),
        same_txt[1].attachment.id.get(),
        "same.txt assets must retain distinct IDs"
    );
    assert_ne!(
        same_txt[0].bytes, same_txt[1].bytes,
        "same.txt assets must retain their own bytes"
    );
}
