//! Tests for the archive input-safety primitives. All inputs are synthetic;
//! nothing here resolves DNS, opens a socket or touches the filesystem.
#![allow(dead_code)] // the module is compiled standalone via #[path]; not every item is used here
#[path = "../src/archive_safety.rs"]
mod archive_safety;

use archive_safety::{
    generated_asset_name, validate_asset_component, validate_attachment_url,
    ArchiveInputErrorKind as K, MAX_ASSET_NAME_LEN, MAX_URL_LEN,
};
use std::collections::HashSet;

const ID: &str = "1100000000000000001";
const CH: &str = "1000000000000000002";

fn url_err(raw: &str) -> K {
    match validate_attachment_url(raw) {
        Ok(u) => panic!(
            "expected rejection, accepted: kind-only report (host {:?})",
            u.host_str()
        ),
        Err(e) => e.kind(),
    }
}

// ---------- URL: accepted ----------

#[test]
fn accepts_signed_cdn_url_and_preserves_query_exactly() {
    let q = "ex=65f0aa00&is=65ee2b00&hm=abcdef0123456789abcdef0123456789&other=a%2Fb%20c&x=";
    let raw = format!("https://cdn.discordapp.com/attachments/{CH}/{ID}/report.png?{q}");
    let u = validate_attachment_url(&raw).expect("valid");
    assert_eq!(u.scheme(), "https");
    assert_eq!(u.host_str(), Some("cdn.discordapp.com"));
    assert_eq!(u.query(), Some(q));
    assert_eq!(u.path(), format!("/attachments/{CH}/{ID}/report.png"));
    assert_eq!(u.as_str(), raw);
}

#[test]
fn accepts_media_proxy_host_and_unsigned_url() {
    let raw = format!("https://media.discordapp.net/attachments/{CH}/{ID}/a.txt");
    let u = validate_attachment_url(&raw).expect("valid");
    assert_eq!(u.host_str(), Some("media.discordapp.net"));
    assert_eq!(u.query(), None);
}

#[test]
fn accepts_ephemeral_attachments_path_deliberately() {
    let raw =
        format!("https://cdn.discordapp.com/ephemeral-attachments/{CH}/{ID}/a.bin?ex=1&is=2&hm=3");
    assert!(validate_attachment_url(&raw).is_ok());
}

#[test]
fn host_case_and_explicit_default_port_normalise_to_approved_host() {
    let raw = format!("HTTPS://CDN.DiscordApp.COM:443/attachments/{CH}/{ID}/a.bin");
    let u = validate_attachment_url(&raw).expect("valid");
    assert_eq!(u.host_str(), Some("cdn.discordapp.com"));
    assert_eq!(u.port(), None);
}

// ---------- URL: rejected ----------

#[test]
fn rejects_non_https_schemes_and_relative_inputs() {
    let tail = format!("cdn.discordapp.com/attachments/{CH}/{ID}/a.bin");
    for raw in [
        format!("http://{tail}"),
        format!("ftp://{tail}"),
        format!("file:///{tail}"),
        format!("//{tail}"),
        format!("/attachments/{CH}/{ID}/a.bin"),
        format!("attachments/{CH}/{ID}/a.bin"),
        tail.clone(),
        String::new(),
        "https://".to_string(),
        "data:text/plain,hi".to_string(),
        "javascript:alert(1)".to_string(),
    ] {
        let k = url_err(&raw);
        assert!(
            matches!(k, K::InvalidUrl | K::UnsupportedScheme),
            "{raw:?} -> {k:?}"
        );
    }
    assert_eq!(url_err(&format!("http://{tail}")), K::UnsupportedScheme);
}

#[test]
fn rejects_lookalike_subdomain_and_parent_hosts() {
    for host in [
        "cdn.discordapp.com.evil.example",
        "evil.cdn.discordapp.com",
        "cdn.discordapp.com.",
        "discordapp.com",
        "evilcdn.discordapp.com",
        "cdn-discordapp.com",
        "cdn.discordapp.co",
        "cdn.discord.com",
        "discord.com",
        "media.discordapp.net.evil.example",
        "xmedia.discordapp.net",
        "cdn.dіscordapp.com", // Cyrillic i
        "cdn.discordapp.com%2eevil.example",
    ] {
        let raw = format!("https://{host}/attachments/{CH}/{ID}/a.bin");
        let k = url_err(&raw);
        assert!(
            matches!(k, K::HostNotAllowed | K::InvalidUrl),
            "{host} -> {k:?}"
        );
    }
}

#[test]
fn rejects_userinfo_including_host_in_userinfo() {
    for raw in [
        format!("https://cdn.discordapp.com@evil.example/attachments/{CH}/{ID}/a.bin"),
        format!("https://evil.example@cdn.discordapp.com/attachments/{CH}/{ID}/a.bin"),
        format!("https://user:pw@cdn.discordapp.com/attachments/{CH}/{ID}/a.bin"),
        format!("https://:@cdn.discordapp.com/attachments/{CH}/{ID}/a.bin"),
        format!("https://cdn.discordapp.com:pw@evil.example/attachments/{CH}/{ID}/a.bin"),
    ] {
        let k = url_err(&raw);
        assert!(
            matches!(k, K::UserInfoNotAllowed | K::HostNotAllowed),
            "{raw:?} -> {k:?}"
        );
    }
    assert_eq!(
        url_err(&format!(
            "https://user:pw@cdn.discordapp.com/attachments/{CH}/{ID}/a.bin"
        )),
        K::UserInfoNotAllowed
    );
}

#[test]
fn rejects_ip_literals_in_every_encoding() {
    for host in [
        "127.0.0.1",
        "[::1]",
        "[::ffff:127.0.0.1]",
        "2130706433",
        "0x7f.1",
        "0177.0.0.1",
        "169.254.169.254",
        "localhost",
    ] {
        let raw = format!("https://{host}/attachments/{CH}/{ID}/a.bin");
        let k = url_err(&raw);
        assert!(
            matches!(k, K::HostNotAllowed | K::InvalidUrl),
            "{host} -> {k:?}"
        );
    }
}

#[test]
fn rejects_non_default_ports() {
    for port in ["80", "8443", "444", "0", "65535"] {
        let raw = format!("https://cdn.discordapp.com:{port}/attachments/{CH}/{ID}/a.bin");
        assert_eq!(url_err(&raw), K::PortNotAllowed, "port {port}");
    }
}

#[test]
fn rejects_fragments() {
    for suffix in ["#frag", "#", "?ex=1#frag"] {
        let raw = format!("https://cdn.discordapp.com/attachments/{CH}/{ID}/a.bin{suffix}");
        assert_eq!(url_err(&raw), K::FragmentNotAllowed, "{suffix}");
    }
}

#[test]
fn rejects_controls_whitespace_and_newline_variants() {
    let base = format!("https://cdn.discordapp.com/attachments/{CH}/{ID}/a.bin");
    for bad in [
        format!("{base}\n"),
        format!("{base}\r\n"),
        format!("\n{base}"),
        format!("{base}\0"),
        format!("{base}\t"),
        format!("https://cdn.discord\napp.com/attachments/{CH}/{ID}/a.bin"),
        format!("https://cdn.discordapp.com/attachments/{CH}/{ID}/a\r\nHost: evil.example"),
        format!("{base}?ex=1\r\nX: y"),
        format!("{base}\u{7f}"),
        format!("{base}\u{1b}[31m"),
        format!(" {base}"),
        format!("{base} "),
    ] {
        assert_eq!(url_err(&bad), K::ControlCharacter, "{bad:?}");
    }
}

#[test]
fn rejects_non_attachment_paths() {
    for path in [
        "/".to_string(),
        "/api/webhooks/1/token".to_string(),
        "/api/v10/channels/1/messages".to_string(),
        "/avatars/1/abc.png".to_string(),
        "/emojis/1.png".to_string(),
        "/attachments".to_string(),
        "/attachments/".to_string(),
        format!("/attachments/{CH}/{ID}"),
        format!("/attachments/{CH}/{ID}/"),
        format!("/attachments/{CH}/{ID}/a.bin/extra"),
        format!("/Attachments/{CH}/{ID}/a.bin"),
        format!("/attachments/notanumber/{ID}/a.bin"),
        format!("/attachments/{CH}/notanumber/a.bin"),
        format!("/attachments/{CH}/0/a.bin"),
        format!("/attachments/{CH}/99999999999999999999999/a.bin"),
        format!("/attachments//{ID}/a.bin"),
        format!("//attachments/{CH}/{ID}/a.bin"),
    ] {
        let raw = format!("https://cdn.discordapp.com{path}");
        assert_eq!(url_err(&raw), K::PathNotAllowed, "{path}");
    }
}

#[test]
fn rejects_dot_segments_encoded_separators_and_backslashes() {
    for path in [
        format!("/attachments/{CH}/{ID}/../../../api/webhooks/1/t"),
        format!("/attachments/{CH}/{ID}/.."),
        format!("/attachments/{CH}/{ID}/."),
        format!("/attachments/{CH}/{ID}/%2e%2e"),
        format!("/attachments/{CH}/{ID}/%2E%2E/x"),
        format!("/attachments/{CH}/{ID}/a%2Fb.bin"),
        format!("/attachments/{CH}/{ID}/a%2fb.bin"),
        format!("/attachments/{CH}/{ID}/a%5Cb.bin"),
        format!("/attachments/{CH}/{ID}/a\\b.bin"),
        format!("/attachments/{CH}/{ID}/a%00b.bin"),
        format!("/attachments/{CH}/../{ID}/a.bin"),
        format!("/attachments\\{CH}/{ID}/a.bin"),
    ] {
        let raw = format!("https://cdn.discordapp.com{path}");
        let k = url_err(&raw);
        assert!(
            matches!(k, K::PathNotAllowed | K::ControlCharacter),
            "{path} -> {k:?}"
        );
    }
}

#[test]
fn rejects_overlong_urls_without_parsing_them() {
    let pad = "a".repeat(MAX_URL_LEN);
    let raw = format!("https://cdn.discordapp.com/attachments/{CH}/{ID}/a.bin?{pad}");
    assert_eq!(url_err(&raw), K::TooLong);
    let exact_len_ok = {
        let prefix = format!("https://cdn.discordapp.com/attachments/{CH}/{ID}/a.bin?q=");
        let fill = "b".repeat(MAX_URL_LEN - prefix.len());
        format!("{prefix}{fill}")
    };
    assert_eq!(exact_len_ok.len(), MAX_URL_LEN);
    assert!(validate_attachment_url(&exact_len_ok).is_ok());
    assert_eq!(url_err(&format!("{exact_len_ok}b")), K::TooLong);
}

// ---------- error display is sanitised ----------

#[test]
fn error_display_and_debug_never_echo_input() {
    let secret = "SIGNEDSECRET123hm";
    let evil_name = "evil-original-name.exe";
    for raw in [
        format!(
            "https://cdn.discordapp.com.evil.example/attachments/{CH}/{ID}/{evil_name}?hm={secret}"
        ),
        format!("http://cdn.discordapp.com/attachments/{CH}/{ID}/{evil_name}?hm={secret}"),
        format!("https://u:p@cdn.discordapp.com/attachments/{CH}/{ID}/{evil_name}?hm={secret}"),
        format!("https://cdn.discordapp.com/attachments/{CH}/{ID}/{evil_name}?hm={secret}#f"),
        format!("https://cdn.discordapp.com/api/{evil_name}?hm={secret}\n"),
        format!("not a url {evil_name} {secret}"),
        format!("{}{secret}", "x".repeat(MAX_URL_LEN)),
    ] {
        let e = validate_attachment_url(&raw).unwrap_err();
        for text in [e.to_string(), format!("{e:?}")] {
            assert!(!text.contains(secret), "{text}");
            assert!(!text.contains(evil_name), "{text}");
            assert!(!text.contains("evil.example"), "{text}");
            assert!(!text.contains("u:p"), "{text}");
            assert!(!text.is_empty());
        }
    }
    let e = validate_asset_component("../../evil-original-name.exe").unwrap_err();
    assert!(!e.to_string().contains("evil"));
    assert!(!format!("{e:?}").contains("evil"));
}

#[test]
fn error_display_is_distinct_per_kind_and_is_std_error() {
    let a = validate_attachment_url("http://x").unwrap_err();
    let b = validate_attachment_url("").unwrap_err();
    assert_ne!(a.kind(), b.kind());
    assert_ne!(a.to_string(), b.to_string());
    let _boxed: Box<dyn std::error::Error + Send + Sync> = Box::new(a);
}

// ---------- generated names ----------

#[test]
fn generated_name_known_values_and_u64_boundaries() {
    assert_eq!(generated_asset_name(1).unwrap(), "attachment-1.bin");
    assert_eq!(generated_asset_name(123).unwrap(), "attachment-123.bin");
    assert_eq!(
        generated_asset_name(u64::MAX).unwrap(),
        "attachment-18446744073709551615.bin"
    );
    assert_eq!(
        generated_asset_name(u64::MAX - 1).unwrap(),
        "attachment-18446744073709551614.bin"
    );
    assert_eq!(
        generated_asset_name(i64::MAX as u64).unwrap(),
        "attachment-9223372036854775807.bin"
    );
    assert_eq!(
        generated_asset_name(i64::MAX as u64 + 1).unwrap(),
        "attachment-9223372036854775808.bin"
    );
    assert_eq!(
        generated_asset_name(u32::MAX as u64 + 1).unwrap(),
        "attachment-4294967296.bin"
    );
}

#[test]
fn generated_name_rejects_zero() {
    assert_eq!(
        generated_asset_name(0).unwrap_err().kind(),
        K::InvalidAttachmentId
    );
}

#[test]
fn generated_name_is_deterministic() {
    for id in [1u64, 42, u64::MAX, 1 << 63] {
        assert_eq!(
            generated_asset_name(id).unwrap(),
            generated_asset_name(id).unwrap()
        );
    }
}

#[test]
fn distinct_ids_never_collide_even_case_insensitively() {
    let ids: Vec<u64> = (1..=2000u64)
        .chain([
            10,
            100,
            1000,
            10_000,
            u32::MAX as u64,
            u32::MAX as u64 + 1,
            u64::MAX - 2,
            u64::MAX - 1,
            u64::MAX,
        ])
        .collect();
    let uniq_ids: HashSet<u64> = ids.iter().copied().collect();
    let names: HashSet<String> = ids
        .iter()
        .map(|i| generated_asset_name(*i).unwrap().to_ascii_lowercase())
        .collect();
    assert_eq!(names.len(), uniq_ids.len());
}

#[test]
fn generated_names_satisfy_the_component_validator_and_ascii_grammar() {
    for id in [1u64, 7, 99, 100, 123456789, i64::MAX as u64, u64::MAX] {
        let n = generated_asset_name(id).unwrap();
        validate_asset_component(&n).unwrap();
        assert!(n.len() <= MAX_ASSET_NAME_LEN);
        assert!(n.starts_with("attachment-") && n.ends_with(".bin"));
        assert!(n
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.'));
        assert_eq!(n.matches('.').count(), 1);
        assert!(!n.contains(['/', '\\', ':', '\0']));
    }
}

#[test]
fn generated_name_has_no_input_channel_for_original_filenames() {
    // The signature takes only a u64: hostile original names cannot influence it. Pair each hostile
    // name with the same ID and show the (name-independent) result is one stable safe component.
    let hostile = [
        "../../etc/passwd",
        "..\\..\\Windows\\System32\\config\\SAM",
        "C:\\Windows\\win.ini",
        "CON",
        "NUL.txt",
        "a.txt:stream",
        "\\\\server\\share\\x",
        "x\u{202e}gnp.exe",
        "report.png",
        "report.png.exe",
    ];
    let expected = generated_asset_name(777).unwrap();
    for _name in hostile {
        assert_eq!(generated_asset_name(777).unwrap(), expected);
    }
    assert_eq!(expected, "attachment-777.bin");
}

// ---------- component validator ----------

#[test]
fn component_accepts_exact_grammar_only() {
    for ok in [
        "attachment-1.bin",
        "attachment-123.bin",
        "attachment-18446744073709551615.bin",
        "attachment-9.bin",
    ] {
        validate_asset_component(ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
    }
}

#[test]
fn component_rejects_u64_overflow_leading_zero_and_zero() {
    for bad in [
        "attachment-18446744073709551616.bin",
        "attachment-99999999999999999999999.bin",
        "attachment-0.bin",
        "attachment-00.bin",
        "attachment-007.bin",
        "attachment-.bin",
        "attachment-+5.bin",
        "attachment--5.bin",
    ] {
        assert!(validate_asset_component(bad).is_err(), "{bad}");
    }
}

#[test]
fn component_rejects_empty_dot_dotdot_and_separators() {
    for bad in [
        "",
        ".",
        "..",
        "...",
        " ",
        "/",
        "\\",
        "a/b",
        "a\\b",
        "/attachment-1.bin",
        "\\attachment-1.bin",
        "attachment-1.bin/",
        "attachment-1.bin\\",
        "./attachment-1.bin",
        "../attachment-1.bin",
        "..\\attachment-1.bin",
        "sub/attachment-1.bin",
        "sub\\attachment-1.bin",
        "attachment-1.bin/..",
    ] {
        assert!(validate_asset_component(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn component_rejects_windows_path_attack_corpus() {
    for bad in [
        "C:",
        "C:\\",
        "C:\\Windows\\win.ini",
        "C:attachment-1.bin",
        "c:/x",
        "\\\\server\\share\\f",
        "//server/share/f",
        "\\\\?\\C:\\x",
        "\\\\.\\PhysicalDrive0",
        "\\\\?\\UNC\\s\\h",
        "//?/C:/x",
        "\\??\\C:\\x",
        "CON",
        "con",
        "PRN",
        "AUX",
        "NUL",
        "COM1",
        "com9",
        "LPT1",
        "lpt9",
        "COM\u{b9}",
        "CONIN$",
        "CONOUT$",
        "CON.txt",
        "NUL.bin",
        "nul.tar.gz",
        "COM1.bin",
        "attachment-1.bin:stream",
        "attachment-1.bin:$DATA",
        "attachment-1.bin::$DATA",
        "attachment-1.bin.",
        "attachment-1.bin ",
        " attachment-1.bin",
        "attachment-1.bin.exe",
        "attachment-1.bin.bin",
        "ATTACHMENT-1.BIN",
        "Attachment-1.bin",
        "attachment-1.BIN",
        "attachment_1.bin",
        "attachment-1.bin~1",
        "ATTACH~1.BIN",
        "~1",
        "attachment-1.bin\0",
        "attachment-1.bin\0.txt",
        "attachment-1\0.bin",
        "attachment-1.bin\n",
        "attachment-1.bin\r",
        "attachment-1.bin\t",
        "attachment-\u{7f}1.bin",
        "attachment-1.bin\u{1b}",
        "%2e%2e",
        "%2e%2e%2fattachment-1.bin",
        "attachment-1.bin?",
        "attachment-1.bin*",
        "attachment-1.bin|",
        "attachment-1.bin<",
        "attachment-1.bin>",
        "attachment-1.bin\"",
        "attachment-1.bin#",
        "attachment-1.bin%",
    ] {
        assert!(validate_asset_component(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn component_rejects_unicode_confusables_and_non_ascii() {
    for bad in [
        "attachment-1.bin\u{200b}", // zero width space
        "attachment\u{2010}1.bin",  // hyphen lookalike
        "attachment\u{2212}1.bin",  // minus sign
        "attachment\u{ff0d}1.bin",  // fullwidth hyphen
        "attachment-\u{ff11}.bin",  // fullwidth digit one
        "attachment-\u{0661}.bin",  // arabic-indic digit one
        "attachment-1\u{ff0e}bin",  // fullwidth full stop
        "attachment-1\u{2024}bin",  // one dot leader
        "аttachment-1.bin",         // Cyrillic a
        "attachment-1.bіn",         // Cyrillic i
        "attachment-1.bin\u{202e}", // RLO
        "\u{202e}attachment-1.bin",
        "attachment-1\u{ff0f}bin",  // fullwidth solidus
        "attachment-1.bin\u{fe55}", // small colon
        "attachment-1.bin\u{ff1a}", // fullwidth colon
        "attachment-1.bin\u{2215}", // division slash
        "attachment-1.bin\u{29f5}", // reverse solidus operator
        "\u{ff0e}\u{ff0e}",         // fullwidth ..
        "\u{feff}attachment-1.bin", // BOM
        "attachment-1.bin\u{0301}", // combining accent
    ] {
        assert!(validate_asset_component(bad).is_err(), "{bad:?}");
    }
}

#[test]
fn component_length_is_bounded() {
    // The longest legal name is u64::MAX, well under the cap.
    assert!(generated_asset_name(u64::MAX).unwrap().len() <= MAX_ASSET_NAME_LEN);
    for n in [MAX_ASSET_NAME_LEN + 1, 255, 4096, 1 << 20] {
        let long = format!("attachment-{}.bin", "1".repeat(n));
        assert_eq!(
            validate_asset_component(&long).unwrap_err().kind(),
            K::TooLong,
            "{n}"
        );
    }
    assert_eq!(
        validate_asset_component(&"a".repeat(1 << 16))
            .unwrap_err()
            .kind(),
        K::TooLong
    );
}

#[test]
fn component_rejection_kinds_are_typed() {
    assert_eq!(
        validate_asset_component("").unwrap_err().kind(),
        K::InvalidAssetName
    );
    assert_eq!(
        validate_asset_component("..").unwrap_err().kind(),
        K::InvalidAssetName
    );
    assert_eq!(
        validate_asset_component("a/b").unwrap_err().kind(),
        K::InvalidAssetName
    );
}
