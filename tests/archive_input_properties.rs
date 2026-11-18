//! Deterministic sweeps of archive URL validation and generated asset names.
//! Tests sampled ID boundaries, signed query preservation, and rejection without diagnostic leaks.
//! All external-looking URLs and private markers are synthetic and treated as untrusted strings.
//! Nothing resolves DNS, opens a socket, or touches the filesystem at runtime.
#![allow(dead_code)] // the module is compiled standalone via #[path]; not every item is used
#[path = "../src/archive_safety.rs"]
mod archive_safety;

use archive_safety::{
    generated_asset_name, validate_asset_component, validate_attachment_url, MAX_ASSET_NAME_LEN,
};
use std::collections::HashSet;

/// Distinct canonical nonzero IDs spanning short numbers and snowflake-sized values.
const URL_IDS: [&str; 8] = [
    "1",
    "42",
    "1000",
    "1000000000",
    "1000000000000000001",
    "2000000000000000002",
    "4000000000000000003",
    "8000000000000000004",
];

/// Synthetic signed query values. These are NOT real Discord signatures or
/// credentials; they are arbitrary opaque strings used only to exercise query
/// preservation.
const SIGNED_QUERIES: [&str; 6] = [
    "ex=65f0aa00&is=65ee2b00&hm=abcdef0123456789abcdef0123456789",
    "ex=deadbeef&is=cafebabe&hm=00112233445566778899aabbccddeeff&sig=xyz",
    "hm=ffffffffffffffff&ex=0&is=0",
    "ex=1&is=2&hm=3&other=a%2Fb%20c&x=",
    "sig=a1b2c3d4&e=9&i=8&m=7",
    "ex=%26%26&is=%3f%3f&hm=%2f%2f",
];

/// Synthetic private marker used only to prove the sanitized error does not
/// echo attacker-controlled input. Not a real secret.
const PRIVATE_MARKER: &str = "LEAK-me-9f8e7d";

// ---- Generated asset names ----

/// Distinct sampled IDs produce stable, distinct names accepted by the component validator;
/// catches truncation, unstable generation, or naming/validation drift.
#[test]
fn generated_names_are_injective_across_id_sweep() {
    let mut ids: Vec<u64> = (1..=4096u64).collect();
    // Nonoverlapping neighborhoods around the boundaries (all > 4096).
    ids.extend([
        (u32::MAX - 4) as u64,
        (u32::MAX - 1) as u64,
        u32::MAX as u64,
        u32::MAX as u64 + 1,
        u32::MAX as u64 + 5,
        u64::MAX - 5,
        u64::MAX - 2,
        u64::MAX - 1,
        u64::MAX,
    ]);
    // Every ID is distinct by construction; assert the multiset is a set.
    let uniq: HashSet<u64> = ids.iter().copied().collect();
    assert_eq!(uniq.len(), ids.len(), "ID set must be distinct");

    // Map each id to its generated name, pairing them so the determinism check
    // is exact regardless of ordering.
    let mut pairs: Vec<(u64, String)> = Vec::with_capacity(ids.len());
    for &id in &ids {
        pairs.push((id, generated_asset_name(id).expect("nonzero id is valid")));
    }

    // Injective: distinct ids -> distinct names.
    let name_set: HashSet<&String> = pairs.iter().map(|(_, n)| n).collect();
    assert_eq!(name_set.len(), pairs.len(), "distinct ids must not collide");

    // Determinism: re-deriving the same id yields the same name.
    for &(id, ref name) in &pairs {
        assert_eq!(
            generated_asset_name(id).unwrap(),
            *name,
            "deterministic re-derivation failed for {id}"
        );
    }

    // Every generated name is accepted by the real component validator.
    for (_, name) in &pairs {
        validate_asset_component(name).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
}

/// Checks uniqueness and exact output around the integer boundaries;
/// catches truncating a 64-bit ID to 32 bits or formatting a boundary incorrectly.
#[test]
fn generated_name_sweep_covers_range_and_boundaries() {
    let ids: Vec<u64> = (1..=4096u64)
        .chain([u32::MAX as u64, u32::MAX as u64 + 1, u64::MAX - 1, u64::MAX])
        .collect();
    let names: HashSet<String> = ids
        .iter()
        .map(|i| generated_asset_name(*i).unwrap())
        .collect();
    // Include all four boundary IDs in the collision check.
    assert_eq!(names.len(), ids.len());
    // The canonical boundary outputs match the documented grammar.
    assert_eq!(
        names.get("attachment-4294967295.bin").map(String::as_str),
        Some("attachment-4294967295.bin")
    );
    assert_eq!(
        names.get("attachment-4294967296.bin").map(String::as_str),
        Some("attachment-4294967296.bin")
    );
    assert_eq!(
        names
            .get("attachment-18446744073709551615.bin")
            .map(String::as_str),
        Some("attachment-18446744073709551615.bin")
    );
}

/// Sampled generated names fit the ASCII grammar and length cap and pass validation;
/// catches emitting an unsafe or unrecognized filename component.
#[test]
fn generated_names_are_ascii_safe_and_grammar_compliant() {
    let ids: Vec<u64> = (1..=4096u64).chain([u32::MAX as u64, u64::MAX]).collect();
    for id in &ids {
        let n = generated_asset_name(*id).unwrap();
        // ASCII-only: every byte is an ASCII printable/safe character.
        assert!(n.bytes().all(|b| b.is_ascii()), "non-ASCII byte in {n:?}");
        // Only lowercase ascii letters, digits, '-', '.' are used.
        assert!(
            n.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.'),
            "unsafe char in {n:?}"
        );
        // Exactly one '.' (the suffix separator), no path/separator/control chars.
        assert_eq!(n.matches('.').count(), 1, "{n}");
        assert!(!n.contains(['/', '\\', ':', '\0']), "{n}");
        // Length bounded by the real cap.
        assert!(n.len() <= MAX_ASSET_NAME_LEN, "{n}");
        // And the real validator accepts it.
        validate_asset_component(&n).unwrap_or_else(|e| panic!("{n}: {e}"));
    }
}

// ---- Accepted attachment URLs ----

/// Both allowed CDN hosts accept the signed fixture sweep without changing URL bytes;
/// catches rewriting opaque query values or rejecting valid attachment hosts.
#[test]
fn valid_signed_cdn_urls_are_accepted_with_query_preserved() {
    let mut seen_ids = HashSet::new();
    for id in URL_IDS {
        assert!(seen_ids.insert(id), "URL_IDS must be distinct: {id}");
    }
    assert!(URL_IDS.len() >= 4, "need several distinct canonical ids");

    for id in URL_IDS {
        for q in SIGNED_QUERIES {
            // Keep the path form fixed so both hosts face the same opaque query inputs.
            for host in ["cdn.discordapp.com", "media.discordapp.net"] {
                let raw =
                    format!("https://{host}/attachments/{id}/1000000000000000002/{id}.png?{q}");
                let u = validate_attachment_url(&raw)
                    .unwrap_or_else(|e| panic!("expected accept {raw:?}: {e}"));
                // Canonical, non-zero ids, correct path.
                assert_eq!(u.scheme(), "https");
                assert_eq!(u.host_str(), Some(host));
                assert_eq!(
                    u.path(),
                    format!("/attachments/{id}/1000000000000000002/{id}.png")
                );
                // Query preserved exactly: not dropped, not rewritten.
                assert_eq!(u.query(), Some(q), "query changed for {raw:?}");
                // Full URL round-trips (query not reordered/canonicalised).
                assert_eq!(u.as_str(), raw, "url rewritten for {raw:?}");
                // No port/fragment/userinfo introduced.
                assert_eq!(u.port(), None);
                assert!(u.fragment().is_none());
                assert!(u.username().is_empty());
            }
        }
    }
}

/// The sampled ephemeral URL is accepted without rewriting its path or query;
/// catches dropping support for that attachment root.
#[test]
fn ephemeral_attachment_url_accepted_with_query_preserved() {
    let id = "1000000000000000003";
    let q = "ex=1&is=2&hm=3";
    let raw = format!(
        "https://cdn.discordapp.com/ephemeral-attachments/1000000000000000002/{id}/a.bin?{q}"
    );
    let u = validate_attachment_url(&raw).expect("ephemeral accepted");
    assert_eq!(
        u.path(),
        format!("/ephemeral-attachments/1000000000000000002/{id}/a.bin")
    );
    assert_eq!(u.query(), Some(q));
    assert_eq!(u.as_str(), raw);
}

// ---- Rejected URLs and sanitized errors ----

/// Host, port, and path variants fail without echoing the tested marker or host fragment;
/// catches reflecting those private input fragments in Display or Debug.
#[test]
fn invalid_variants_rejected_and_error_never_echoes_marker() {
    let id = "1000000000000000004";
    let ch = "1000000000000000002";

    // Invalid host variants (lookalikes / disallowed hosts).
    let bad_hosts = [
        "cdn.discordapp.com.evil.example",
        "evil.cdn.discordapp.com",
        "cdn.discord.com",
        "127.0.0.1",
        "[::1]",
        "localhost",
        "discordapp.com",
    ];
    // Invalid port variants (non-default explicit ports).
    let bad_ports = ["80", "8443", "444", "65535"];
    // Invalid path variants (wrong root, non-numeric id, traversal).
    let bad_paths = [
        "/api/webhooks/1/token",
        "/attachments/{ch}/{id}",
        "/attachments/{ch}/{id}/",
        "/attachments/notanumber/{id}/a.bin",
        "/attachments/{ch}/0/a.bin",
        "/attachments/{ch}/{id}/../../etc/passwd",
        "/Attachments/{ch}/{id}/a.bin",
    ];

    let mut variants: Vec<String> = Vec::new();
    for host in bad_hosts {
        variants.push(format!(
            "https://{host}/attachments/{ch}/{id}/a.bin?hm={PRIVATE_MARKER}"
        ));
    }
    for port in bad_ports {
        variants.push(format!(
            "https://cdn.discordapp.com:{port}/attachments/{ch}/{id}/a.bin?hm={PRIVATE_MARKER}"
        ));
    }
    for p in bad_paths {
        let path = p.replace("{ch}", ch).replace("{id}", id);
        variants.push(format!(
            "https://cdn.discordapp.com{path}?hm={PRIVATE_MARKER}"
        ));
    }

    assert!(variants.len() >= 10, "need a meaningful sweep of variants");

    for raw in &variants {
        let e = validate_attachment_url(raw).expect_err(&format!("expected rejection: {raw:?}"));
        // Rejection carries a fixed kind; confirm the sanitized error never echoes.
        for text in [e.to_string(), format!("{e:?}")] {
            // The sanitized error must not echo the private marker...
            assert!(!text.contains(PRIVATE_MARKER), "leaked marker: {text:?}");
            // Also check the disallowed host fragment independently of the query marker.
            assert!(!text.contains("evil.example"), "leaked host: {text:?}");
            assert!(!text.is_empty());
        }
    }
}

/// A newline-suffixed URL fails without reflecting the marker or host;
/// catches silently normalizing the control character or leaking input in diagnostics.
#[test]
fn control_variant_with_marker_rejected_and_not_echoed() {
    let raw = format!(
        "https://cdn.discordapp.com/attachments/1000000000000000002/1000000000000000004/a.bin?hm={PRIVATE_MARKER}\n"
    );
    let e = validate_attachment_url(&raw).expect_err("control variant must be rejected");
    for text in [e.to_string(), format!("{e:?}")] {
        assert!(!text.contains(PRIVATE_MARKER), "leaked marker: {text:?}");
        assert!(
            !text.contains("cdn.discordapp.com"),
            "leaked host: {text:?}"
        );
    }
}
