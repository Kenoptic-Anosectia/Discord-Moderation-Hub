//! Pure welcome-template renderer: loading, placeholder rules, escaping and limits.
//!
//! Everything here is Discord-free: a template is loaded from a JSON `&str`, and the
//! renderer fills placeholders for one joining member into plain data.

use std::collections::BTreeMap;
use std::fmt;

/// Discord embed limit: the total number of characters across the whole embed.
pub const MAX_EMBED_TOTAL_CHARS: usize = 6000;
/// Discord embed limit: the maximum number of fields.
pub const MAX_EMBED_FIELDS: usize = 25;
/// Discord embed limit: the title length.
pub const MAX_TITLE_CHARS: usize = 256;
/// Discord embed limit: the description length.
pub const MAX_DESCRIPTION_CHARS: usize = 4096;
/// Discord embed limit: the field name length.
pub const MAX_FIELD_NAME_CHARS: usize = 256;
/// Discord embed limit: the field value length.
pub const MAX_FIELD_VALUE_CHARS: usize = 1024;
/// Discord embed limit: the footer text length.
pub const MAX_FOOTER_CHARS: usize = 2048;
/// Discord message limit: the content length.
pub const MAX_CONTENT_CHARS: usize = 2000;
/// The maximum length of the sanitised username, in characters.
pub const MAX_USERNAME_CHARS: usize = 64;

/// Stable reason codes for a template that was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemplateErrorKind {
    /// The source was not a JSON object.
    MalformedJson,
    /// The template has no `embed` object.
    MissingEmbed,
    /// The template has a field the loader does not know.
    UnknownField,
    /// The template uses a placeholder the renderer does not know.
    UnknownPlaceholder,
    /// A `{channel:KEY}` or `{role:KEY}` placeholder has a malformed key.
    MalformedPlaceholder,
    /// A `{` has no matching `}`.
    UnclosedPlaceholder,
    /// A `{channel:KEY}` placeholder names a key the caller's map does not have.
    UnknownChannelKey,
    /// A `{role:KEY}` placeholder names a key the caller's map does not have.
    UnknownRoleKey,
    /// The embed title is longer than [`MAX_TITLE_CHARS`].
    TitleTooLong,
    /// The embed description is longer than [`MAX_DESCRIPTION_CHARS`].
    DescriptionTooLong,
    /// The embed has more than [`MAX_EMBED_FIELDS`] fields.
    TooManyFields,
    /// A field name is longer than [`MAX_FIELD_NAME_CHARS`].
    FieldNameTooLong,
    /// A field value is longer than [`MAX_FIELD_VALUE_CHARS`].
    FieldValueTooLong,
    /// The footer text is longer than [`MAX_FOOTER_CHARS`].
    FooterTooLong,
    /// The author name is longer than [`MAX_TITLE_CHARS`].
    AuthorNameTooLong,
    /// The embed exceeds [`MAX_EMBED_TOTAL_CHARS`] characters in total.
    EmbedTotalTooLong,
    /// The message content is longer than [`MAX_CONTENT_CHARS`].
    ContentTooLong,
    /// A field has an empty name.
    EmptyFieldName,
    /// A field has an empty value.
    EmptyFieldValue,
}

impl TemplateErrorKind {
    /// The stable machine-readable code for this refusal.
    pub fn code(&self) -> &'static str {
        match self {
            TemplateErrorKind::MalformedJson => "malformed_json",
            TemplateErrorKind::MissingEmbed => "missing_embed",
            TemplateErrorKind::UnknownField => "unknown_field",
            TemplateErrorKind::UnknownPlaceholder => "unknown_placeholder",
            TemplateErrorKind::MalformedPlaceholder => "malformed_placeholder",
            TemplateErrorKind::UnclosedPlaceholder => "unclosed_placeholder",
            TemplateErrorKind::UnknownChannelKey => "unknown_channel_key",
            TemplateErrorKind::UnknownRoleKey => "unknown_role_key",
            TemplateErrorKind::TitleTooLong => "title_too_long",
            TemplateErrorKind::DescriptionTooLong => "description_too_long",
            TemplateErrorKind::TooManyFields => "too_many_fields",
            TemplateErrorKind::FieldNameTooLong => "field_name_too_long",
            TemplateErrorKind::FieldValueTooLong => "field_value_too_long",
            TemplateErrorKind::FooterTooLong => "footer_too_long",
            TemplateErrorKind::AuthorNameTooLong => "author_name_too_long",
            TemplateErrorKind::EmbedTotalTooLong => "embed_total_too_long",
            TemplateErrorKind::ContentTooLong => "content_too_long",
            TemplateErrorKind::EmptyFieldName => "empty_field_name",
            TemplateErrorKind::EmptyFieldValue => "empty_field_value",
        }
    }
}

/// A refused template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateError {
    kind: TemplateErrorKind,
}

impl TemplateError {
    fn new(kind: TemplateErrorKind) -> Self {
        Self { kind }
    }

    /// Returns the stable reason code for this refusal.
    pub fn kind(&self) -> TemplateErrorKind {
        self.kind
    }
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "welcome template refused: {}", self.kind.code())
    }
}

impl std::error::Error for TemplateError {}

/// The named ids a template may refer to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdMap {
    channels: BTreeMap<String, u64>,
    roles: BTreeMap<String, u64>,
}

impl IdMap {
    /// Adds a channel id under `key`.
    pub fn insert_channel(&mut self, key: &str, id: u64) -> bool {
        insert_id(&mut self.channels, key, id)
    }

    /// Adds a role id under `key`.
    pub fn insert_role(&mut self, key: &str, id: u64) -> bool {
        insert_id(&mut self.roles, key, id)
    }

    /// Adds several channel ids at once.
    pub fn extend_channels<I, K>(&mut self, pairs: I)
    where
        I: IntoIterator<Item = (K, u64)>,
        K: AsRef<str>,
    {
        for (key, id) in pairs {
            self.insert_channel(key.as_ref(), id);
        }
    }

    /// Adds several role ids at once.
    pub fn extend_roles<I, K>(&mut self, pairs: I)
    where
        I: IntoIterator<Item = (K, u64)>,
        K: AsRef<str>,
    {
        for (key, id) in pairs {
            self.insert_role(key.as_ref(), id);
        }
    }

    /// The channel id stored under `key`, if any.
    pub fn channel(&self, key: &str) -> Option<u64> {
        self.channels.get(key).copied()
    }

    /// The role id stored under `key`, if any.
    pub fn role(&self, key: &str) -> Option<u64> {
        self.roles.get(key).copied()
    }

    /// Every channel id in the map, by key.
    pub fn channels(&self) -> &BTreeMap<String, u64> {
        &self.channels
    }

    /// Every role id in the map, by key.
    pub fn roles(&self) -> &BTreeMap<String, u64> {
        &self.roles
    }
}

/// Inserts an id under a key, keeping the first id for that key.
fn insert_id(map: &mut BTreeMap<String, u64>, key: &str, id: u64) -> bool {
    if id == 0 || !is_plain_key(key) {
        return false;
    }
    if map.contains_key(key) {
        return false;
    }
    map.insert(key.to_string(), id);
    true
}

/// True when the key is a plain word: letters, digits and the underscore.
fn is_plain_key(key: &str) -> bool {
    !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The plain facts about the member that joined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberContext {
    /// Id of the joining user.
    pub user_id: u64,
    /// The joining user's display name. Untrusted.
    pub username: String,
    /// Name of the server the member joined.
    pub server_name: String,
    /// Number of members in the server after the join.
    pub member_count: u64,
    /// True when the joining account is a bot.
    pub is_bot: bool,
}

/// One rendered embed field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedField {
    /// The field name.
    pub name: String,
    /// The field value.
    pub value: String,
    /// Whether Discord shows the field inline.
    pub inline: bool,
}

/// The rendered embed, as plain data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedEmbed {
    /// The embed title.
    pub title: Option<String>,
    /// The embed description.
    pub description: Option<String>,
    /// The embed fields, in template order.
    pub fields: Vec<RenderedField>,
    /// The footer text.
    pub footer_text: Option<String>,
    /// The author name.
    pub author_name: Option<String>,
}

/// The rendered welcome message, as plain data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedWelcome {
    /// The message content, if the template had any.
    pub content: Option<String>,
    /// The single embed the template describes.
    pub embed: RenderedEmbed,
}

/// Why a member is skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// The member is not skipped: the welcome is rendered.
    None,
    /// The joining account is a bot, so no welcome is posted.
    Bot,
}

/// A validated welcome template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WelcomeTemplate {
    content: Option<String>,
    embed: RenderedEmbed,
}

impl WelcomeTemplate {
    /// Loads and validates a template from a JSON source.
    pub fn load(src: &str, ids: &IdMap) -> Result<Self, TemplateError> {
        let _ = (src, ids);
        Err(TemplateError::new(TemplateErrorKind::MalformedJson))
    }

    /// The message content of the template, before rendering.
    pub fn content(&self) -> Option<&str> {
        self.content.as_deref()
    }

    /// The embed of the template, before rendering.
    pub fn embed(&self) -> &RenderedEmbed {
        &self.embed
    }
}

/// The decision for one joining member.
pub fn welcome_policy(
    template: &WelcomeTemplate,
    member: &MemberContext,
    ids: &IdMap,
) -> SkipReason {
    let _ = (template, ids);
    if member.is_bot {
        SkipReason::Bot
    } else {
        SkipReason::None
    }
}

/// Renders the template for one joining member.
pub fn render_welcome(
    template: &WelcomeTemplate,
    member: &MemberContext,
    ids: &IdMap,
) -> RenderedWelcome {
    let _ = (template, member, ids);
    RenderedWelcome {
        content: None,
        embed: RenderedEmbed {
            title: None,
            description: None,
            fields: Vec::new(),
            footer_text: None,
            author_name: None,
        },
    }
}

/// Sanitises the joining user's name.
pub fn sanitize_username(username: &str) -> String {
    username.to_string()
}

/// The English ordinal of `number`.
pub fn english_ordinal(number: u64) -> String {
    number.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    // Synthetic ids. They are deliberately far from any real Discord snowflake and carry
    // no meaning: 18 digits that were never issued by Discord.
    const GUILD: u64 = 1_000_000_000_000_000_001;
    const WELCOME_CHANNEL: u64 = 1_000_000_000_000_000_002;
    const RULES_CHANNEL: u64 = 1_000_000_000_000_000_003;
    const MEMBER_ROLE: u64 = 1_000_000_000_000_000_004;
    const JOINING_USER: u64 = 1_000_000_000_000_000_005;

    fn id_map() -> IdMap {
        let mut map = IdMap::default();
        map.insert_channel("welcome", WELCOME_CHANNEL);
        map.insert_channel("rules", RULES_CHANNEL);
        map.insert_channel("rules_main", RULES_CHANNEL);
        map.insert_channel("rules_nsfw", RULES_CHANNEL);
        map.insert_channel("roles_general", RULES_CHANNEL);
        map.insert_channel("roles_nsfw", RULES_CHANNEL);
        map.insert_channel("roles_kinks", RULES_CHANNEL);
        map.insert_channel("tickets", RULES_CHANNEL);
        map.insert_role("member", MEMBER_ROLE);
        map
    }

    fn member() -> MemberContext {
        MemberContext {
            user_id: JOINING_USER,
            username: "newcomer".to_string(),
            server_name: "Test Server".to_string(),
            member_count: 42,
            is_bot: false,
        }
    }

    fn template_with(body: &str) -> String {
        format!("{{\"content\": null, \"embed\": {body}}}")
    }

    fn embed_json(fields: &str) -> String {
        format!("{{\"title\": \"t\", \"description\": \"d\", \"fields\": [{fields}]}}")
    }

    // ---------------------------------------------------------------------------
    // Loading
    // ---------------------------------------------------------------------------

    /// A minimal valid template loads.
    #[test]
    fn minimal_template_loads() {
        let src = template_with(&embed_json("{\"name\": \"n\", \"value\": \"v\"}"));
        let t = WelcomeTemplate::load(&src, &id_map()).expect("valid template loads");
        assert_eq!(t.embed().title.as_deref(), Some("t"));
        assert_eq!(t.embed().description.as_deref(), Some("d"));
        assert_eq!(t.embed().fields.len(), 1);
        assert_eq!(t.embed().fields[0].name, "n");
        assert_eq!(t.embed().fields[0].value, "v");
    }

    /// Malformed JSON is refused with a stable error code.
    #[test]
    fn malformed_json_is_refused() {
        let err = WelcomeTemplate::load("{not json", &id_map()).expect_err("must be refused");
        assert_eq!(err.kind(), TemplateErrorKind::MalformedJson);
        // The error must not echo the offending text.
        assert!(!err.to_string().contains("not json"));
    }

    /// A JSON document that is not an object is refused.
    #[test]
    fn non_object_json_is_refused() {
        let err = WelcomeTemplate::load("[]", &id_map()).expect_err("must be refused");
        assert_eq!(err.kind(), TemplateErrorKind::MalformedJson);
    }

    /// A template with no embed at all is refused: the feature renders one embed.
    #[test]
    fn template_without_embed_is_refused() {
        let err =
            WelcomeTemplate::load("{\"content\": \"hi\"}", &id_map()).expect_err("must be refused");
        assert_eq!(err.kind(), TemplateErrorKind::MissingEmbed);
    }

    /// An unknown field in the JSON is refused, so a typo in the template file is caught at
    /// startup instead of silently dropping content.
    #[test]
    fn unknown_template_field_is_refused() {
        let src = "{\"embed\": {\"title\": \"t\"}, \"titel\": \"typo\"}";
        let err = WelcomeTemplate::load(src, &id_map()).expect_err("must be refused");
        assert_eq!(err.kind(), TemplateErrorKind::UnknownField);
    }

    /// An unknown field inside the embed is refused too.
    #[test]
    fn unknown_embed_field_is_refused() {
        let src = "{\"embed\": {\"title\": \"t\", \"descrption\": \"typo\"}}";
        let err = WelcomeTemplate::load(src, &id_map()).expect_err("must be refused");
        assert_eq!(err.kind(), TemplateErrorKind::UnknownField);
    }

    // ---------------------------------------------------------------------------
    // Rule 1: unknown placeholders are refused at LOAD time
    // ---------------------------------------------------------------------------

    /// An unknown placeholder name is refused when the template is loaded, not when a member
    /// joins.
    #[test]
    fn unknown_placeholder_is_refused_at_load() {
        let src = template_with(&embed_json("{\"name\": \"n\", \"value\": \"{usrename}\"}"));
        let err = WelcomeTemplate::load(&src, &id_map()).expect_err("must be refused at load");
        assert_eq!(err.kind(), TemplateErrorKind::UnknownPlaceholder);
    }

    /// An unknown placeholder in the embed title is refused at load.
    #[test]
    fn unknown_placeholder_in_title_is_refused_at_load() {
        let src = "{\"embed\": {\"title\": \"{member_countt}\"}}";
        let err = WelcomeTemplate::load(src, &id_map()).expect_err("must be refused at load");
        assert_eq!(err.kind(), TemplateErrorKind::UnknownPlaceholder);
    }

    /// An unknown placeholder in the content is refused at load.
    #[test]
    fn unknown_placeholder_in_content_is_refused_at_load() {
        let src = "{\"content\": \"{nope}\", \"embed\": {\"title\": \"t\"}}";
        let err = WelcomeTemplate::load(src, &id_map()).expect_err("must be refused at load");
        assert_eq!(err.kind(), TemplateErrorKind::UnknownPlaceholder);
    }

    /// An unknown channel KEY is refused at load.
    #[test]
    fn unknown_channel_key_is_refused_at_load() {
        let src = template_with(&embed_json(
            "{\"name\": \"n\", \"value\": \"{channel:rules}\"}",
        ));
        let err = WelcomeTemplate::load(&src, &id_map()).expect_err("must be refused at load");
        assert_eq!(err.kind(), TemplateErrorKind::UnknownChannelKey);
    }

    /// An unknown role KEY is refused at load.
    #[test]
    fn unknown_role_key_is_refused_at_load() {
        let src = template_with(&embed_json(
            "{\"name\": \"n\", \"value\": \"{role:member}\"}",
        ));
        let err = WelcomeTemplate::load(&src, &id_map()).expect_err("must be refused at load");
        assert_eq!(err.kind(), TemplateErrorKind::UnknownRoleKey);
    }

    /// A malformed KEY (empty or non-word characters) is refused at load.
    #[test]
    fn malformed_placeholder_key_is_refused_at_load() {
        for bad in [
            "{channel:}",
            "{channel:with space}",
            "{role:}",
            "{channel:bad-key}",
        ] {
            let src = template_with(&embed_json(&format!(
                "{{\"name\": \"n\", \"value\": \"{bad}\"}}"
            )));
            let err = WelcomeTemplate::load(&src, &id_map())
                .err()
                .unwrap_or_else(|| panic!("{bad} must be refused"));
            // A malformed key is refused; the code says which class it was.
            assert!(
                matches!(
                    err.kind(),
                    TemplateErrorKind::UnknownChannelKey
                        | TemplateErrorKind::UnknownRoleKey
                        | TemplateErrorKind::MalformedPlaceholder
                ),
                "{bad} produced {:?}",
                err.kind()
            );
        }
    }

    /// An unbalanced brace is refused at load.
    #[test]
    fn unbalanced_brace_is_refused_at_load() {
        let src = template_with(&embed_json(
            "{\"name\": \"n\", \"value\": \"{user_mention\"}",
        ));
        let err = WelcomeTemplate::load(&src, &id_map()).expect_err("must be refused");
        assert_eq!(err.kind(), TemplateErrorKind::UnclosedPlaceholder);
    }

    /// A stray closing brace is refused at load.
    #[test]
    fn stray_closing_brace_is_refused_at_load() {
        let src = template_with(&embed_json(
            "{\"name\": \"n\", \"value\": \"user_mention}\"}",
        ));
        let err = WelcomeTemplate::load(&src, &id_map()).expect_err("must be refused");
        assert_eq!(err.kind(), TemplateErrorKind::UnclosedPlaceholder);
    }

    // ---------------------------------------------------------------------------
    // Rendering every placeholder
    // ---------------------------------------------------------------------------

    /// Every known placeholder is substituted in the place it appears.
    #[test]
    fn every_placeholder_is_substituted() {
        let src = "{\"content\": \"{user_mention}\", \"embed\": {\"title\": \"{server}\", \
                   \"description\": \"{username} {member_count} {member_ordinal}\", \
                   \"fields\": [{\"name\": \"c\", \"value\": \"{channel:rules}\"}, \
                              {\"name\": \"r\", \"value\": \"{role:member}\"}]}}";
        let t = WelcomeTemplate::load(src, &id_map()).expect("valid template loads");
        let out = render_welcome(&t, &member(), &id_map());
        assert_eq!(out.content.as_deref(), Some("<@1000000000000000005>"));
        assert_eq!(out.embed.title.as_deref(), Some("Test Server"));
        assert_eq!(out.embed.description.as_deref(), Some("newcomer 42 42nd"));
        assert_eq!(out.embed.fields[0].value, "<#1000000000000000003>");
        assert_eq!(out.embed.fields[1].value, "<@&1000000000000000004>");
    }

    /// The rendered output carries no placeholder token left over.
    #[test]
    fn no_placeholder_token_survives_rendering() {
        let src = "{\"content\": \"{user_mention} {username}\", \"embed\": {\"title\": \"{server}\", \
                   \"description\": \"{member_count} {member_ordinal} {channel:welcome} {role:member}\"}}";
        let t = WelcomeTemplate::load(src, &id_map()).expect("valid template loads");
        let out = render_welcome(&t, &member(), &id_map());
        let mut all = out.content.unwrap_or_default();
        all.push_str(out.embed.title.as_deref().unwrap_or_default());
        all.push_str(out.embed.description.as_deref().unwrap_or_default());
        for field in &out.embed.fields {
            all.push_str(&field.name);
            all.push_str(&field.value);
        }
        assert!(!all.contains('{'), "leftover placeholder in {all:?}");
    }

    /// Ordinals are correct English ordinals, including the teens.
    #[test]
    fn member_ordinals_are_correct() {
        let cases = [
            (1u64, "1st"),
            (2, "2nd"),
            (3, "3rd"),
            (4, "4th"),
            (10, "10th"),
            (11, "11th"),
            (12, "12th"),
            (13, "13th"),
            (14, "14th"),
            (20, "20th"),
            (21, "21st"),
            (22, "22nd"),
            (23, "23rd"),
            (24, "24th"),
            (31, "31st"),
            (101, "101st"),
            (111, "111th"),
            (112, "112th"),
            (113, "113th"),
            (121, "121st"),
            (1001, "1001st"),
            (1111, "1111th"),
            (1000000, "1000000th"),
        ];
        for (count, expected) in cases {
            let src = "{\"embed\": {\"description\": \"{member_ordinal}\"}}";
            let t = WelcomeTemplate::load(src, &id_map()).expect("valid template loads");
            let mut m = member();
            m.member_count = count;
            let out = render_welcome(&t, &m, &id_map());
            assert_eq!(
                out.embed.description.as_deref(),
                Some(expected),
                "ordinal of {count}"
            );
        }
    }

    /// The member count is rendered as a plain decimal number.
    #[test]
    fn member_count_is_a_plain_number() {
        let src = "{\"embed\": {\"description\": \"{member_count}\"}}";
        let t = WelcomeTemplate::load(src, &id_map()).expect("valid template loads");
        let mut m = member();
        m.member_count = 1234567;
        let out = render_welcome(&t, &m, &id_map());
        assert_eq!(out.embed.description.as_deref(), Some("1234567"));
    }

    // ---------------------------------------------------------------------------
    // Rule 2: the username is untrusted
    // ---------------------------------------------------------------------------

    /// A username with markdown characters is escaped, so it cannot forge formatting.
    #[test]
    fn username_markdown_is_escaped() {
        let src = "{\"embed\": {\"description\": \"{username}\"}}";
        let t = WelcomeTemplate::load(src, &id_map()).expect("valid template loads");
        let mut m = member();
        m.username = "**bold** __under__ `code` ~~strike~~ ||spoiler||".to_string();
        let out = render_welcome(&t, &m, &id_map());
        let rendered = out.embed.description.unwrap();
        assert!(!rendered.contains("**"), "markdown survived: {rendered:?}");
        assert!(!rendered.contains("__"), "markdown survived: {rendered:?}");
        assert!(!rendered.contains("~~"), "markdown survived: {rendered:?}");
        assert!(!rendered.contains("||"), "markdown survived: {rendered:?}");
        // The name itself is still readable.
        assert!(rendered.contains("bold"), "name was lost: {rendered:?}");
    }

    /// A username that contains `@everyone` cannot produce a mass mention.
    #[test]
    fn username_at_everyone_is_neutralised() {
        let src = "{\"embed\": {\"description\": \"{username}\"}}";
        let t = WelcomeTemplate::load(src, &id_map()).expect("valid template loads");
        for hostile in ["@everyone", "@here", "@ everyone", "@EveryOne", "@everyone"] {
            let mut m = member();
            m.username = hostile.to_string();
            let out = render_welcome(&t, &m, &id_map());
            let rendered = out.embed.description.unwrap();
            let lowered = rendered.to_lowercase();
            assert!(
                !lowered.contains("@everyone"),
                "{hostile:?} produced a mass mention: {rendered:?}"
            );
            assert!(
                !lowered.contains("@here"),
                "{hostile:?} produced a mass mention: {rendered:?}"
            );
        }
    }

    /// A username that contains a raw user or role mention cannot produce a mention.
    #[test]
    fn username_mention_syntax_is_neutralised() {
        let src = "{\"embed\": {\"description\": \"{username}\"}}";
        let t = WelcomeTemplate::load(src, &id_map()).expect("valid template loads");
        for hostile in [
            "<@1000000000000000005>",
            "<@!1000000000000000005>",
            "<@&1000000000000000004>",
            "<#1000000000000000003>",
        ] {
            let mut m = member();
            m.username = hostile.to_string();
            let out = render_welcome(&t, &m, &id_map());
            let rendered = out.embed.description.unwrap();
            assert!(
                !rendered.contains("<@") && !rendered.contains("<#") && !rendered.contains(">"),
                "{hostile:?} produced a mention: {rendered:?}"
            );
        }
    }

    /// Zero-width and bidi control characters are stripped from the username.
    #[test]
    fn username_control_characters_are_stripped() {
        let src = "{\"embed\": {\"description\": \"{username}\"}}";
        let t = WelcomeTemplate::load(src, &id_map()).expect("valid template loads");
        let mut m = member();
        m.username = "a\u{200b}b\u{200c}c\u{200d}d\u{feff}e\u{2060}f".to_string();
        let out = render_welcome(&t, &m, &id_map());
        let rendered = out.embed.description.unwrap();
        assert_eq!(rendered, "abcdef");
        for ch in ['\u{200b}', '\u{200c}', '\u{200d}', '\u{feff}', '\u{2060}'] {
            assert!(!rendered.contains(ch), "control char survived");
        }
    }

    /// Bidi override and isolate characters are stripped: they cannot reorder the text.
    #[test]
    fn username_bidi_controls_are_stripped() {
        let src = "{\"embed\": {\"description\": \"{username}\"}}";
        let t = WelcomeTemplate::load(src, &id_map()).expect("valid template loads");
        let mut m = member();
        m.username = "safe\u{202e}gnitpurroc\u{202c}end".to_string();
        let out = render_welcome(&t, &m, &id_map());
        let rendered = out.embed.description.unwrap();
        assert!(!rendered.contains('\u{202e}'));
        assert!(!rendered.contains('\u{202c}'));
        assert!(!rendered.contains('\u{2066}'));
        assert!(!rendered.contains('\u{2069}'));
    }

    /// The username is capped in length.
    #[test]
    fn username_length_is_capped() {
        let src = "{\"embed\": {\"description\": \"{username}\"}}";
        let t = WelcomeTemplate::load(src, &id_map()).expect("valid template loads");
        let mut m = member();
        m.username = "x".repeat(5000);
        let out = render_welcome(&t, &m, &id_map());
        let rendered = out.embed.description.unwrap();
        assert!(
            rendered.chars().count() <= 64,
            "username was not capped: {} chars",
            rendered.chars().count()
        );
    }

    /// A long username is capped by CHARACTERS, not bytes: a multi-byte name is not cut in
    /// the middle of a code point.
    #[test]
    fn username_cap_counts_characters_not_bytes() {
        let src = "{\"embed\": {\"description\": \"{username}\"}}";
        let t = WelcomeTemplate::load(src, &id_map()).expect("valid template loads");
        let mut m = member();
        m.username = "é".repeat(500);
        let out = render_welcome(&t, &m, &id_map());
        let rendered = out.embed.description.unwrap();
        assert!(rendered.chars().count() <= 64);
        assert!(rendered.is_char_boundary(rendered.len()));
        assert!(rendered.chars().all(|c| c == 'é'));
    }

    /// The untrusted username is escaped in EVERY place it can appear.
    #[test]
    fn username_is_escaped_in_every_place_it_can_appear() {
        let src = "{\"content\": \"{username}\", \"embed\": {\"title\": \"{username}\", \
                   \"description\": \"{username}\", \"author\": {\"name\": \"{username}\"}, \
                   \"footer\": {\"text\": \"{username}\"}, \
                   \"fields\": [{\"name\": \"{username}\", \"value\": \"{username}\"}]}}";
        let t = WelcomeTemplate::load(src, &id_map()).expect("valid template loads");
        let mut m = member();
        m.username = "**x** @everyone".to_string();
        let out = render_welcome(&t, &m, &id_map());
        let mut all = out.content.unwrap_or_default();
        all.push_str(out.embed.title.as_deref().unwrap_or_default());
        all.push_str(out.embed.description.as_deref().unwrap_or_default());
        all.push_str(out.embed.author_name.as_deref().unwrap_or_default());
        all.push_str(out.embed.footer_text.as_deref().unwrap_or_default());
        for field in &out.embed.fields {
            all.push_str(&field.name);
            all.push_str(&field.value);
        }
        assert!(!all.contains("**"), "markdown survived: {all:?}");
        assert!(
            !all.to_lowercase().contains("@everyone"),
            "ping survived: {all:?}"
        );
    }

    /// `{user_mention}` is the only place a mention is produced.
    #[test]
    fn only_user_mention_produces_a_mention() {
        let src = "{\"embed\": {\"description\": \"{username} {user_mention}\"}}";
        let t = WelcomeTemplate::load(src, &id_map()).expect("valid template loads");
        let mut m = member();
        m.username = "<@1000000000000000005>".to_string();
        let out = render_welcome(&t, &m, &id_map());
        let rendered = out.embed.description.unwrap();
        // Exactly one mention survives, and it is the intended one.
        assert_eq!(rendered.matches("<@").count(), 1, "in {rendered:?}");
        assert!(
            rendered.contains("<@1000000000000000005>"),
            "in {rendered:?}"
        );
    }

    /// The channel and role placeholders are the only other places a mention-shaped token
    /// may appear, and they carry an id that came from the caller's map.
    #[test]
    fn channel_and_role_placeholders_carry_caller_map_ids() {
        let src = "{\"embed\": {\"description\": \"{channel:rules} {role:member}\"}}";
        let t = WelcomeTemplate::load(src, &id_map()).expect("valid template loads");
        let out = render_welcome(&t, &member(), &id_map());
        assert_eq!(
            out.embed.description.as_deref(),
            Some("<#1000000000000000003> <@&1000000000000000004>")
        );
    }

    /// The rendered output contains no raw id that is not in the caller's map (and no id
    /// other than the joining user's own).
    #[test]
    fn output_contains_no_foreign_raw_id() {
        let src = "{\"embed\": {\"description\": \"{channel:welcome} {role:member}\", \
                   \"title\": \"{server}\", \
                   \"fields\": [{\"name\": \"n\", \"value\": \"{channel:rules}\"}]}}";
        let t = WelcomeTemplate::load(src, &id_map()).expect("valid template loads");
        let out = render_welcome(&t, &member(), &id_map());
        let mut all = out.embed.description.unwrap_or_default();
        all.push_str(&out.embed.title.unwrap_or_default());
        for field in &out.embed.fields {
            all.push_str(&field.name);
            all.push_str(&field.value);
        }
        let allowed = [
            WELCOME_CHANNEL.to_string(),
            RULES_CHANNEL.to_string(),
            MEMBER_ROLE.to_string(),
        ];
        for token in all.split(|c: char| !c.is_ascii_digit()) {
            if token.len() == 18 && token.chars().all(|c| c.is_ascii_digit()) {
                assert!(
                    allowed.contains(&token.to_string()),
                    "foreign id {token} appeared in {all:?}"
                );
            }
        }
    }

    /// The joining user's own id only appears inside the mention produced by
    /// `{user_mention}`.
    #[test]
    fn the_user_id_appears_only_in_the_user_mention() {
        let src = "{\"embed\": {\"description\": \"{username} {user_mention}\"}}";
        let t = WelcomeTemplate::load(src, &id_map()).expect("valid template loads");
        let mut m = member();
        m.username = "plainname".to_string();
        let out = render_welcome(&t, &m, &id_map());
        let rendered = out.embed.description.unwrap();
        assert_eq!(
            rendered.matches("1000000000000000005").count(),
            1,
            "in {rendered:?}"
        );
    }

    // ---------------------------------------------------------------------------
    // Rule 3: Discord embed limits are enforced at load
    // ---------------------------------------------------------------------------

    /// The title limit is exact: 256 characters load, 257 are refused.
    #[test]
    fn title_limit_is_exact() {
        let ok = format!(
            "{{\"embed\": {{\"title\": \"{}\"}}}}",
            "t".repeat(MAX_TITLE_CHARS)
        );
        WelcomeTemplate::load(&ok, &id_map()).expect("256 chars must load");
        let over = format!(
            "{{\"embed\": {{\"title\": \"{}\"}}}}",
            "t".repeat(MAX_TITLE_CHARS + 1)
        );
        let err = WelcomeTemplate::load(&over, &id_map()).expect_err("257 chars must be refused");
        assert_eq!(err.kind(), TemplateErrorKind::TitleTooLong);
    }

    /// The description limit is exact: 4096 characters load, 4097 are refused.
    #[test]
    fn description_limit_is_exact() {
        let ok = format!(
            "{{\"embed\": {{\"description\": \"{}\"}}}}",
            "d".repeat(MAX_DESCRIPTION_CHARS)
        );
        WelcomeTemplate::load(&ok, &id_map()).expect("4096 chars must load");
        let over = format!(
            "{{\"embed\": {{\"description\": \"{}\"}}}}",
            "d".repeat(MAX_DESCRIPTION_CHARS + 1)
        );
        let err = WelcomeTemplate::load(&over, &id_map()).expect_err("4097 chars must be refused");
        assert_eq!(err.kind(), TemplateErrorKind::DescriptionTooLong);
    }

    /// The field count limit is exact: 25 fields load, 26 are refused.
    #[test]
    fn field_count_limit_is_exact() {
        let many = |n: usize| {
            let fields: Vec<String> = (0..n)
                .map(|i| format!("{{\"name\": \"n{i}\", \"value\": \"v\"}}"))
                .collect();
            format!("{{\"embed\": {{\"fields\": [{}]}}}}", fields.join(","))
        };
        WelcomeTemplate::load(&many(MAX_EMBED_FIELDS), &id_map()).expect("25 fields must load");
        let err = WelcomeTemplate::load(&many(MAX_EMBED_FIELDS + 1), &id_map())
            .expect_err("26 fields must be refused");
        assert_eq!(err.kind(), TemplateErrorKind::TooManyFields);
    }

    /// The field name limit is exact: 256 characters load, 257 are refused.
    #[test]
    fn field_name_limit_is_exact() {
        let ok = format!(
            "{{\"embed\": {{\"fields\": [{{\"name\": \"{}\", \"value\": \"v\"}}]}}}}",
            "n".repeat(MAX_FIELD_NAME_CHARS)
        );
        WelcomeTemplate::load(&ok, &id_map()).expect("256 chars must load");
        let over = format!(
            "{{\"embed\": {{\"fields\": [{{\"name\": \"{}\", \"value\": \"v\"}}]}}}}",
            "n".repeat(MAX_FIELD_NAME_CHARS + 1)
        );
        let err = WelcomeTemplate::load(&over, &id_map()).expect_err("257 chars must be refused");
        assert_eq!(err.kind(), TemplateErrorKind::FieldNameTooLong);
    }

    /// The field value limit is exact: 1024 characters load, 1025 are refused.
    #[test]
    fn field_value_limit_is_exact() {
        let ok = format!(
            concat!(
                "{{\"embed\": {{\"fields\": [{{\"name\": \"n\", \"value\": \"",
                "{}",
                "\"}}]}}}}"
            ),
            "v".repeat(MAX_FIELD_VALUE_CHARS)
        );
        WelcomeTemplate::load(&ok, &id_map()).expect("1024 chars must load");
        let over = format!(
            concat!(
                "{{\"embed\": {{\"fields\": [{{\"name\": \"n\", \"value\": \"",
                "{}",
                "\"}}]}}}}"
            ),
            "v".repeat(MAX_FIELD_VALUE_CHARS + 1)
        );
        let err = WelcomeTemplate::load(&over, &id_map()).expect_err("1025 chars must be refused");
        assert_eq!(err.kind(), TemplateErrorKind::FieldValueTooLong);
    }

    /// The footer limit is exact: 2048 characters load, 2049 are refused.
    #[test]
    fn footer_limit_is_exact() {
        let ok = format!(
            "{{\"embed\": {{\"footer\": {{\"text\": \"{}\"}}}}}}",
            "f".repeat(MAX_FOOTER_CHARS)
        );
        WelcomeTemplate::load(&ok, &id_map()).expect("2048 chars must load");
        let over = format!(
            "{{\"embed\": {{\"footer\": {{\"text\": \"{}\"}}}}}}",
            "f".repeat(MAX_FOOTER_CHARS + 1)
        );
        let err = WelcomeTemplate::load(&over, &id_map()).expect_err("2049 chars must be refused");
        assert_eq!(err.kind(), TemplateErrorKind::FooterTooLong);
    }

    /// The author name limit is exact: 256 characters load, 257 are refused.
    #[test]
    fn author_name_limit_is_exact() {
        let ok = format!(
            "{{\"embed\": {{\"author\": {{\"name\": \"{}\"}}}}}}",
            "a".repeat(MAX_TITLE_CHARS)
        );
        WelcomeTemplate::load(&ok, &id_map()).expect("256 chars must load");
        let over = format!(
            "{{\"embed\": {{\"author\": {{\"name\": \"{}\"}}}}}}",
            "a".repeat(MAX_TITLE_CHARS + 1)
        );
        let err = WelcomeTemplate::load(&over, &id_map()).expect_err("257 chars must be refused");
        assert_eq!(err.kind(), TemplateErrorKind::AuthorNameTooLong);
    }

    /// The total embed limit is enforced: the sum of all embed text must be at most 6000
    /// characters, and one character over is refused.
    #[test]
    fn total_embed_limit_is_exact() {
        // 25 fields of 1024 characters each is 25 600 characters: over the total, so it must
        // be refused even though every individual part is within its own limit.
        let fields: Vec<String> = (0..25)
            .map(|i| {
                format!(
                    "{{\"name\": \"n{i}\", \"value\": \"{}\"}}",
                    "v".repeat(1024)
                )
            })
            .collect();
        let over = format!("{{\"embed\": {{\"fields\": [{}]}}}}", fields.join(","));
        let err =
            WelcomeTemplate::load(&over, &id_map()).expect_err("over the total must be refused");
        assert_eq!(err.kind(), TemplateErrorKind::EmbedTotalTooLong);

        // A description of 4096 plus a title of 256 plus one field of 1024 is 5376, plus a
        // footer of 624 is exactly 6000: that must load.
        let exact = format!(
            "{{\"embed\": {{\"title\": \"{}\", \"description\": \"{}\", \
             \"footer\": {{\"text\": \"{}\"}}, \
             \"fields\": [{{\"name\": \"n\", \"value\": \"{}\"}}]}}}}",
            "t".repeat(MAX_TITLE_CHARS),
            "d".repeat(MAX_DESCRIPTION_CHARS),
            "f".repeat(624),
            "v".repeat(MAX_FIELD_VALUE_CHARS)
        );
        WelcomeTemplate::load(&exact, &id_map()).expect("exactly 6000 chars must load");

        let one_over = format!(
            "{{\"embed\": {{\"title\": \"{}\", \"description\": \"{}\", \
             \"footer\": {{\"text\": \"{}\"}}, \
             \"fields\": [{{\"name\": \"n\", \"value\": \"{}\"}}]}}}}",
            "t".repeat(MAX_TITLE_CHARS),
            "d".repeat(MAX_DESCRIPTION_CHARS),
            "f".repeat(625),
            "v".repeat(MAX_FIELD_VALUE_CHARS)
        );
        let err =
            WelcomeTemplate::load(&one_over, &id_map()).expect_err("6001 chars must be refused");
        assert_eq!(err.kind(), TemplateErrorKind::EmbedTotalTooLong);
    }

    /// The limits count CHARACTERS, not bytes: a multi-byte title of 256 characters is
    /// accepted even though it is many more bytes.
    #[test]
    fn limits_count_characters_not_bytes() {
        let ok = format!(
            "{{\"embed\": {{\"title\": \"{}\"}}}}",
            "é".repeat(MAX_TITLE_CHARS)
        );
        WelcomeTemplate::load(&ok, &id_map()).expect("256 multi-byte chars must load");
        let over = format!(
            "{{\"embed\": {{\"title\": \"{}\"}}}}",
            "é".repeat(MAX_TITLE_CHARS + 1)
        );
        let err =
            WelcomeTemplate::load(&over, &id_map()).expect_err("257 multi-byte chars refused");
        assert_eq!(err.kind(), TemplateErrorKind::TitleTooLong);
    }

    /// The content length limit is enforced: the Discord message content limit is 2000
    /// characters.
    #[test]
    fn content_limit_is_enforced() {
        let ok = format!(
            "{{\"content\": \"{}\", \"embed\": {{\"title\": \"t\"}}}}",
            "c".repeat(2000)
        );
        WelcomeTemplate::load(&ok, &id_map()).expect("2000 chars must load");
        let over = format!(
            "{{\"content\": \"{}\", \"embed\": {{\"title\": \"t\"}}}}",
            "c".repeat(2001)
        );
        let err = WelcomeTemplate::load(&over, &id_map()).expect_err("2001 chars must be refused");
        assert_eq!(err.kind(), TemplateErrorKind::ContentTooLong);
    }

    /// A missing field name or value is refused: Discord would reject the embed.
    #[test]
    fn empty_field_name_or_value_is_refused() {
        let src = "{\"embed\": {\"fields\": [{\"name\": \"\", \"value\": \"v\"}]}}";
        let err = WelcomeTemplate::load(src, &id_map()).expect_err("empty name must be refused");
        assert_eq!(err.kind(), TemplateErrorKind::EmptyFieldName);
        let src = "{\"embed\": {\"fields\": [{\"name\": \"n\", \"value\": \"\"}]}}";
        let err = WelcomeTemplate::load(src, &id_map()).expect_err("empty value must be refused");
        assert_eq!(err.kind(), TemplateErrorKind::EmptyFieldValue);
    }

    // ---------------------------------------------------------------------------
    // Rule 4: bots are skipped
    // ---------------------------------------------------------------------------

    /// A bot member is skipped by the policy.
    #[test]
    fn bot_member_is_skipped() {
        let src = "{\"embed\": {\"title\": \"t\"}}";
        let t = WelcomeTemplate::load(src, &id_map()).expect("valid template loads");
        let mut m = member();
        m.is_bot = true;
        let decision = welcome_policy(&t, &m, &id_map());
        assert_eq!(decision, SkipReason::Bot);
    }

    /// A human member is not skipped.
    #[test]
    fn human_member_is_not_skipped() {
        let src = "{\"embed\": {\"title\": \"t\"}}";
        let t = WelcomeTemplate::load(src, &id_map()).expect("valid template loads");
        assert_eq!(welcome_policy(&t, &member(), &id_map()), SkipReason::None);
    }

    /// A bot member is skipped even when the template would render fine.
    #[test]
    fn bot_is_skipped_before_rendering() {
        let src = "{\"embed\": {\"title\": \"{username}\"}}";
        let t = WelcomeTemplate::load(src, &id_map()).expect("valid template loads");
        let mut m = member();
        m.is_bot = true;
        m.username = "@everyone".to_string();
        let decision = welcome_policy(&t, &m, &id_map());
        assert_eq!(decision, SkipReason::Bot);
    }

    // ---------------------------------------------------------------------------
    // The example template
    // ---------------------------------------------------------------------------

    /// The example template file loads and renders.
    #[test]
    fn the_example_template_loads_and_renders() {
        let src = include_str!("../../docs/examples/welcome.example.json");
        let t = WelcomeTemplate::load(src, &id_map()).expect("the example template must load");
        let out = render_welcome(&t, &member(), &id_map());
        assert!(out.content.is_some(), "the example must render content");
        assert!(out.embed.title.is_some(), "the example must render a title");
        assert!(
            !out.embed.fields.is_empty(),
            "the example must render fields"
        );
        let mut all = out.content.unwrap_or_default();
        all.push_str(out.embed.title.as_deref().unwrap_or_default());
        all.push_str(out.embed.description.as_deref().unwrap_or_default());
        all.push_str(out.embed.footer_text.as_deref().unwrap_or_default());
        for field in &out.embed.fields {
            all.push_str(&field.name);
            all.push_str(&field.value);
        }
        assert!(!all.contains('{'), "leftover placeholder in {all:?}");
    }

    /// The example template expresses the YAGPDB welcome: it mentions the member count,
    /// the member, the rules channels and the role channels.
    #[test]
    fn the_example_template_expresses_the_yagpdb_welcome() {
        let src = include_str!("../../docs/examples/welcome.example.json");
        let t = WelcomeTemplate::load(src, &id_map()).expect("the example template must load");
        let out = render_welcome(&t, &member(), &id_map());
        assert!(out
            .content
            .as_deref()
            .unwrap_or_default()
            .contains("<@1000000000000000005>"));
        let desc = out.embed.description.unwrap_or_default();
        assert!(desc.contains("42"), "member count missing: {desc:?}");
        let footer = out.embed.footer_text.unwrap_or_default();
        assert!(
            footer.contains("42"),
            "member count missing from footer: {footer:?}"
        );
        assert!(
            footer.contains("newcomer"),
            "username missing from footer: {footer:?}"
        );
        let rules_channel_mentioned = out
            .embed
            .fields
            .iter()
            .any(|f| f.value.contains("<#1000000000000000003>"));
        assert!(rules_channel_mentioned, "rules channel missing");
    }

    /// The example template contains no raw id: only placeholders and named keys.
    #[test]
    fn the_example_template_contains_no_raw_id() {
        let src = include_str!("../../docs/examples/welcome.example.json");
        for token in src.split(|c: char| !c.is_ascii_digit()) {
            if token.len() >= 15 && token.chars().all(|c| c.is_ascii_digit()) {
                panic!("the example template contains the raw id {token}");
            }
        }
    }

    /// The example template uses placeholder keys that the caller's map resolves.
    #[test]
    fn the_example_template_keys_are_documented() {
        let src = include_str!("../../docs/examples/welcome.example.json");
        let t = WelcomeTemplate::load(src, &id_map()).expect("the example template must load");
        // The template renders with the map used by the tests, so every key it uses is a
        // key the caller must supply.
        let out = render_welcome(&t, &member(), &id_map());
        assert!(
            out.embed.fields.len() >= 5,
            "the example must keep its fields"
        );
    }

    // ---------------------------------------------------------------------------
    // Id map
    // ---------------------------------------------------------------------------

    /// The id map keeps channels and roles apart: a channel key never resolves to a role.
    #[test]
    fn id_map_keeps_channels_and_roles_apart() {
        let mut map = IdMap::default();
        map.insert_channel("shared", 111);
        map.insert_role("shared", 222);
        assert_eq!(map.channel("shared"), Some(111));
        assert_eq!(map.role("shared"), Some(222));
        assert_eq!(map.channel("missing"), None);
        assert_eq!(map.role("missing"), None);
    }

    /// The id map refuses the id zero, which is not a real Discord id.
    #[test]
    fn id_map_refuses_the_id_zero() {
        let mut map = IdMap::default();
        assert!(!map.insert_channel("zero", 0));
        assert!(!map.insert_role("zero", 0));
        assert_eq!(map.channel("zero"), None);
        assert_eq!(map.role("zero"), None);
    }

    /// The id map refuses a key that is not a plain word.
    #[test]
    fn id_map_refuses_a_malformed_key() {
        let mut map = IdMap::default();
        assert!(!map.insert_channel("bad key", 111));
        assert!(!map.insert_channel("", 111));
        assert!(!map.insert_role("bad-key", 111));
        assert_eq!(map.channel("bad key"), None);
    }

    /// A key that is inserted twice keeps the first id: a caller cannot smuggle a second id
    /// into the same key.
    #[test]
    fn id_map_keeps_the_first_id_for_a_key() {
        let mut map = IdMap::default();
        assert!(map.insert_channel("rules", 111));
        assert!(!map.insert_channel("rules", 222));
        assert_eq!(map.channel("rules"), Some(111));
    }

    /// The map is built from plain pairs, so a wiring card can fill it from `.env`.
    #[test]
    fn id_map_can_be_built_from_pairs() {
        let mut map = IdMap::default();
        map.extend_channels([("a", 1u64), ("b", 2)]);
        map.extend_roles([("r", 3u64)]);
        assert_eq!(map.channel("a"), Some(1));
        assert_eq!(map.channel("b"), Some(2));
        assert_eq!(map.role("r"), Some(3));
        assert_eq!(map.channels().len(), 2);
        assert_eq!(map.roles().len(), 1);
    }

    // ---------------------------------------------------------------------------
    // Error hygiene
    // ---------------------------------------------------------------------------

    /// The error codes are stable, distinct and never carry template text.
    #[test]
    fn error_codes_are_stable_and_carry_no_text() {
        let src = "{\"embed\": {\"title\": \"{nope}\"}}";
        let err = WelcomeTemplate::load(src, &id_map()).expect_err("must be refused");
        let text = err.to_string();
        let debug = format!("{err:?}");
        for forbidden in ["nope", "1000000000000000000", "@", "<"] {
            assert!(
                !text.contains(forbidden),
                "error text carries {forbidden}: {text}"
            );
            assert!(
                !debug.contains(forbidden),
                "error debug carries {forbidden}: {debug}"
            );
        }
    }

    /// Every error kind has its own stable code, and the codes are all distinct.
    #[test]
    fn error_kinds_are_distinct() {
        let kinds = [
            TemplateErrorKind::MalformedJson,
            TemplateErrorKind::MissingEmbed,
            TemplateErrorKind::UnknownField,
            TemplateErrorKind::UnknownPlaceholder,
            TemplateErrorKind::MalformedPlaceholder,
            TemplateErrorKind::UnclosedPlaceholder,
            TemplateErrorKind::UnknownChannelKey,
            TemplateErrorKind::UnknownRoleKey,
            TemplateErrorKind::TitleTooLong,
            TemplateErrorKind::DescriptionTooLong,
            TemplateErrorKind::TooManyFields,
            TemplateErrorKind::FieldNameTooLong,
            TemplateErrorKind::FieldValueTooLong,
            TemplateErrorKind::FooterTooLong,
            TemplateErrorKind::AuthorNameTooLong,
            TemplateErrorKind::EmbedTotalTooLong,
            TemplateErrorKind::ContentTooLong,
            TemplateErrorKind::EmptyFieldName,
            TemplateErrorKind::EmptyFieldValue,
        ];
        let mut seen = BTreeMap::new();
        for kind in kinds {
            let code = kind.code();
            assert!(!code.is_empty(), "{kind:?} has an empty code");
            assert!(seen.insert(code, kind).is_none(), "duplicate code {code}");
        }
        assert!(seen.len() >= 19, "unexpected number of codes");
    }

    /// The error implements `std::error::Error` so a wiring card can propagate it.
    #[test]
    fn error_is_a_std_error() {
        let src = "{\"embed\": {\"title\": \"{nope}\"}}";
        let err = WelcomeTemplate::load(src, &id_map()).expect_err("must be refused");
        let boxed: Box<dyn std::error::Error> = Box::new(err);
        assert!(!boxed.to_string().is_empty());
    }

    // ---------------------------------------------------------------------------
    // Robustness of the template source
    // ---------------------------------------------------------------------------

    /// A template with a `null` content and no fields is valid: Discord allows it.
    #[test]
    fn null_content_and_absent_fields_are_valid() {
        let src = "{\"content\": null, \"embed\": {\"title\": \"t\"}}";
        let t = WelcomeTemplate::load(src, &id_map()).expect("valid template loads");
        assert!(t.content().is_none());
        assert!(t.embed().fields.is_empty());
        let out = render_welcome(&t, &member(), &id_map());
        assert!(out.content.is_none());
        assert!(out.embed.fields.is_empty());
    }

    /// An empty content string renders to no content at all, not an empty message.
    #[test]
    fn empty_content_renders_to_nothing() {
        let src = "{\"content\": \"\", \"embed\": {\"title\": \"t\"}}";
        let t = WelcomeTemplate::load(src, &id_map()).expect("valid template loads");
        let out = render_welcome(&t, &member(), &id_map());
        assert!(out.content.is_none());
    }

    /// A literal `{{` is not a placeholder escape; it is refused as an unknown placeholder
    /// rather than silently rendered.
    #[test]
    fn double_brace_is_refused_rather_than_silently_rendered() {
        let src = "{\"embed\": {\"description\": \"{{user_mention}}\"}}";
        let err = WelcomeTemplate::load(src, &id_map()).expect_err("must be refused");
        assert_eq!(err.kind(), TemplateErrorKind::UnknownPlaceholder);
    }

    /// The inline flag of a field is kept.
    #[test]
    fn field_inline_flag_is_kept() {
        let src =
            "{\"embed\": {\"fields\": [{\"name\": \"n\", \"value\": \"v\", \"inline\": true}]}}";
        let t = WelcomeTemplate::load(src, &id_map()).expect("valid template loads");
        assert!(t.embed().fields[0].inline);
        let out = render_welcome(&t, &member(), &id_map());
        assert!(out.embed.fields[0].inline);
    }

    /// A field without an inline flag defaults to false.
    #[test]
    fn field_inline_defaults_to_false() {
        let src = "{\"embed\": {\"fields\": [{\"name\": \"n\", \"value\": \"v\"}]}}";
        let t = WelcomeTemplate::load(src, &id_map()).expect("valid template loads");
        assert!(!t.embed().fields[0].inline);
    }

    /// The rendered embed is plain data: it holds strings, not serenity types.
    #[test]
    fn rendered_embed_is_plain_data() {
        let src = "{\"embed\": {\"title\": \"t\", \"description\": \"d\", \
                   \"fields\": [{\"name\": \"n\", \"value\": \"v\"}], \
                   \"footer\": {\"text\": \"f\"}, \"author\": {\"name\": \"a\"}}}";
        let t = WelcomeTemplate::load(src, &id_map()).expect("valid template loads");
        let out = render_welcome(&t, &member(), &id_map());
        let embed: &RenderedEmbed = &out.embed;
        assert_eq!(embed.title.as_deref(), Some("t"));
        assert_eq!(embed.description.as_deref(), Some("d"));
        assert_eq!(embed.footer_text.as_deref(), Some("f"));
        assert_eq!(embed.author_name.as_deref(), Some("a"));
        let field: &RenderedField = &embed.fields[0];
        assert_eq!(field.name, "n");
        assert_eq!(field.value, "v");
    }
}
