//! Embed template loading, Discord limit validation and the content hash.
//!
//! A template is a JSON document describing one Discord embed. It is loaded from a
//! `&str` (the file reading belongs to the wiring card), validated against the Discord
//! embed limits and then reduced to a canonical form whose SHA-256 hash identifies the
//! CONTENT of the embed.
//!
//! The hash is what makes "edit in place, never duplicate" possible: the same template
//! loaded twice produces the same hash, whatever order the JSON keys were written in,
//! and any change to the rendered content produces a different hash.
//!
//! Privacy notes:
//! * Every error is a stable code. No error, no `Display` output and no `Debug` output
//!   ever contains template text, a field name or a value.
//! * The template text is only ever used as INPUT to the validator and the hash. It is
//!   not copied into any error, and the module holds no global state that could leak it.
//!
//! Limits (Discord documents them as inclusive; leading and trailing whitespace is
//! trimmed before the length is measured, exactly as Discord trims it):
//! title 256, description 4096, 25 fields, field name 256, field value 1024,
//! footer text 2048, author name 256, and a combined total of 6000 characters over
//! title + description + every field name + every field value + footer text +
//! author name.
//!
//! Wiring card W1 removes the module-level `allow(dead_code)` in `super::mod`.

use std::fmt;

use serde::Deserialize;

/// Maximum number of characters in the embed title.
pub const MAX_TITLE_CHARS: usize = 256;
/// Maximum number of characters in the embed description.
pub const MAX_DESCRIPTION_CHARS: usize = 4096;
/// Maximum number of fields in one embed.
pub const MAX_FIELDS: usize = 25;
/// Maximum number of characters in one field name.
pub const MAX_FIELD_NAME_CHARS: usize = 256;
/// Maximum number of characters in one field value.
pub const MAX_FIELD_VALUE_CHARS: usize = 1024;
/// Maximum number of characters in the footer text.
pub const MAX_FOOTER_CHARS: usize = 2048;
/// Maximum number of characters in the author name.
pub const MAX_AUTHOR_NAME_CHARS: usize = 256;
/// Maximum combined number of characters over title, description, field names, field
/// values, footer text and author name.
pub const MAX_TOTAL_CHARS: usize = 6000;

/// Stable reason codes for a template that was refused.
///
/// The codes never carry template text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateErrorKind {
    /// The document was not a well-formed template: invalid JSON, a wrong shape, an
    /// unknown key, a wrong type or a colour outside the representable range.
    InvalidJson,
    /// The template carried no renderable content at all.
    EmptyTemplate,
    /// The title was longer than [`MAX_TITLE_CHARS`].
    TitleTooLong,
    /// The description was longer than [`MAX_DESCRIPTION_CHARS`].
    DescriptionTooLong,
    /// The template carried more than [`MAX_FIELDS`] fields.
    TooManyFields,
    /// A field name was longer than [`MAX_FIELD_NAME_CHARS`].
    FieldNameTooLong,
    /// A field value was longer than [`MAX_FIELD_VALUE_CHARS`].
    FieldValueTooLong,
    /// The footer text was longer than [`MAX_FOOTER_CHARS`].
    FooterTooLong,
    /// The author name was longer than [`MAX_AUTHOR_NAME_CHARS`].
    AuthorNameTooLong,
    /// The combined length exceeded [`MAX_TOTAL_CHARS`].
    TotalTooLong,
}

impl TemplateErrorKind {
    /// Returns the stable code of this reason.
    pub fn code(self) -> &'static str {
        match self {
            TemplateErrorKind::InvalidJson => "invalid_json",
            TemplateErrorKind::EmptyTemplate => "empty_template",
            TemplateErrorKind::TitleTooLong => "title_too_long",
            TemplateErrorKind::DescriptionTooLong => "description_too_long",
            TemplateErrorKind::TooManyFields => "too_many_fields",
            TemplateErrorKind::FieldNameTooLong => "field_name_too_long",
            TemplateErrorKind::FieldValueTooLong => "field_value_too_long",
            TemplateErrorKind::FooterTooLong => "footer_too_long",
            TemplateErrorKind::AuthorNameTooLong => "author_name_too_long",
            TemplateErrorKind::TotalTooLong => "total_too_long",
        }
    }
}

/// A refused template.
///
/// The error names the reason only. It never carries template text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TemplateError {
    kind: TemplateErrorKind,
}

impl TemplateError {
    /// Builds an error for the given reason.
    pub fn new(kind: TemplateErrorKind) -> Self {
        Self { kind }
    }

    /// Returns the stable reason code of this refusal.
    pub fn kind(&self) -> TemplateErrorKind {
        self.kind
    }

    /// Returns the stable code of this refusal.
    pub fn code(&self) -> &'static str {
        self.kind.code()
    }
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.kind.code())
    }
}

impl std::error::Error for TemplateError {}

/// One field of an embed template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateField {
    /// The field name, already trimmed.
    pub name: String,
    /// The field value, already trimmed.
    pub value: String,
    /// Whether the field is rendered inline.
    pub inline: bool,
}

/// A validated embed template.
///
/// The text fields are already trimmed and normalised: a present-but-empty optional
/// text is normalised to "absent", so an empty title and a missing title render the
/// same embed and hash the same.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbedTemplate {
    /// The embed title, if any.
    pub title: Option<String>,
    /// The embed description, if any.
    pub description: Option<String>,
    /// The embed fields, in the order they will be rendered.
    pub fields: Vec<TemplateField>,
    /// The footer text, if any.
    pub footer: Option<String>,
    /// The author name, if any.
    pub author_name: Option<String>,
    /// The embed colour, if any.
    pub color: Option<u32>,
}

impl EmbedTemplate {
    /// Returns the combined number of characters that Discord counts towards the
    /// 6000 character total.
    pub fn total_chars(&self) -> usize {
        0
    }

    /// Returns the canonical form of this template.
    pub fn canonical(&self) -> String {
        String::new()
    }

    /// Returns the SHA-256 hex digest of the canonical form.
    pub fn content_hash(&self) -> String {
        String::new()
    }
}

/// The on-disk shape of a template file.
///
/// Every key is closed: an unknown key is a refusal, so a typo such as `"titel"`
/// cannot silently drop content. `Option` fields are optional; everything else is
/// required.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTemplate {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    fields: Option<Vec<RawField>>,
    #[serde(default)]
    footer: Option<RawFooter>,
    #[serde(default)]
    author: Option<RawAuthor>,
    #[serde(default)]
    color: Option<u32>,
}

/// The on-disk shape of one field.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawField {
    name: String,
    value: String,
    #[serde(default)]
    inline: bool,
}

/// The on-disk shape of the footer.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFooter {
    text: String,
}

/// The on-disk shape of the author.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAuthor {
    name: String,
}

/// Loads an embed template from a JSON document.
///
/// The template is validated against the Discord embed limits and returned trimmed and
/// normalised. Every refusal is a stable code that names the limit that was broken;
/// no refusal ever contains template text.
pub fn load(source: &str) -> Result<EmbedTemplate, TemplateError> {
    let _ = source;
    Err(TemplateError::new(TemplateErrorKind::InvalidJson))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The General Rules embed of the old YAGPDB bot, rebuilt from `YAG:199-248` of
    /// `YAGPDB Custom Commands.md`. The role and channel references are replaced by
    /// clearly synthetic ids: the real ones are server specific and this repo is public.
    const GENERAL_RULES: &str = r#"{
        "title": "Server Rules (General)",
        "description": "Be kind and respectful. Follow Discord's Terms of Service. Admins and moderators have final say; listen to them. No spam: no emotes, repeated characters, stickers, GIFs, etc. This is an 18+ server with NSFW content. Age verification may be required; refusal = kick, knowingly underage = permanent ban. Check pins in new channels for channel-specific rules. Respect pronouns, gender, orientation, race, citizenship, etc. No discrimination. Use a custom profile: at minimum a non-default PFP. Staff may ask you to change it; refusal = kick. Rules are guided by common sense: just behave.",
        "color": 5793266,
        "fields": [
            {
                "name": "📌 Key Points",
                "value": "• Follow Discord TOS\n• Mods/admins have final say\n• No spam of any kind\n• 18+ only; age checks enforced\n• Check channel pins for extra rules\n• Respect identities; no discrimination\n• Non-default PFP required\n• Common sense applies",
                "inline": false
            }
        ],
        "footer": { "text": "SKYNET // NETWORK CORE" }
    }"#;

    /// The NSFW & Kink Rules embed of the old YAGPDB bot, rebuilt from `YAG:199-248`.
    /// The safeword emoji ids and the staff role id are synthetic.
    const NSFW_RULES: &str = r#"{
        "title": "NSFW & Kink Rules",
        "color": 15525061,
        "fields": [
            {
                "name": "⚠️ General NSFW Rules",
                "value": "All general server rules also apply in NSFW channels.\n\nRead people's roles before interacting: everyone has different kinks, limits, likes, and dislikes. Be respectful.\n\nServer safewords (emotes):\n<:Safeword_Green:1000000000000000001> = \"I like this; consent given.\"\n<:Safeword_Yellow:1000000000000000002> = \"I'm uncomfortable. Stop & ask to continue.\"\n<:Safeword_Red:1000000000000000003> = \"VERY uncomfortable. Stop immediately and go to aftercare.\"\n\nIf someone fails to respond appropriately to safewords, mention <@&1000000000000000004> immediately.",
                "inline": false
            },
            {
                "name": "🎭 Role Selection (Pick ONE)",
                "value": "Choose exactly one: Dom, Switch, Sub, Bottom Barrel Sub.\n\nDom: Dominant in BDSM activities. Can change nicknames for roleplay (don't abuse this). Expected to behave dominantly when in-theme.\n\nSwitch: Can be dominant or submissive. Has same perms as Doms. Clearly communicate whether you're in a dominant or submissive mood with each partner.\n\nSub: Submissive in BDSM activities. Some perms removed (e.g., free nickname changes). Expected to act submissively toward Doms and dominant Switches when in-theme.\n\nBottom Barrel Sub / Slave: Like Sub, but with even fewer perms and no expectation of respect from anyone. Expect near-constant teasing, kink-pushing, and rough treatment from Doms, Switches, and even other Subs. Only take this if you want that. Switch back to normal Sub if you need a break.",
                "inline": false
            },
            {
                "name": "❤️ Consent & Safety",
                "value": "Respect people's sexuality and stated limits. If someone says \"no\" or \"stop,\" you stop immediately. Violations = mod action.\n\nIf a trans person is okay with misgendering as a kink, or someone is okay with race play, keep it semi-reasonable. For extreme play, move it to DMs-especially if using hard slurs that will offend most readers.\n\nNo images of minors in any channel. This includes \"aged up\" characters that still look like minors. Violation = immediate ban, no negotiation.",
                "inline": false
            },
            {
                "name": "🩹 Aftercare & Unlisted Kinks",
                "value": "ALWAYS offer aftercare (Doms and Subs). This is critical for mental/physical safety. If mods are notified of a failure to provide aftercare: exactly ONE warning, then aggressive escalation (including possible immediate ban depending on severity/history).\n\nAsk before engaging in any kink not listed on someone's bio, roles, or intro-even in on-theme channels. Failure to request consent = moderation, one warning then aggressive escalation.\n\nIf someone asks not to be called titles off-theme, respect that. Off-theme channels are meant to be off-theme; only use titles if everyone consents.\n\nIf you see someone with a <:Safeword_Red:1000000000000000003> next to their name, be kind-they're on safeword and likely not in their best state. If needed, mention <@&1000000000000000004>.",
                "inline": false
            },
            {
                "name": "🍋🍵 Protocol & Bratting",
                "value": "Bratting is generally not allowed. Subs may only brat toward Doms and Switches with the Brat Tamer role. If a sub breaks this, contact <@&1000000000000000004>.\n\nChannel prefixes:\n🍋 = High protocol / on-theme: dynamics enforced (subs submissive to doms; bratting only allowed to Doms/Brat Tamer Switches).\n🍵 = Low protocol / off-theme: mods do NOT enforce dynamics. If you have an ownership dynamic and want to punish a sub for off-theme behavior, do it in DMs or an on-theme channel, not in the 🍵 channel.\n🍋 + 🍵 = Half-theme: mix of high/low. Mods enforce less aggressively, but subs should still be respectful to doms. If a scene starts in a half-theme channel, subs MUST follow dynamics rules during that scene.",
                "inline": false
            }
        ]
    }"#;

    /// Builds a JSON document with one title of `len` characters.
    fn title_json(len: usize) -> String {
        let text = "t".repeat(len);
        format!(r#"{{"title":"{text}"}}"#)
    }

    /// Builds a JSON document with one description of `len` characters.
    fn description_json(len: usize) -> String {
        let text = "d".repeat(len);
        format!(r#"{{"description":"{text}"}}"#)
    }

    /// Builds a JSON document with `count` fields, each with a name and a value of
    /// `len` characters.
    fn fields_json(count: usize, name_len: usize, value_len: usize) -> String {
        let name = "n".repeat(name_len);
        let value = "v".repeat(value_len);
        let mut fields = Vec::with_capacity(count);
        for index in 0..count {
            fields.push(format!(r#"{{"name":"{name}{index}","value":"{value}"}}"#));
        }
        format!(r#"{{"fields":[{}]}}"#, fields.join(","))
    }

    /// Expects the template to load and returns it.
    fn expect_ok(source: &str) -> EmbedTemplate {
        load(source).expect("template must load")
    }

    /// Expects the template to be refused for the given reason.
    fn expect_err(source: &str, kind: TemplateErrorKind) {
        let err = load(source).expect_err("template must be refused");
        assert_eq!(err.kind(), kind, "wrong reason for {source}");
    }

    /// The General Rules embed of the old bot loads within the Discord limits.
    #[test]
    fn general_rules_embed_loads_within_limits() {
        let template = expect_ok(GENERAL_RULES);
        assert_eq!(template.title.as_deref(), Some("Server Rules (General)"));
        assert_eq!(template.fields.len(), 1);
        assert_eq!(template.fields[0].name, "📌 Key Points");
        assert!(!template.fields[0].inline);
        assert_eq!(template.footer.as_deref(), Some("SKYNET // NETWORK CORE"));
        assert_eq!(template.color, Some(5793266));
        assert!(template.total_chars() < MAX_TOTAL_CHARS);
    }

    /// The NSFW & Kink Rules embed of the old bot loads within the Discord limits:
    /// five fields, each value under 1024 characters, and the whole embed under 6000.
    #[test]
    fn nsfw_rules_embed_loads_within_limits() {
        let template = expect_ok(NSFW_RULES);
        assert_eq!(template.title.as_deref(), Some("NSFW & Kink Rules"));
        assert_eq!(template.fields.len(), 5);
        for field in &template.fields {
            assert!(
                field.value.chars().count() <= MAX_FIELD_VALUE_CHARS,
                "field {:?} is too long",
                field.name
            );
            assert!(field.name.chars().count() <= MAX_FIELD_NAME_CHARS);
            assert!(!field.inline);
        }
        assert!(template.total_chars() <= MAX_TOTAL_CHARS);
        // The real embed is about 3.5k characters; the module must not silently
        // truncate or drop a field to make it fit.
        assert!(template.total_chars() > 3000, "content was lost");
    }

    /// The title limit is inclusive: 256 characters load, 257 do not.
    #[test]
    fn title_limit_boundary() {
        expect_ok(&title_json(MAX_TITLE_CHARS));
        expect_err(
            &title_json(MAX_TITLE_CHARS + 1),
            TemplateErrorKind::TitleTooLong,
        );
    }

    /// The description limit is inclusive: 4096 characters load, 4097 do not.
    #[test]
    fn description_limit_boundary() {
        expect_ok(&description_json(MAX_DESCRIPTION_CHARS));
        expect_err(
            &description_json(MAX_DESCRIPTION_CHARS + 1),
            TemplateErrorKind::DescriptionTooLong,
        );
    }

    /// The field count limit is inclusive: 25 fields load, 26 do not.
    #[test]
    fn field_count_limit_boundary() {
        expect_ok(&fields_json(MAX_FIELDS, 1, 1));
        expect_err(
            &fields_json(MAX_FIELDS + 1, 1, 1),
            TemplateErrorKind::TooManyFields,
        );
    }

    /// The field name limit is inclusive: 256 characters load, 257 do not.
    #[test]
    fn field_name_limit_boundary() {
        expect_ok(&fields_json(1, MAX_FIELD_NAME_CHARS, 1));
        expect_err(
            &fields_json(1, MAX_FIELD_NAME_CHARS + 1, 1),
            TemplateErrorKind::FieldNameTooLong,
        );
    }

    /// The field value limit is inclusive: 1024 characters load, 1025 do not.
    #[test]
    fn field_value_limit_boundary() {
        expect_ok(&fields_json(1, 1, MAX_FIELD_VALUE_CHARS));
        expect_err(
            &fields_json(1, 1, MAX_FIELD_VALUE_CHARS + 1),
            TemplateErrorKind::FieldValueTooLong,
        );
    }

    /// The footer limit is inclusive: 2048 characters load, 2049 do not.
    #[test]
    fn footer_limit_boundary() {
        let ok = "f".repeat(MAX_FOOTER_CHARS);
        let bad = "f".repeat(MAX_FOOTER_CHARS + 1);
        expect_ok(&format!(r#"{{"footer":{{"text":"{ok}"}}}}"#));
        expect_err(
            &format!(r#"{{"footer":{{"text":"{bad}"}}}}"#),
            TemplateErrorKind::FooterTooLong,
        );
    }

    /// The author name limit is inclusive: 256 characters load, 257 do not.
    #[test]
    fn author_name_limit_boundary() {
        let ok = "a".repeat(MAX_AUTHOR_NAME_CHARS);
        let bad = "a".repeat(MAX_AUTHOR_NAME_CHARS + 1);
        expect_ok(&format!(r#"{{"author":{{"name":"{ok}"}}}}"#));
        expect_err(
            &format!(r#"{{"author":{{"name":"{bad}"}}}}"#),
            TemplateErrorKind::AuthorNameTooLong,
        );
    }

    /// The combined total is inclusive: 6000 characters load, 6001 do not.
    #[test]
    fn total_limit_boundary() {
        let title = "t".repeat(MAX_TITLE_CHARS);
        let description = "d".repeat(MAX_DESCRIPTION_CHARS);
        let footer = "f".repeat(MAX_TOTAL_CHARS - MAX_TITLE_CHARS - MAX_DESCRIPTION_CHARS);
        let source = format!(
            r#"{{"title":"{title}","description":"{description}","footer":{{"text":"{footer}"}}}}"#
        );
        assert_eq!(expect_ok(&source).total_chars(), MAX_TOTAL_CHARS);

        let over = format!(
            r#"{{"title":"{title}","description":"{description}","footer":{{"text":"{footer}x"}}}}"#
        );
        expect_err(&over, TemplateErrorKind::TotalTooLong);
    }

    /// The total also counts the author name and the field names and values.
    #[test]
    fn total_counts_every_part() {
        let source = r#"{
            "title": "aaa",
            "description": "bb",
            "fields": [{"name":"ccc","value":"dddd"}],
            "footer": {"text":"ee"},
            "author": {"name":"ff"}
        }"#;
        assert_eq!(expect_ok(source).total_chars(), 3 + 2 + 3 + 4 + 2 + 2);
    }

    /// Leading and trailing whitespace is trimmed before the length is measured, so a
    /// title of the limit length plus padding still loads.
    #[test]
    fn surrounding_whitespace_is_trimmed_before_measuring() {
        let title = "t".repeat(MAX_TITLE_CHARS);
        let source = format!(r#"{{"title":"   {title}   "}}"#);
        let template = expect_ok(&source);
        assert_eq!(template.title.as_deref(), Some(title.as_str()));
    }

    /// A present but empty text is normalised to absent, so it renders and hashes like
    /// a missing text.
    #[test]
    fn empty_text_is_normalised_to_absent() {
        let template = expect_ok(r#"{"title":"   ","description":""}"#);
        assert_eq!(template.title, None);
        assert_eq!(template.description, None);
    }

    /// A template with no content at all is refused.
    #[test]
    fn empty_template_is_refused() {
        expect_err("{}", TemplateErrorKind::EmptyTemplate);
        expect_err(
            r#"{"title":"","fields":[]}"#,
            TemplateErrorKind::EmptyTemplate,
        );
    }

    /// Invalid JSON, a wrong shape and an unknown key are all refused as
    /// `invalid_json`.
    #[test]
    fn malformed_documents_are_refused() {
        for source in [
            "",
            "not json",
            "{",
            "[]",
            r#"{"title": 5}"#,
            r#"{"titel":"typo"}"#,
            r#"{"fields":[{"name":"a"}]}"#,
            r#"{"footer":{"text":"a","icon_url":"x"}}"#,
            r##"{"color":"#ffffff"}"##,
            r#"{"color":16777216}"#,
        ] {
            expect_err(source, TemplateErrorKind::InvalidJson);
        }
    }

    /// The refusal names the limit that was broken and never carries template text.
    #[test]
    fn errors_carry_no_template_text() {
        let secret = "SUPERSECRETTEMPLATEMARKERZZ";
        let source = format!(r#"{{"title":"{secret}"}}"#);
        let err = load(&source).expect("template loads");
        assert!(!err.canonical().contains(secret));

        let long = "x".repeat(MAX_TITLE_CHARS + 1);
        let source = format!(r#"{{"title":"{long}{secret}"}}"#);
        let err = load(&source).expect_err("title is too long");
        let text = format!("{err} | {err:?}");
        assert!(!text.contains(secret), "error leaked the template text: {text}");
        assert!(!text.contains(&long), "error leaked the template text");
        assert!(text.len() < 80, "{text}");
    }

    /// Every reason code is distinct and stable.
    #[test]
    fn error_codes_are_distinct_and_stable() {
        let all = [
            TemplateErrorKind::InvalidJson,
            TemplateErrorKind::EmptyTemplate,
            TemplateErrorKind::TitleTooLong,
            TemplateErrorKind::DescriptionTooLong,
            TemplateErrorKind::TooManyFields,
            TemplateErrorKind::FieldNameTooLong,
            TemplateErrorKind::FieldValueTooLong,
            TemplateErrorKind::FooterTooLong,
            TemplateErrorKind::AuthorNameTooLong,
            TemplateErrorKind::TotalTooLong,
        ];
        let mut codes: Vec<_> = all.iter().map(|kind| kind.code()).collect();
        codes.sort();
        codes.dedup();
        assert_eq!(codes.len(), all.len());
        assert_eq!(TemplateErrorKind::TitleTooLong.code(), "title_too_long");
    }

    /// The content hash is 64 lowercase hex characters and is stable across calls.
    #[test]
    fn content_hash_is_64_hex_characters() {
        let hash = expect_ok(GENERAL_RULES).content_hash();
        assert_eq!(hash.len(), 64);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!hash.chars().any(|c| c.is_ascii_uppercase()));
        assert_eq!(hash, expect_ok(GENERAL_RULES).content_hash());
    }

    /// The hash does not depend on the order the JSON keys were written in.
    #[test]
    fn hash_is_stable_across_json_key_order() {
        let a = r#"{"title":"Rules","description":"Be kind","fields":[{"name":"A","value":"1","inline":true}]}"#;
        let b = r#"{"fields":[{"inline":true,"value":"1","name":"A"}],"description":"Be kind","title":"Rules"}"#;
        assert_eq!(expect_ok(a).content_hash(), expect_ok(b).content_hash());
        assert_eq!(expect_ok(a).canonical(), expect_ok(b).canonical());
    }

    /// The hash changes when the rendered content changes.
    #[test]
    fn hash_changes_with_content() {
        let base = expect_ok(GENERAL_RULES).content_hash();
        let changed_title =
            GENERAL_RULES.replace("Server Rules (General)", "Server Rules (General v2)");
        assert_ne!(base, expect_ok(&changed_title).content_hash());

        let changed_field =
            GENERAL_RULES.replace("Common sense applies", "Common sense applies (mostly)");
        assert_ne!(base, expect_ok(&changed_field).content_hash());

        let changed_color = GENERAL_RULES.replace("5793266", "5793267");
        assert_ne!(base, expect_ok(&changed_color).content_hash());
    }

    /// The hash changes when the ORDER of the fields changes, because the order is part
    /// of the rendered embed.
    #[test]
    fn hash_changes_with_field_order() {
        let a = r#"{"fields":[{"name":"A","value":"1"},{"name":"B","value":"2"}]}"#;
        let b = r#"{"fields":[{"name":"B","value":"2"},{"name":"A","value":"1"}]}"#;
        assert_ne!(expect_ok(a).content_hash(), expect_ok(b).content_hash());
    }

    /// A value that looks like a canonical-form tag cannot be confused with one.
    #[test]
    fn canonical_form_is_unambiguous() {
        let a = r#"{"title":"x","description":"y"}"#;
        let b = r#"{"title":"x\ndescription=y"}"#;
        assert_ne!(expect_ok(a).canonical(), expect_ok(b).canonical());
        assert_ne!(expect_ok(a).content_hash(), expect_ok(b).content_hash());
    }
}
