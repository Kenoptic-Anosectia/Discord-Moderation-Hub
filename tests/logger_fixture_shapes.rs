//! Checks synthetic logger event fixtures against the locked Serenity event models.
//! Fixture JSON is untrusted input to a test-only validator, not captured messages or cache data.
//! Does not run logger handlers, read a live cache, or send anything to Discord.

use serde::Deserialize;
use serde_json::{json, Value};
use serenity::model::{
    channel::Embed,
    event::{MessageDeleteBulkEvent, MessageDeleteEvent, MessageUpdateEvent},
    id::MessageId,
};
use std::collections::HashSet;

const PACK: &str = include_str!("fixtures/logger/message_events.json");

/// Test-only fixture envelope; unknown fields are rejected to catch schema drift.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Pack {
    provenance: String,
    cases: Vec<Case>,
}

/// Pairs one raw event with the expected wire shape and minimal synthetic cache setup.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    name: String,
    kind: Kind,
    content_shape: ContentShape,
    // Deliberately only a cache SETUP summary, not a Serenity Message or copied logger.
    prior_cache: Vec<Prior>,
    event: Value,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Kind {
    Update,
    Delete,
    BulkDelete,
}

/// Expected raw content presence; null is not a valid substitute for an absent key here.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ContentShape {
    Absent,
    Empty,
    Text,
}

/// Minimal synthetic cache setup, not an event or a full cached message.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Prior {
    id: MessageId,
    content: String,
    embeds: Vec<Embed>,
}

/// Parses a complete synthetic pack or rejects malformed or contradictory fixture input.
/// Error strings are test diagnostics, not suitable for logging arbitrary live payloads.
fn validate_pack(input: &str) -> Result<Pack, String> {
    let p: Pack = serde_json::from_str(input).map_err(|e| e.to_string())?;
    if !p.provenance.starts_with("SYNTHETIC:") || p.cases.is_empty() {
        return Err("synthetic provenance and nonempty cases required".into());
    }
    let mut names = HashSet::new();
    for c in &p.cases {
        if c.name.is_empty() || !names.insert(&c.name) {
            return Err("empty or duplicate scenario name".into());
        }
        // Validate raw presence first: the typed event can collapse distinct wire forms.
        let content_matches = match c.content_shape {
            ContentShape::Absent => c.event.get("content").is_none(),
            ContentShape::Empty => c.event.get("content").and_then(Value::as_str) == Some(""),
            ContentShape::Text => c
                .event
                .get("content")
                .and_then(Value::as_str)
                .is_some_and(|text| !text.is_empty()),
        };
        if !content_matches {
            return Err(format!("{}: contradictory content_shape", c.name));
        }
        // Deserialize the real locked public event types, never a local event lookalike.
        match c.kind {
            Kind::Update => {
                serde_json::from_value::<MessageUpdateEvent>(c.event.clone())
                    .map_err(|e| format!("{}: {e}", c.name))?;
            }
            Kind::Delete => {
                serde_json::from_value::<MessageDeleteEvent>(c.event.clone())
                    .map_err(|e| format!("{}: {e}", c.name))?;
            }
            Kind::BulkDelete => {
                serde_json::from_value::<MessageDeleteBulkEvent>(c.event.clone())
                    .map_err(|e| format!("{}: {e}", c.name))?;
            }
        }
        let mut ids = HashSet::new();
        for prior in &c.prior_cache {
            if !ids.insert(prior.id) || !prior.content.starts_with("SYNTHETIC ") {
                return Err(format!("{}: invalid synthetic cache setup", c.name));
            }
        }
    }
    Ok(p)
}

/// Loads the checked-in fixture pack; panics on fixture drift rather than skipping scenarios.
fn pack() -> Pack {
    validate_pack(PACK).expect("final synthetic fixture pack must validate")
}

/// Looks up a unique named scenario; panics on an invalid pack or missing name.
fn case(name: &str) -> Case {
    pack().cases.into_iter().find(|c| c.name == name).unwrap()
}

/// Decodes a fixture known to be an update; panics if the caller passes the wrong shape.
fn update(c: &Case) -> MessageUpdateEvent {
    serde_json::from_value(c.event.clone()).unwrap()
}

/// The validated pack contains exactly the expected six named cases;
/// catches a silently omitted or substituted scenario.
#[test]
fn final_pack_has_exact_six_named_scenarios() {
    let p = pack();
    let names: HashSet<_> = p.cases.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(
        names,
        HashSet::from([
            "partial_content_absent",
            "explicit_empty_content",
            "embed_only_update",
            "no_cached_message",
            "single_delete",
            "bulk_delete",
        ])
    );
    assert_eq!(p.cases.len(), 6);
}

/// Raw JSON and typed updates distinguish omitted content from an explicit empty string;
/// catches rewriting a sparse update as a content clear.
#[test]
fn update_omission_and_explicit_empty_are_distinct_in_raw_and_serenity() {
    let absent = case("partial_content_absent");
    let empty = case("explicit_empty_content");
    assert!(absent.event.get("content").is_none());
    assert!(update(&absent).content.is_none());
    assert_eq!(empty.event.get("content"), Some(&json!("")));
    assert_eq!(update(&empty).content.as_deref(), Some(""));
    assert_eq!(absent.prior_cache[0].content, "SYNTHETIC prior text");
    assert!(update(&empty).embeds.is_none());
}

/// An embed-only fixture omits content and decodes one expected embed;
/// catches inventing a content update or dropping the embed payload.
#[test]
fn embed_only_preserves_content_omission_and_typed_embed() {
    let c = case("embed_only_update");
    let e = update(&c);
    assert!(c.event.get("content").is_none());
    assert!(e.content.is_none());
    let embeds = e.embeds.unwrap();
    assert_eq!(embeds.len(), 1);
    assert_eq!(embeds[0].description.as_deref(), Some("SYNTHETIC embed"));
    assert!(c.prior_cache[0].embeds.is_empty());
}

/// The cache-miss fixture has new text but no prior entry or typed author;
/// catches populating metadata the event does not supply.
#[test]
fn cache_miss_has_no_invented_prior_or_event_author() {
    let c = case("no_cached_message");
    assert!(c.prior_cache.is_empty());
    let e = update(&c);
    assert!(e.author.is_none());
    assert_eq!(e.content.as_deref(), Some("SYNTHETIC new text"));
}

/// Single and bulk delete fixtures decode through Serenity with the expected IDs and cache setup;
/// catches using incompatible wire keys or dropping a bulk ID.
#[test]
fn delete_models_use_actual_id_and_ids_wire_fields() {
    let single = case("single_delete");
    let e: MessageDeleteEvent = serde_json::from_value(single.event).unwrap();
    assert_eq!(e.message_id, single.prior_cache[0].id);
    assert_eq!(e.channel_id.get(), 201);
    assert_eq!(e.guild_id.unwrap().get(), 301);
    let bulk = case("bulk_delete");
    let e: MessageDeleteBulkEvent = serde_json::from_value(bulk.event).unwrap();
    assert_eq!(
        e.ids.iter().map(|id| id.get()).collect::<Vec<_>>(),
        [101, 102]
    );
    assert_eq!(bulk.prior_cache.len(), 1); // one known, one unknown
}

/// Typed updates collapse null and omitted content, but distinguish null webhook IDs and empty embeds;
/// catches a change in Serenity's optional-field decoding.
#[test]
fn raw_null_distinction_is_lost_for_content_but_preserved_for_webhook_id() {
    let base = json!({"id": "101", "channel_id": "201"});
    let absent: MessageUpdateEvent = serde_json::from_value(base.clone()).unwrap();
    let mut raw = base;
    raw["content"] = Value::Null;
    raw["webhook_id"] = Value::Null;
    let null: MessageUpdateEvent = serde_json::from_value(raw.clone()).unwrap();
    assert!(raw.get("content").unwrap().is_null());
    assert_eq!(absent.content, null.content); // both None; cannot recover raw presence
    assert_eq!(absent.webhook_id, None);
    assert_eq!(null.webhook_id, Some(None));
    raw["embeds"] = json!([]);
    let empty: MessageUpdateEvent = serde_json::from_value(raw).unwrap();
    assert_eq!(absent.embeds, None);
    assert_eq!(empty.embeds, Some(vec![]));
}

/// The test validator rejects malformed JSON and each sampled schema or shape contradiction;
/// catches accepting malformed event data or mislabeled fixtures.
#[test]
fn validator_rejects_malformed_and_contradictory_inputs() {
    assert!(validate_pack("{broken").is_err());
    for (label, mutate) in [
        ("missing required event ID", 0),
        ("absent label with explicit empty content", 1),
        ("wrong content type", 2),
        ("duplicate scenario", 3),
        ("bulk IDs not an array", 4),
        ("unknown envelope field", 5),
        ("non synthetic provenance", 6),
    ] {
        // Start from the same valid pack so rejection cannot come from a previous mutation.
        let mut raw: Value = serde_json::from_str(PACK).unwrap();
        match mutate {
            0 => {
                raw["cases"][0]["event"]
                    .as_object_mut()
                    .unwrap()
                    .remove("id");
            }
            1 => raw["cases"][0]["event"]["content"] = json!(""),
            2 => raw["cases"][1]["event"]["content"] = json!(42),
            3 => {
                let duplicate = raw["cases"][0].clone();
                raw["cases"].as_array_mut().unwrap().push(duplicate);
            }
            4 => raw["cases"][5]["event"]["ids"] = json!("101"),
            5 => raw["unexpected"] = json!(true),
            6 => raw["provenance"] = json!("unlabeled"),
            _ => unreachable!(),
        }
        assert!(validate_pack(&raw.to_string()).is_err(), "{label}");
    }
}
