//! Pure welcome-template renderer: loading, placeholder rules, escaping and limits.
//!
//! Everything here is Discord-free: a template is loaded from a JSON `&str`, and the
//! renderer fills placeholders for one joining member into plain data.
//!
//! ## Rules
//!
//! 1. An unknown placeholder or an unknown `{channel:KEY}` / `{role:KEY}` key is
//!    refused when the template is LOADED, not when a member joins: a typo must be
//!    caught at startup, never produce a broken welcome in production.
//! 2. The joining user's `username` is untrusted. It is stripped of zero-width and
//!    bidi control characters, neutralised against `@everyone` / `@here` and role or
//!    user mention syntax, escaped against markdown and capped in length before it is
//!    placed anywhere. The cap applies to the ESCAPED name, so the rendered name is
//!    never longer than [`MAX_USERNAME_CHARS`]. The server name is caller-supplied
//!    free text too: it is stripped of control characters, neutralised against the
//!    mention syntax and capped at [`MAX_SERVER_NAME_CHARS`]. `{user_mention}` is the
//!    only way a user mention is produced.
//! 3. The Discord embed limits are checked on the TEMPLATE TEXT at load: title 256,
//!    description 4096, at most 25 fields, field name 256, field value 1024, footer
//!    2048, author name 256 and 6000 characters in total. The limits count
//!    characters, not bytes.
//! 4. A placeholder EXPANDS at render time, so the template text being inside the
//!    limits does not mean the rendered text is. The renderer therefore clamps the
//!    rendered result to the same limits: every slot is shortened to its own limit,
//!    and the embed is shortened until its total fits 6000 characters. When something
//!    was cut, [`RenderedWelcome::truncated`] is true. The order in which the embed
//!    is shortened is fixed and documented on [`clamp_embed`].
//! 5. A bot member is skipped by [`welcome_policy`].
//! 6. The rendered output carries no raw id that did not come from the caller's map.
//!
//! There is no serenity type, no I/O, no clock read and no randomness: every id, the
//! member count and the server name are passed in by the caller.
//!
//! Errors are stable enum codes. They never carry template text, user names or user
//! ids, so nothing private can leak through an error path.

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
/// The maximum length of the server name, in characters.
///
/// The server name is caller-supplied free text, like the username. It is capped so
/// that one placeholder has a bounded expansion.
pub const MAX_SERVER_NAME_CHARS: usize = 100;
/// The maximum number of digits an id can have: a `u64` holds at most twenty.
const MAX_ID_DIGITS: usize = 20;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Stable reason codes for a template that was refused.
///
/// The codes never carry the offending value.
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
    /// A field name is not a string.
    MalformedFieldName,
    /// A field value is not a string.
    MalformedFieldValue,
}

impl TemplateErrorKind {
    /// The stable machine-readable code for this refusal.
    ///
    /// The code is a fixed ASCII word and never carries template text or an id.
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
            TemplateErrorKind::MalformedFieldName => "malformed_field_name",
            TemplateErrorKind::MalformedFieldValue => "malformed_field_value",
        }
    }
}

/// A refused template.
///
/// The error names the stable reason code and never echoes the template text, a user
/// name or an id, so it is safe to log verbatim.
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

// ---------------------------------------------------------------------------
// The named id map
// ---------------------------------------------------------------------------

/// The named ids a template may refer to.
///
/// The template never contains a raw id: it names a key such as `{channel:rules}` and
/// the caller resolves it through this map. The map keeps channels and roles apart, so
/// a channel key can never resolve to a role.
///
/// A key must be a plain word (`[A-Za-z0-9_]+`) and an id must not be zero. The first
/// id inserted for a key wins, so a caller cannot smuggle a second id into a key.
///
/// The caller must hand the SAME map to [`WelcomeTemplate::load`] and to
/// [`render_welcome`]. A key that the map does not have is refused at load time; a
/// key that the map does not have at RENDER time renders as an empty string, because
/// the renderer does not fail a join on a missing id.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdMap {
    channels: BTreeMap<String, u64>,
    roles: BTreeMap<String, u64>,
}

impl IdMap {
    /// Adds a channel id under `key`.
    ///
    /// Returns false when the key is malformed, the id is zero or the key already
    /// holds an id (the first id wins).
    pub fn insert_channel(&mut self, key: &str, id: u64) -> bool {
        insert_id(&mut self.channels, key, id)
    }

    /// Adds a role id under `key`.
    ///
    /// Returns false when the key is malformed, the id is zero or the key already
    /// holds an id (the first id wins).
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

/// True when the key is a plain word: letters, digits and the underscore.
fn is_plain_key(key: &str) -> bool {
    !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
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

// ---------------------------------------------------------------------------
// The rendered result
// ---------------------------------------------------------------------------

/// The plain facts about the member that joined.
///
/// The username is untrusted: see [`render_welcome`].
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
///
/// It holds strings only: no serenity type appears here, so the wiring card decides
/// how to build the Discord message.
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
    /// True when a part of the template was cut so that the rendered message stays
    /// inside the Discord limits.
    pub truncated: bool,
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
///
/// The template text was checked at load time: every placeholder is known, every
/// `{channel:KEY}` and `{role:KEY}` key resolves and the template text is inside the
/// Discord limits. A placeholder still EXPANDS when the template is rendered, so the
/// rendered result is clamped to those limits by [`render_welcome`]; a template that
/// loaded can always be rendered, but a long placeholder can cost the owner some of
/// the template text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WelcomeTemplate {
    content: Option<String>,
    embed: RenderedEmbed,
}

impl WelcomeTemplate {
    /// Loads and validates a template from a JSON source.
    ///
    /// The source describes one embed plus an optional message content:
    ///
    /// ```json
    /// {
    ///   "content": "Welcome {user_mention}",
    ///   "embed": {
    ///     "title": "Welcome to {server}",
    ///     "description": "You are our {member_ordinal} member.",
    ///     "fields": [{"name": "Rules", "value": "See {channel:rules}", "inline": false}],
    ///     "footer": {"text": "Total members: {member_count}"},
    ///     "author": {"name": "{user_mention}"}
    ///   }
    /// }
    /// ```
    ///
    /// `ids` resolves the named keys the template refers to. A key the map does not
    /// have is refused here, at load time, not when a member joins.
    ///
    /// This function performs no file I/O: the caller reads the file and passes the
    /// text in.
    pub fn load(src: &str, ids: &IdMap) -> Result<Self, TemplateError> {
        let value: serde_json::Value = serde_json::from_str(src)
            .map_err(|_| TemplateError::new(TemplateErrorKind::MalformedJson))?;
        let root = value
            .as_object()
            .ok_or_else(|| TemplateError::new(TemplateErrorKind::MalformedJson))?;

        for key in root.keys() {
            if key != "content" && key != "embed" {
                return Err(TemplateError::new(TemplateErrorKind::UnknownField));
            }
        }

        let content = match root.get("content") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(text)) => {
                check_placeholders(text, ids)?;
                if text.chars().count() > MAX_CONTENT_CHARS {
                    return Err(TemplateError::new(TemplateErrorKind::ContentTooLong));
                }
                Some(text.clone())
            }
            Some(_) => return Err(TemplateError::new(TemplateErrorKind::MalformedJson)),
        };

        let embed_value = root
            .get("embed")
            .ok_or_else(|| TemplateError::new(TemplateErrorKind::MissingEmbed))?;
        let embed = parse_embed(embed_value, ids)?;

        Ok(Self { content, embed })
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

/// Parses and validates the embed object.
fn parse_embed(value: &serde_json::Value, ids: &IdMap) -> Result<RenderedEmbed, TemplateError> {
    let obj = value
        .as_object()
        .ok_or_else(|| TemplateError::new(TemplateErrorKind::MalformedJson))?;

    for key in obj.keys() {
        match key.as_str() {
            "title" | "description" | "fields" | "footer" | "author" => {}
            _ => return Err(TemplateError::new(TemplateErrorKind::UnknownField)),
        }
    }

    let title = optional_text(obj.get("title"), ids)?;
    let description = optional_text(obj.get("description"), ids)?;

    let mut fields = Vec::new();
    match obj.get("fields") {
        None | Some(serde_json::Value::Null) => {}
        Some(serde_json::Value::Array(list)) => {
            if list.len() > MAX_EMBED_FIELDS {
                return Err(TemplateError::new(TemplateErrorKind::TooManyFields));
            }
            for item in list {
                fields.push(parse_field(item, ids)?);
            }
        }
        Some(_) => return Err(TemplateError::new(TemplateErrorKind::MalformedJson)),
    }

    let footer_text = match obj.get("footer") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Object(footer)) => {
            for key in footer.keys() {
                if key != "text" {
                    return Err(TemplateError::new(TemplateErrorKind::UnknownField));
                }
            }
            match footer.get("text") {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(text)) => {
                    check_placeholders(text, ids)?;
                    if text.chars().count() > MAX_FOOTER_CHARS {
                        return Err(TemplateError::new(TemplateErrorKind::FooterTooLong));
                    }
                    Some(text.clone())
                }
                Some(_) => return Err(TemplateError::new(TemplateErrorKind::MalformedJson)),
            }
        }
        Some(_) => return Err(TemplateError::new(TemplateErrorKind::MalformedJson)),
    };

    let author_name = match obj.get("author") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Object(author)) => {
            for key in author.keys() {
                if key != "name" {
                    return Err(TemplateError::new(TemplateErrorKind::UnknownField));
                }
            }
            match author.get("name") {
                None | Some(serde_json::Value::Null) => None,
                Some(serde_json::Value::String(text)) => {
                    check_placeholders(text, ids)?;
                    if text.chars().count() > MAX_TITLE_CHARS {
                        return Err(TemplateError::new(TemplateErrorKind::AuthorNameTooLong));
                    }
                    Some(text.clone())
                }
                Some(_) => return Err(TemplateError::new(TemplateErrorKind::MalformedJson)),
            }
        }
        Some(_) => return Err(TemplateError::new(TemplateErrorKind::MalformedJson)),
    };

    if let Some(title) = &title {
        if title.chars().count() > MAX_TITLE_CHARS {
            return Err(TemplateError::new(TemplateErrorKind::TitleTooLong));
        }
    }
    if let Some(description) = &description {
        if description.chars().count() > MAX_DESCRIPTION_CHARS {
            return Err(TemplateError::new(TemplateErrorKind::DescriptionTooLong));
        }
    }

    let embed = RenderedEmbed {
        title,
        description,
        fields,
        footer_text,
        author_name,
    };
    if embed_total_chars(&embed) > MAX_EMBED_TOTAL_CHARS {
        return Err(TemplateError::new(TemplateErrorKind::EmbedTotalTooLong));
    }
    Ok(embed)
}

/// Parses and validates one field object.
fn parse_field(value: &serde_json::Value, ids: &IdMap) -> Result<RenderedField, TemplateError> {
    let obj = value
        .as_object()
        .ok_or_else(|| TemplateError::new(TemplateErrorKind::MalformedJson))?;

    for key in obj.keys() {
        match key.as_str() {
            "name" | "value" | "inline" => {}
            _ => return Err(TemplateError::new(TemplateErrorKind::UnknownField)),
        }
    }

    let name = match obj.get("name") {
        Some(serde_json::Value::String(text)) => text.clone(),
        // A name that is not a string is a malformed field, not an empty name.
        _ => return Err(TemplateError::new(TemplateErrorKind::MalformedFieldName)),
    };
    if name.is_empty() {
        return Err(TemplateError::new(TemplateErrorKind::EmptyFieldName));
    }
    let value_text = match obj.get("value") {
        Some(serde_json::Value::String(text)) => text.clone(),
        // A value that is not a string is a malformed field, not an empty value.
        _ => return Err(TemplateError::new(TemplateErrorKind::MalformedFieldValue)),
    };
    if value_text.is_empty() {
        return Err(TemplateError::new(TemplateErrorKind::EmptyFieldValue));
    }

    check_placeholders(&name, ids)?;
    check_placeholders(&value_text, ids)?;

    if name.chars().count() > MAX_FIELD_NAME_CHARS {
        return Err(TemplateError::new(TemplateErrorKind::FieldNameTooLong));
    }
    if value_text.chars().count() > MAX_FIELD_VALUE_CHARS {
        return Err(TemplateError::new(TemplateErrorKind::FieldValueTooLong));
    }

    let inline = match obj.get("inline") {
        None | Some(serde_json::Value::Null) => false,
        Some(serde_json::Value::Bool(flag)) => *flag,
        Some(_) => return Err(TemplateError::new(TemplateErrorKind::MalformedJson)),
    };

    Ok(RenderedField {
        name,
        value: value_text,
        inline,
    })
}

/// Reads an optional string field and validates its placeholders.
fn optional_text(
    value: Option<&serde_json::Value>,
    ids: &IdMap,
) -> Result<Option<String>, TemplateError> {
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(text)) => {
            check_placeholders(text, ids)?;
            Ok(Some(text.clone()))
        }
        Some(_) => Err(TemplateError::new(TemplateErrorKind::MalformedJson)),
    }
}

/// The number of characters in the whole embed.
///
/// Discord counts the title, the description, every field name, every field value, the
/// footer text and the author name. The characters are counted, not the bytes.
fn embed_total_chars(embed: &RenderedEmbed) -> usize {
    let mut total = 0usize;
    if let Some(title) = &embed.title {
        total += title.chars().count();
    }
    if let Some(description) = &embed.description {
        total += description.chars().count();
    }
    for field in &embed.fields {
        total += field.name.chars().count();
        total += field.value.chars().count();
    }
    if let Some(footer) = &embed.footer_text {
        total += footer.chars().count();
    }
    if let Some(author) = &embed.author_name {
        total += author.chars().count();
    }
    total
}

// ---------------------------------------------------------------------------
// Placeholders
// ---------------------------------------------------------------------------

/// The placeholders that do not need a key.
const SCALAR_PLACEHOLDERS: [&str; 5] = [
    "user_mention",
    "username",
    "server",
    "member_count",
    "member_ordinal",
];

/// Checks every placeholder in `text`.
///
/// An unknown placeholder, an unknown key or an unbalanced brace is refused here, at
/// load time. The check never returns the offending text.
fn check_placeholders(text: &str, ids: &IdMap) -> Result<(), TemplateError> {
    for (kind, key) in placeholder_tokens(text)? {
        match kind {
            PlaceholderKind::Scalar => {
                if !SCALAR_PLACEHOLDERS.contains(&key) {
                    return Err(TemplateError::new(TemplateErrorKind::UnknownPlaceholder));
                }
            }
            PlaceholderKind::Channel => {
                if !is_plain_key(key) {
                    return Err(TemplateError::new(TemplateErrorKind::MalformedPlaceholder));
                }
                if ids.channel(key).is_none() {
                    return Err(TemplateError::new(TemplateErrorKind::UnknownChannelKey));
                }
            }
            PlaceholderKind::Role => {
                if !is_plain_key(key) {
                    return Err(TemplateError::new(TemplateErrorKind::MalformedPlaceholder));
                }
                if ids.role(key).is_none() {
                    return Err(TemplateError::new(TemplateErrorKind::UnknownRoleKey));
                }
            }
        }
    }
    Ok(())
}

/// The kind of a placeholder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaceholderKind {
    /// A placeholder without a key, such as `{username}`.
    Scalar,
    /// A `{channel:KEY}` placeholder.
    Channel,
    /// A `{role:KEY}` placeholder.
    Role,
}

/// Splits `text` into the placeholder tokens it contains.
///
/// The text is scanned once. A `{` must be closed by a `}`: an unbalanced brace is an
/// error, never a literal brace. The literal content of the text is not returned, so
/// no user text can leak through this function.
fn placeholder_tokens(text: &str) -> Result<Vec<(PlaceholderKind, &str)>, TemplateError> {
    let mut tokens = Vec::new();
    let bytes = text.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] == b'}' {
            // A `}` that closes nothing: the braces are unbalanced.
            return Err(TemplateError::new(TemplateErrorKind::UnclosedPlaceholder));
        }
        if bytes[index] != b'{' {
            index += 1;
            continue;
        }
        let close = match text[index + 1..].find('}') {
            Some(offset) => index + 1 + offset,
            None => return Err(TemplateError::new(TemplateErrorKind::UnclosedPlaceholder)),
        };
        let inner = &text[index + 1..close];
        if inner.contains('{') {
            // A second `{` before the closing `}`: the body names no placeholder.
            return Err(TemplateError::new(TemplateErrorKind::UnknownPlaceholder));
        }
        let (kind, key) = classify_placeholder(inner)?;
        tokens.push((kind, key));
        index = close + 1;
    }
    Ok(tokens)
}

/// Splits a placeholder body into its kind and its key.
fn classify_placeholder(inner: &str) -> Result<(PlaceholderKind, &str), TemplateError> {
    match inner.split_once(':') {
        None => {
            if inner.is_empty() {
                return Err(TemplateError::new(TemplateErrorKind::UnknownPlaceholder));
            }
            Ok((PlaceholderKind::Scalar, inner))
        }
        Some(("channel", key)) => Ok((PlaceholderKind::Channel, key)),
        Some(("role", key)) => Ok((PlaceholderKind::Role, key)),
        Some(_) => Err(TemplateError::new(TemplateErrorKind::UnknownPlaceholder)),
    }
}

// ---------------------------------------------------------------------------
// The untrusted username
// ---------------------------------------------------------------------------

/// Characters that are removed from the username because they can hide text or
/// reorder it without being visible.
fn is_stripped_control(ch: char) -> bool {
    matches!(
        ch,
        '\u{00ad}' // soft hyphen
            | '\u{034f}' // combining grapheme joiner
            | '\u{200b}' // zero width space
            | '\u{200c}' // zero width non-joiner
            | '\u{200d}' // zero width joiner
            | '\u{200e}' // left-to-right mark
            | '\u{200f}' // right-to-left mark
            | '\u{202a}' // left-to-right embedding
            | '\u{202b}' // right-to-left embedding
            | '\u{202c}' // pop directional formatting
            | '\u{202d}' // left-to-right override
            | '\u{202e}' // right-to-left override
            | '\u{2060}' // word joiner
            | '\u{2061}' // function application
            | '\u{2062}' // invisible times
            | '\u{2063}' // invisible separator
            | '\u{2064}' // invisible plus
            | '\u{2066}' // left-to-right isolate
            | '\u{2067}' // right-to-left isolate
            | '\u{2068}' // first strong isolate
            | '\u{2069}' // pop directional isolate
            | '\u{feff}' // zero width no-break space
    )
}

/// Strips the invisible control characters from the username.
fn strip_controls(text: &str) -> String {
    text.chars().filter(|c| !is_stripped_control(*c)).collect()
}

/// Escapes the Discord markdown characters in the username.
///
/// The escaped form renders as the original characters, so the name stays readable
/// while it can no longer forge formatting.
fn escape_markdown(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if matches!(
            ch,
            '*' | '_' | '~' | '`' | '|' | '\\' | '>' | '#' | '-' | '=' | '{' | '}' | '(' | ')'
        ) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

/// Neutralises the mention syntax in the username.
///
/// Discord only fires a mass mention for the literal `@everyone` and `@here`, and a
/// user or role mention only for the literal `<@id>`, `<@!id>` or `<@&id>`. The
/// username therefore cannot keep the characters that build those tokens:
///
/// * every `@` becomes the full-width `＠`, so `@everyone` can never be formed;
/// * every `<` becomes `‹` and every `>` becomes `›`, so `<@id>` can never be formed.
///
/// The name stays readable: `＠everyone` still reads as "everyone" and the width of
/// the bracket is the only visible change.
fn neutralize_mentions(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '<' => out.push('\u{2039}'),
            '>' => out.push('\u{203a}'),
            '@' => out.push('\u{ff20}'),
            _ => out.push(ch),
        }
    }
    out
}

/// Sanitises the joining user's name.
///
/// The name is stripped of invisible control characters, escaped against markdown and
/// neutralised against mention syntax. The RESULT is then capped in length by
/// characters, so the whole rendered name - escapes included - never exceeds
/// [`MAX_USERNAME_CHARS`]. The result can no longer ping anyone and can no longer
/// forge formatting.
pub fn sanitize_username(username: &str) -> String {
    let stripped = strip_controls(username);
    let escaped = escape_markdown(&stripped);
    let neutralised = neutralize_mentions(&escaped);
    neutralised.chars().take(MAX_USERNAME_CHARS).collect()
}

/// Sanitises the server name.
///
/// The server name is caller-supplied free text, so it is treated like the username:
/// the invisible control characters are stripped, the mention syntax is neutralised
/// and the result is capped in length by characters. The markdown is NOT escaped:
/// the name is the server's own name, chosen by the server owner, and escaping it
/// would show stray backslashes in every welcome.
pub fn sanitize_server_name(server_name: &str) -> String {
    let stripped = strip_controls(server_name);
    let neutralised = neutralize_mentions(&stripped);
    neutralised.chars().take(MAX_SERVER_NAME_CHARS).collect()
}

// ---------------------------------------------------------------------------
// Clamping the rendered output
// ---------------------------------------------------------------------------

/// Shortens `text` to at most `max` characters.
///
/// The cut is on a character boundary, so a multi-byte character is never split. The
/// second value of the pair is true when something was cut.
fn clamp_chars(text: &str, max: usize) -> (String, bool) {
    let count = text.chars().count();
    if count <= max {
        return (text.to_string(), false);
    }
    (text.chars().take(max).collect(), true)
}

/// Shortens `text` by up to `count` characters, from the end.
///
/// Returns the number of characters that were removed.
fn shorten_end(text: &mut String, count: usize) -> usize {
    let len = text.chars().count();
    let take = count.min(len);
    if take == 0 {
        return 0;
    }
    let kept: String = text.chars().take(len - take).collect();
    *text = kept;
    take
}

/// Clamps the rendered embed so that Discord accepts it.
///
/// Every slot is first shortened to its own limit. If the embed is still longer than
/// the Discord total, the parts are shortened in this fixed order:
///
/// 1. the description,
/// 2. the footer text,
/// 3. the field values and the field names, from the LAST field backwards; a field
///    that can no longer keep both a name and a value is dropped,
/// 4. the author name,
/// 5. the title.
///
/// The title is shortened last so that the embed keeps its headline for as long as
/// possible. The function returns true when something was cut.
fn clamp_embed(embed: &mut RenderedEmbed) -> bool {
    let mut cut = false;

    if let Some(text) = embed.title.as_mut() {
        let (clamped, was_cut) = clamp_chars(text, MAX_TITLE_CHARS);
        *text = clamped;
        cut |= was_cut;
    }
    if let Some(text) = embed.description.as_mut() {
        let (clamped, was_cut) = clamp_chars(text, MAX_DESCRIPTION_CHARS);
        *text = clamped;
        cut |= was_cut;
    }
    for field in &mut embed.fields {
        let (name, name_cut) = clamp_chars(&field.name, MAX_FIELD_NAME_CHARS);
        field.name = name;
        cut |= name_cut;
        let (value, value_cut) = clamp_chars(&field.value, MAX_FIELD_VALUE_CHARS);
        field.value = value;
        cut |= value_cut;
    }
    if let Some(text) = embed.footer_text.as_mut() {
        let (clamped, was_cut) = clamp_chars(text, MAX_FOOTER_CHARS);
        *text = clamped;
        cut |= was_cut;
    }
    if let Some(text) = embed.author_name.as_mut() {
        let (clamped, was_cut) = clamp_chars(text, MAX_TITLE_CHARS);
        *text = clamped;
        cut |= was_cut;
    }

    cut |= clamp_embed_total(embed);
    cut
}

/// Shortens the embed until its total fits in [`MAX_EMBED_TOTAL_CHARS`].
///
/// The parts are shortened in the order [`clamp_embed`] documents. The function
/// returns true when something was cut.
///
/// The loop is bounded by the shape of the embed, not by its length: every pass
/// either shortens one slot or drops one field, so there is at most one pass per
/// slot plus one pass per field. The bound is a hard stop, so a future change that
/// made a pass fail to shorten anything would stop the loop instead of spinning. A
/// field that cannot keep both a name and a value is dropped rather than emptied,
/// because Discord rejects a field without a name or without a value.
fn clamp_embed_total(embed: &mut RenderedEmbed) -> bool {
    let mut cut = false;
    // One pass per slot (description, footer, author name, title) plus one pass per
    // field, and one spare pass for the round that finds the embed fitting.
    let mut passes_left = embed.fields.len() + 5;
    while passes_left > 0 {
        let total = embed_total_chars(embed);
        if total <= MAX_EMBED_TOTAL_CHARS {
            break;
        }
        let excess = total - MAX_EMBED_TOTAL_CHARS;

        // 1. The description.
        if let Some(text) = embed.description.as_mut() {
            let removed = shorten_end(text, excess);
            if removed > 0 {
                cut = true;
                if text.is_empty() {
                    embed.description = None;
                }
                passes_left -= 1;
                continue;
            }
        }

        // 2. The footer text.
        if let Some(text) = embed.footer_text.as_mut() {
            let removed = shorten_end(text, excess);
            if removed > 0 {
                cut = true;
                if text.is_empty() {
                    embed.footer_text = None;
                }
                passes_left -= 1;
                continue;
            }
        }

        // 3. The last field: its value first, then its name. A field that cannot keep
        //    both a name and a value is dropped.
        let last_field = embed
            .fields
            .last()
            .map(|field| (field.name.chars().count(), field.value.chars().count()));
        if let Some((name_len, value_len)) = last_field {
            // A field keeps at least one character in its name and in its value.
            let spare_value = value_len.saturating_sub(1);
            let spare_name = name_len.saturating_sub(1);
            if spare_value + spare_name >= excess {
                if let Some(field) = embed.fields.last_mut() {
                    let from_value = excess.min(spare_value);
                    shorten_end(&mut field.value, from_value);
                    shorten_end(&mut field.name, excess - from_value);
                }
                cut = true;
                passes_left -= 1;
                continue;
            }
            // The field cannot absorb the whole excess, so it is dropped whole. It is
            // not emptied in place: an empty field would still count towards the
            // total and would still be rejected by Discord.
            embed.fields.pop();
            cut = true;
            passes_left -= 1;
            continue;
        }

        // 4. The author name.
        if let Some(text) = embed.author_name.as_mut() {
            let removed = shorten_end(text, excess);
            if removed > 0 {
                cut = true;
                if text.is_empty() {
                    embed.author_name = None;
                }
                passes_left -= 1;
                continue;
            }
        }

        // 5. The title.
        if let Some(text) = embed.title.as_mut() {
            let removed = shorten_end(text, excess);
            if removed > 0 {
                cut = true;
                if text.is_empty() {
                    embed.title = None;
                }
                passes_left -= 1;
                continue;
            }
        }

        // Nothing is left to shorten: the embed is as small as it can get.
        break;
    }
    cut
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// The English ordinal of `number`.
///
/// The teens are irregular: 11th, 12th and 13th, and 111th, 112th and 113th.
pub fn english_ordinal(number: u64) -> String {
    let suffix = match number % 100 {
        11..=13 => "th",
        _ => match number % 10 {
            1 => "st",
            2 => "nd",
            3 => "rd",
            _ => "th",
        },
    };
    format!("{number}{suffix}")
}

/// The decision for one joining member.
///
/// A bot is skipped: the old YAGPDB command posted the welcome for bots as well, and
/// that is deliberately not copied. The decision needs nothing but the member, so the
/// function takes only the member.
pub fn welcome_policy(member: &MemberContext) -> SkipReason {
    if member.is_bot {
        SkipReason::Bot
    } else {
        SkipReason::None
    }
}

/// Renders the template for one joining member.
///
/// The member's name is sanitised before it is placed anywhere. `{user_mention}` is
/// the only way a user mention is produced; `{channel:KEY}` and `{role:KEY}` carry an
/// id that came from the caller's map, so no other raw id can appear in the output.
///
/// The rendered text is clamped to the Discord limits, because a placeholder expands
/// at render time: a template whose text is exactly at the limit can render past it.
/// When something was cut, [`RenderedWelcome::truncated`] is true, so the caller can
/// log it.
pub fn render_welcome(
    template: &WelcomeTemplate,
    member: &MemberContext,
    ids: &IdMap,
) -> RenderedWelcome {
    let context = RenderContext {
        user_mention: format!("<@{}>", member.user_id),
        username: sanitize_username(&member.username),
        server: sanitize_server_name(&member.server_name),
        member_count: member.member_count.to_string(),
        member_ordinal: english_ordinal(member.member_count),
        ids,
    };

    let mut truncated = false;

    let content = template
        .content
        .as_deref()
        .map(|text| {
            let rendered = render_text(text, &context);
            let (rendered, was_cut) = clamp_chars(&rendered, MAX_CONTENT_CHARS);
            truncated |= was_cut;
            rendered
        })
        .filter(|text| !text.is_empty());

    let mut embed = RenderedEmbed {
        title: template
            .embed
            .title
            .as_deref()
            .map(|text| render_text(text, &context)),
        description: template
            .embed
            .description
            .as_deref()
            .map(|text| render_text(text, &context)),
        fields: template
            .embed
            .fields
            .iter()
            .map(|field| RenderedField {
                name: render_text(&field.name, &context),
                value: render_text(&field.value, &context),
                inline: field.inline,
            })
            .collect(),
        footer_text: template
            .embed
            .footer_text
            .as_deref()
            .map(|text| render_text(text, &context)),
        author_name: template
            .embed
            .author_name
            .as_deref()
            .map(|text| render_text(text, &context)),
    };
    truncated |= clamp_embed(&mut embed);

    RenderedWelcome {
        content,
        embed,
        truncated,
    }
}

/// The values the placeholders resolve to for one member.
struct RenderContext<'a> {
    user_mention: String,
    username: String,
    server: String,
    member_count: String,
    member_ordinal: String,
    ids: &'a IdMap,
}

/// Substitutes every placeholder in `text`.
fn render_text(text: &str, context: &RenderContext<'_>) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        if bytes[index] != b'{' {
            let ch_len = utf8_len(bytes[index]);
            out.push_str(&text[index..index + ch_len]);
            index += ch_len;
            continue;
        }
        let close = match text[index..].find('}') {
            Some(offset) => index + offset,
            // The template was validated at load time, so this cannot happen for a
            // template that loaded. Keeping the brace is the conservative choice.
            None => {
                out.push_str(&text[index..]);
                break;
            }
        };
        let inner = &text[index + 1..close];
        out.push_str(&resolve_placeholder(inner, context));
        index = close + 1;
    }
    out
}

/// The number of bytes of the UTF-8 sequence that starts with this byte.
fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

/// Resolves one placeholder body to its text.
fn resolve_placeholder(inner: &str, context: &RenderContext<'_>) -> String {
    match inner {
        "user_mention" => context.user_mention.clone(),
        "username" => context.username.clone(),
        "server" => context.server.clone(),
        "member_count" => context.member_count.clone(),
        "member_ordinal" => context.member_ordinal.clone(),
        _ => match inner.split_once(':') {
            Some(("channel", key)) => match context.ids.channel(key) {
                Some(id) => format!("<#{id}>"),
                None => String::new(),
            },
            Some(("role", key)) => match context.ids.role(key) {
                Some(id) => format!("<@&{id}>"),
                None => String::new(),
            },
            _ => String::new(),
        },
    }
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
            "{\"name\": \"n\", \"value\": \"{channel:absent}\"}",
        ));
        let err = WelcomeTemplate::load(&src, &id_map()).expect_err("must be refused at load");
        assert_eq!(err.kind(), TemplateErrorKind::UnknownChannelKey);
    }

    /// An unknown role KEY is refused at load.
    #[test]
    fn unknown_role_key_is_refused_at_load() {
        let src = template_with(&embed_json(
            "{\"name\": \"n\", \"value\": \"{role:absent}\"}",
        ));
        let err = WelcomeTemplate::load(&src, &id_map()).expect_err("must be refused at load");
        assert_eq!(err.kind(), TemplateErrorKind::UnknownRoleKey);
    }

    /// A malformed KEY (empty or non-word characters) is refused at load.
    #[test]
    fn malformed_placeholder_key_is_refused_at_load() {
        for bad in [
            // An empty key.
            "{channel:}",
            "{role:}",
            // A key with a space, a dash, a dot or a slash in it.
            "{channel:with space}",
            "{channel:bad-key}",
            "{channel:bad.key}",
            "{channel:bad/key}",
            "{channel:../etc/passwd}",
            // A key that tries to be a path, a URI or a shell fragment.
            "{channel:file:///etc/passwd}",
            "{channel:$(id)}",
            "{channel:`id`}",
            // A key with a non-ASCII character.
            "{channel:rulesé}",
            // A key with a brace inside it.
            "{channel:a{b}",
            // A key that names the wrong kind of thing.
            "{user:1000000000000000003}",
            "{everyone}",
            "{channel:channel:rules}",
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
                        | TemplateErrorKind::UnknownPlaceholder
                        | TemplateErrorKind::UnclosedPlaceholder
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

    /// The rendered output contains no raw id that the caller did not supply: every
    /// id-shaped token in the output comes from the caller's map or is the joining
    /// user's own id.
    ///
    /// The server name and the member count are copied through unchanged, so the test
    /// uses a server name that carries digits and asserts that those digits are the
    /// server's own text, not an id the renderer chose.
    #[test]
    fn output_contains_no_foreign_raw_id() {
        let src = "{\"embed\": {\"description\": \"{channel:welcome} {role:member}\", \
                   \"title\": \"{server}\", \
                   \"fields\": [{\"name\": \"n\", \"value\": \"{channel:rules}\"}]}}";
        let t = WelcomeTemplate::load(src, &id_map()).expect("valid template loads");
        let mut m = member();
        m.server_name = "Server 1000000000000000999".to_string();
        let out = render_welcome(&t, &m, &id_map());
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
            // The server's own name is copied through: it is the server's text, not an
            // id the renderer picked.
            "1000000000000000999".to_string(),
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

        // A description of 4096 plus a title of 256 plus a field name of 1 plus a field
        // value of 1024 is 5377, plus a footer of 623 is exactly 6000: that must load.
        // (Discord counts the title, the description, every field name, every field
        // value and the footer text towards the total.)
        let exact = format!(
            "{{\"embed\": {{\"title\": \"{}\", \"description\": \"{}\", \
             \"footer\": {{\"text\": \"{}\"}}, \
             \"fields\": [{{\"name\": \"n\", \"value\": \"{}\"}}]}}}}",
            "t".repeat(MAX_TITLE_CHARS),
            "d".repeat(MAX_DESCRIPTION_CHARS),
            "f".repeat(623),
            "v".repeat(MAX_FIELD_VALUE_CHARS)
        );
        WelcomeTemplate::load(&exact, &id_map()).expect("exactly 6000 chars must load");

        let one_over = format!(
            "{{\"embed\": {{\"title\": \"{}\", \"description\": \"{}\", \
             \"footer\": {{\"text\": \"{}\"}}, \
             \"fields\": [{{\"name\": \"n\", \"value\": \"{}\"}}]}}}}",
            "t".repeat(MAX_TITLE_CHARS),
            "d".repeat(MAX_DESCRIPTION_CHARS),
            "f".repeat(624),
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

    /// A field name or value that is not a string is a MALFORMED field, not an empty
    /// one: the two are different mistakes and get different reason codes.
    #[test]
    fn a_non_string_field_name_or_value_is_refused_as_malformed() {
        let src = "{\"embed\": {\"fields\": [{\"name\": 7, \"value\": \"v\"}]}}";
        let err = WelcomeTemplate::load(src, &id_map()).expect_err("a number name must be refused");
        assert_eq!(err.kind(), TemplateErrorKind::MalformedFieldName);
        let src = "{\"embed\": {\"fields\": [{\"name\": \"n\", \"value\": 7}]}}";
        let err =
            WelcomeTemplate::load(src, &id_map()).expect_err("a number value must be refused");
        assert_eq!(err.kind(), TemplateErrorKind::MalformedFieldValue);
    }

    /// A key the render-time map does not have renders as an empty string: the
    /// renderer does not fail a join on a missing id. The load-time map is the one
    /// that decides which keys are valid.
    #[test]
    fn a_key_missing_from_the_render_map_renders_as_nothing() {
        let src = "{\"embed\": {\"description\": \"[{channel:rules}]\"}}";
        let t = WelcomeTemplate::load(src, &id_map()).expect("template must load");
        let empty_map = IdMap::default();
        let out = render_welcome(&t, &member(), &empty_map);
        assert_eq!(out.embed.description.as_deref(), Some("[]"));
    }

    // ---------------------------------------------------------------------------
    // Rule 4: bots are skipped
    // ---------------------------------------------------------------------------

    /// A bot member is skipped by the policy.
    #[test]
    fn bot_member_is_skipped() {
        let mut m = member();
        m.is_bot = true;
        let decision = welcome_policy(&m);
        assert_eq!(decision, SkipReason::Bot);
    }

    /// A human member is not skipped.
    #[test]
    fn human_member_is_not_skipped() {
        assert_eq!(welcome_policy(&member()), SkipReason::None);
    }

    /// A bot member is skipped even when the template would render fine.
    #[test]
    fn bot_is_skipped_before_rendering() {
        let src = "{\"embed\": {\"title\": \"{username}\"}}";
        let t = WelcomeTemplate::load(src, &id_map()).expect("valid template loads");
        let mut m = member();
        m.is_bot = true;
        m.username = "@everyone".to_string();
        // The template renders fine, so the skip is the policy's decision, not a
        // side effect of a broken template.
        let rendered = render_welcome(&t, &m, &id_map());
        assert!(rendered.embed.title.is_some());
        let decision = welcome_policy(&m);
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
            TemplateErrorKind::MalformedFieldName,
            TemplateErrorKind::MalformedFieldValue,
        ];
        let mut seen = BTreeMap::new();
        for kind in kinds {
            let code = kind.code();
            assert!(!code.is_empty(), "{kind:?} has an empty code");
            assert!(seen.insert(code, kind).is_none(), "duplicate code {code}");
        }
        assert!(seen.len() >= 21, "unexpected number of codes");
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

    // -----------------------------------------------------------------------
    // The RENDERED output is bounded, not only the template text
    // -----------------------------------------------------------------------

    /// The number of characters in the whole rendered embed.
    fn rendered_embed_total(embed: &RenderedEmbed) -> usize {
        let mut total = 0usize;
        if let Some(title) = &embed.title {
            total += title.chars().count();
        }
        if let Some(description) = &embed.description {
            total += description.chars().count();
        }
        for field in &embed.fields {
            total += field.name.chars().count();
            total += field.value.chars().count();
        }
        if let Some(footer) = &embed.footer_text {
            total += footer.chars().count();
        }
        if let Some(author) = &embed.author_name {
            total += author.chars().count();
        }
        total
    }

    /// The rendered description stays inside the Discord description limit even when the
    /// template text is exactly at the limit and a placeholder expands it.
    #[test]
    fn rendered_description_stays_within_the_limit() {
        // 4086 literal characters plus one `{username}` placeholder: the template text
        // is 4096 characters, exactly the limit, so the template loads. The rendered
        // text must still be at most 4096 characters.
        let template = format!(
            "{{\"embed\": {{\"description\": \"{}{{username}}\"}}}}",
            "d".repeat(4086)
        );
        let t = WelcomeTemplate::load(&template, &id_map()).expect("template must load");
        let mut m = member();
        m.username = "x".repeat(5000);
        let out = render_welcome(&t, &m, &id_map());
        let rendered = out.embed.description.expect("description must render");
        assert!(
            rendered.chars().count() <= 4096,
            "rendered description is {} chars, over the 4096 limit",
            rendered.chars().count()
        );
    }

    /// The rendered field value stays inside the Discord field-value limit even when the
    /// template text is exactly at the limit and a placeholder expands it.
    #[test]
    fn rendered_field_value_stays_within_the_limit() {
        let template = format!(
            "{{\"embed\": {{\"fields\": [{{\"name\": \"n\", \"value\": \"{}{{username}}\"}}]}}}}",
            "v".repeat(1014)
        );
        let t = WelcomeTemplate::load(&template, &id_map()).expect("template must load");
        let mut m = member();
        m.username = "x".repeat(5000);
        let out = render_welcome(&t, &m, &id_map());
        let rendered = &out.embed.fields[0].value;
        assert!(
            rendered.chars().count() <= 1024,
            "rendered field value is {} chars, over the 1024 limit",
            rendered.chars().count()
        );
    }

    /// The rendered message content stays inside the Discord content limit even when the
    /// template text is exactly at the limit and a placeholder expands it.
    #[test]
    fn rendered_content_stays_within_the_limit() {
        let template = format!(
            "{{\"content\": \"{}{{username}}\", \"embed\": {{\"title\": \"t\"}}}}",
            "c".repeat(1990)
        );
        let t = WelcomeTemplate::load(&template, &id_map()).expect("template must load");
        let mut m = member();
        m.username = "x".repeat(5000);
        let out = render_welcome(&t, &m, &id_map());
        let rendered = out.content.expect("content must render");
        assert!(
            rendered.chars().count() <= 2000,
            "rendered content is {} chars, over the 2000 limit",
            rendered.chars().count()
        );
    }

    /// The rendered embed total stays inside the Discord total limit even when the
    /// template text is exactly at the limit and the placeholders expand it.
    #[test]
    fn rendered_embed_total_stays_within_the_limit() {
        // Title 256 + description 4080 + one field name of 1 + one field value of 1014
        // plus two placeholders that expand: the template text is 6000 characters,
        // exactly the total limit, so the template loads.
        let template = format!(
            "{{\"embed\": {{\"title\": \"{}\", \"description\": \"{}{{username}}\", \
             \"fields\": [{{\"name\": \"n\", \"value\": \"{}{{username}}\"}}]}}}}",
            "t".repeat(256),
            "d".repeat(4080),
            "v".repeat(1014)
        );
        let t = WelcomeTemplate::load(&template, &id_map()).expect("template must load");
        let mut m = member();
        m.username = "x".repeat(5000);
        let out = render_welcome(&t, &m, &id_map());
        let total = rendered_embed_total(&out.embed);
        assert!(
            total <= 6000,
            "rendered embed total is {total} chars, over the 6000 limit"
        );
    }

    /// A template that repeats a placeholder expands far beyond the limit. The rendered
    /// result must still be inside the limit, whatever the expansion is.
    #[test]
    fn rendered_output_stays_within_the_limits_for_a_repeated_placeholder() {
        let template = format!(
            "{{\"embed\": {{\"description\": \"{}\"}}}}",
            "{username}".repeat(400)
        );
        let t = WelcomeTemplate::load(&template, &id_map()).expect("template must load");
        let mut m = member();
        m.username = "x".repeat(5000);
        let out = render_welcome(&t, &m, &id_map());
        let rendered = out.embed.description.expect("description must render");
        assert!(
            rendered.chars().count() <= 4096,
            "rendered description is {} chars, over the 4096 limit",
            rendered.chars().count()
        );
    }

    /// The server name is caller-supplied free text. It must not be able to push the
    /// rendered output past the limit, however long it is.
    #[test]
    fn rendered_output_stays_within_the_limits_for_a_long_server_name() {
        let template = "{\"embed\": {\"description\": \"{server}\"}}";
        let t = WelcomeTemplate::load(template, &id_map()).expect("template must load");
        let mut m = member();
        m.server_name = "S".repeat(5000);
        let out = render_welcome(&t, &m, &id_map());
        let rendered = out.embed.description.expect("description must render");
        assert!(
            rendered.chars().count() <= 4096,
            "rendered description is {} chars, over the 4096 limit",
            rendered.chars().count()
        );
    }

    /// Every rendered slot stays inside its own limit for the worst-case member: the
    /// longest name, the longest server name and the largest possible ids.
    #[test]
    fn every_rendered_slot_stays_within_its_limit_for_the_worst_case_member() {
        let template = "{\"content\": \"{user_mention} {username} {server} {member_count} \
                        {member_ordinal} {channel:rules} {role:member}\", \
                        \"embed\": {\"title\": \"{user_mention} {username}\", \
                        \"description\": \"{server} {username}\", \
                        \"fields\": [{\"name\": \"{username}\", \"value\": \"{user_mention}\"}], \
                        \"footer\": {\"text\": \"{username}\"}, \
                        \"author\": {\"name\": \"{user_mention}\"}}}";
        let t = WelcomeTemplate::load(template, &id_map()).expect("template must load");
        let mut m = member();
        m.user_id = u64::MAX;
        m.username = "x".repeat(5000);
        m.server_name = "S".repeat(5000);
        m.member_count = u64::MAX;
        let out = render_welcome(&t, &m, &id_map());

        let content = out.content.expect("content must render");
        assert!(
            content.chars().count() <= 2000,
            "content is {} chars",
            content.chars().count()
        );
        let title = out.embed.title.as_deref().expect("title must render");
        assert!(
            title.chars().count() <= 256,
            "title is {} chars",
            title.chars().count()
        );
        let description = out
            .embed
            .description
            .as_deref()
            .expect("description must render");
        assert!(
            description.chars().count() <= 4096,
            "description is {} chars",
            description.chars().count()
        );
        let footer = out
            .embed
            .footer_text
            .as_deref()
            .expect("footer must render");
        assert!(
            footer.chars().count() <= 2048,
            "footer is {} chars",
            footer.chars().count()
        );
        let author = out
            .embed
            .author_name
            .as_deref()
            .expect("author must render");
        assert!(
            author.chars().count() <= 256,
            "author is {} chars",
            author.chars().count()
        );
        for field in &out.embed.fields {
            assert!(
                field.name.chars().count() <= 256,
                "field name is {} chars",
                field.name.chars().count()
            );
            assert!(
                field.value.chars().count() <= 1024,
                "field value is {} chars",
                field.value.chars().count()
            );
        }
        let total = rendered_embed_total(&out.embed);
        assert!(total <= 6000, "embed total is {total} chars");
    }

    /// The sanitised username is the whole rendered username: the escaping must not
    /// push it past the cap.
    #[test]
    fn the_sanitised_username_is_at_most_the_cap() {
        let mut m = member();
        m.username = "*".repeat(5000);
        let sanitised = sanitize_username(&m.username);
        assert!(
            sanitised.chars().count() <= 64,
            "the sanitised username is {} chars",
            sanitised.chars().count()
        );
    }

    /// A truncation is visible to the caller: the rendered welcome says that something
    /// was cut, so the wiring card can log it.
    #[test]
    fn a_truncated_render_is_reported_to_the_caller() {
        let template = format!(
            "{{\"embed\": {{\"description\": \"{}{{username}}\"}}}}",
            "d".repeat(4086)
        );
        let t = WelcomeTemplate::load(&template, &id_map()).expect("template must load");
        let mut m = member();
        m.username = "x".repeat(5000);
        let out = render_welcome(&t, &m, &id_map());
        assert!(
            out.truncated,
            "the caller cannot see that the description was cut"
        );
    }

    /// A render that fits is NOT reported as truncated.
    #[test]
    fn a_complete_render_is_not_reported_as_truncated() {
        let template = "{\"embed\": {\"description\": \"Welcome {username}!\"}}";
        let t = WelcomeTemplate::load(template, &id_map()).expect("template must load");
        let out = render_welcome(&t, &member(), &id_map());
        assert!(!out.truncated, "a short welcome was reported as truncated");
    }

    /// The server name is capped: it is caller-supplied free text, so one placeholder
    /// must have a bounded expansion.
    #[test]
    fn the_server_name_is_capped() {
        let mut m = member();
        m.server_name = "S".repeat(5000);
        let sanitised = sanitize_server_name(&m.server_name);
        assert!(
            sanitised.chars().count() <= 100,
            "the sanitised server name is {} chars",
            sanitised.chars().count()
        );
    }

    /// The server name cannot carry a mass mention either.
    #[test]
    fn the_server_name_cannot_ping_everyone() {
        let sanitised = sanitize_server_name("@everyone");
        assert!(!sanitised.to_lowercase().contains("@everyone"));
        assert!(
            sanitised.contains("everyone"),
            "the name was lost: {sanitised}"
        );
    }

    /// The server name cannot hide text with invisible characters.
    #[test]
    fn the_server_name_control_characters_are_stripped() {
        let sanitised = sanitize_server_name("a\u{200b}b\u{202e}c");
        assert_eq!(sanitised, "abc");
    }

    /// The clamp drops the fields that cannot fit rather than leaving an empty field.
    /// Discord rejects a field without a name or without a value.
    #[test]
    fn the_clamp_never_leaves_an_empty_field() {
        // Twenty-five fields of 1024 characters each: the template text is 25 600
        // characters, so it is refused at load. A template that LOADS can still
        // overflow once the placeholders expand, so the clamp has to cope.
        let fields: Vec<String> = (0..25)
            .map(|i| {
                format!(
                    "{{\"name\": \"n{i}\", \"value\": \"{}\"}}",
                    "v".repeat(1024)
                )
            })
            .collect();
        let src = format!("{{\"embed\": {{\"fields\": [{}]}}}}", fields.join(","));
        let err = WelcomeTemplate::load(&src, &id_map()).expect_err("25 600 chars must be refused");
        assert_eq!(err.kind(), TemplateErrorKind::EmbedTotalTooLong);

        // A template that loads and then expands past the total: every field must
        // still have a name and a value afterwards.
        let src =
            "{\"embed\": {\"fields\": [{\"name\": \"{username}\", \"value\": \"{username}\"}]}}";
        let t = WelcomeTemplate::load(src, &id_map()).expect("template must load");
        let mut m = member();
        m.username = "x".repeat(5000);
        let out = render_welcome(&t, &m, &id_map());
        for field in &out.embed.fields {
            assert!(!field.name.is_empty(), "a field lost its name");
            assert!(!field.value.is_empty(), "a field lost its value");
        }
    }

    /// The clamp keeps the headline: the title is shortened last.
    #[test]
    fn the_clamp_keeps_the_title_as_long_as_it_can() {
        // Title 256 + description 4096 + a field value of 1024 + a field name of 1 is
        // 5377 characters: the template loads. The description then expands past its
        // own limit, and the embed must still fit the 6000 total, so the description
        // is what gets cut - not the title.
        let src = format!(
            "{{\"embed\": {{\"title\": \"{}\", \"description\": \"{}{{username}}\", \
             \"fields\": [{{\"name\": \"n\", \"value\": \"{}\"}}]}}}}",
            "t".repeat(256),
            "d".repeat(4080),
            "v".repeat(1024)
        );
        let t = WelcomeTemplate::load(&src, &id_map()).expect("template must load");
        let mut m = member();
        m.username = "x".repeat(5000);
        let out = render_welcome(&t, &m, &id_map());
        let title = out.embed.title.as_deref().expect("title must survive");
        assert_eq!(title.chars().count(), 256, "the title was cut");
        assert!(out.truncated, "the cut was not reported");
    }

    /// The clamp never splits a multi-byte character.
    #[test]
    fn the_clamp_cuts_on_a_character_boundary() {
        let src = format!(
            "{{\"embed\": {{\"description\": \"{}\"}}}}",
            "é".repeat(4096)
        );
        let t = WelcomeTemplate::load(&src, &id_map()).expect("template must load");
        let out = render_welcome(&t, &member(), &id_map());
        let description = out.embed.description.as_deref().expect("description");
        assert!(description.chars().all(|c| c == 'é'));
        assert!(description.chars().count() <= 4096);
    }

    /// The embed total is enforced even when every slot is already inside its OWN
    /// limit.
    ///
    /// The template text is 5988 characters, so it loads. The `{server}` placeholders
    /// expand it: the title, the description and the author name each grow past their
    /// own limit and are clamped back, and the four clamped slots together are 6168
    /// characters - more than the 6000 an embed allows. Only the total clamp can
    /// bring that back.
    #[test]
    fn the_embed_total_is_enforced_when_every_slot_is_legal() {
        let src = format!(
            "{{\"embed\": {{\"title\": \"{}{{server}}\", \"description\": \"{}{{server}}\", \
             \"footer\": {{\"text\": \"{}{{server}}\"}}, \
             \"author\": {{\"name\": \"{}{{server}}\"}}}}}}",
            "t".repeat(248),
            "d".repeat(4000),
            "f".repeat(1460),
            "a".repeat(248)
        );
        let t = WelcomeTemplate::load(&src, &id_map()).expect("template must load");
        let mut m = member();
        m.server_name = "S".repeat(5000);
        let out = render_welcome(&t, &m, &id_map());
        // Every slot is inside its own limit.
        assert!(
            out.embed
                .title
                .as_deref()
                .unwrap_or_default()
                .chars()
                .count()
                <= 256
        );
        assert!(
            out.embed
                .description
                .as_deref()
                .unwrap_or_default()
                .chars()
                .count()
                <= 4096
        );
        assert!(
            out.embed
                .footer_text
                .as_deref()
                .unwrap_or_default()
                .chars()
                .count()
                <= 2048
        );
        assert!(
            out.embed
                .author_name
                .as_deref()
                .unwrap_or_default()
                .chars()
                .count()
                <= 256
        );
        // The total is enforced even though every slot is legal on its own.
        let total = rendered_embed_total(&out.embed);
        assert!(
            total <= 6000,
            "the rendered embed total is {total} chars, over the 6000 limit"
        );
        assert!(out.truncated, "the cut was not reported");
    }

    /// The embed total is enforced when the FIELDS are what overflows.
    ///
    /// Twenty-four fields of 230 literal characters plus a `{server}` placeholder are
    /// 5712 characters of template text, so the template loads. The server name
    /// expands every field value, and the fields together are then over the total.
    #[test]
    fn the_embed_total_is_enforced_when_the_fields_overflow() {
        let src = format!(
            "{{\"embed\": {{\"fields\": [{}]}}}}",
            (0..24)
                .map(|i| format!(
                    "{{\"name\": \"n{i}\", \"value\": \"{}{{server}}\"}}",
                    "v".repeat(230)
                ))
                .collect::<Vec<String>>()
                .join(",")
        );
        let t = WelcomeTemplate::load(&src, &id_map()).expect("template must load");
        let mut m = member();
        m.server_name = "S".repeat(5000);
        let out = render_welcome(&t, &m, &id_map());
        // Every field value is inside its own limit, and every field still has a name
        // and a value.
        for field in &out.embed.fields {
            assert!(!field.name.is_empty(), "a field lost its name");
            assert!(!field.value.is_empty(), "a field lost its value");
            assert!(field.value.chars().count() <= 1024);
        }
        // The total is enforced even though every field is legal on its own.
        let total = rendered_embed_total(&out.embed);
        assert!(
            total <= 6000,
            "the rendered embed total is {total} chars, over the 6000 limit"
        );
        assert!(out.truncated, "the cut was not reported");
    }

    /// The total clamp keeps an embed that is exactly at the limit whole.
    #[test]
    fn the_total_clamp_keeps_an_embed_that_is_exactly_at_the_limit() {
        // Title 256 + description 4096 + footer 1648 is exactly 6000 characters.
        let src = format!(
            "{{\"embed\": {{\"title\": \"{}\", \"description\": \"{}\", \
             \"footer\": {{\"text\": \"{}\"}}}}}}",
            "t".repeat(256),
            "d".repeat(4096),
            "f".repeat(1648)
        );
        let t = WelcomeTemplate::load(&src, &id_map()).expect("template must load");
        let out = render_welcome(&t, &member(), &id_map());
        assert_eq!(rendered_embed_total(&out.embed), 6000);
        assert!(
            !out.truncated,
            "an embed at the limit was reported as truncated"
        );
        assert_eq!(
            out.embed.footer_text.as_deref().map(|t| t.chars().count()),
            Some(1648),
            "the footer was cut"
        );
    }

    /// The username cap is applied to the ESCAPED name, so the name keeps as many
    /// characters as the cap allows. Capping before the escaping would halve the
    /// visible name, because every special character costs a backslash.
    #[test]
    fn the_username_cap_counts_the_escaped_name() {
        // Thirty-two stars escape to sixty-four characters, which is exactly the cap.
        let sanitised = sanitize_username(&"*".repeat(5000));
        assert_eq!(sanitised.chars().count(), 64);
        assert_eq!(sanitised, "\\*".repeat(32));
        // A name of plain characters survives whole up to the cap.
        let plain = sanitize_username(&"a".repeat(5000));
        assert_eq!(plain.chars().count(), 64);
        assert!(plain.chars().all(|c| c == 'a'));
    }

    /// The clamp always finishes. A render that has to cut a lot must not spin: this
    /// test renders the largest embed the loader accepts, with the longest possible
    /// placeholders, and the test only ends if the renderer returns.
    #[test]
    fn the_clamp_always_terminates() {
        // Twenty-five fields of 1024 characters each is refused at load, so use the
        // largest embed that loads: twenty-four fields of 238 characters, which the
        // placeholders then expand well past the total.
        let src = format!(
            "{{\"embed\": {{\"fields\": [{}]}}}}",
            (0..24)
                .map(|i| format!(
                    "{{\"name\": \"n{i}\", \"value\": \"{}{{server}}\"}}",
                    "v".repeat(230)
                ))
                .collect::<Vec<String>>()
                .join(",")
        );
        let t = WelcomeTemplate::load(&src, &id_map()).expect("template must load");
        let mut m = member();
        m.server_name = "S".repeat(5000);
        let out = render_welcome(&t, &m, &id_map());
        // The render returned, so the clamp terminated. The embed still fits.
        assert!(rendered_embed_total(&out.embed) <= 6000);
    }

    /// The rendered field NAME stays inside the Discord field-name limit, even when
    /// the template text is exactly at the limit and a placeholder expands it.
    #[test]
    fn the_rendered_field_name_stays_within_the_limit() {
        let src = field_template(246, "v");
        let t = WelcomeTemplate::load(&src, &id_map()).expect("template must load");
        let mut m = member();
        m.username = "x".repeat(5000);
        let out = render_welcome(&t, &m, &id_map());
        let name = &out.embed.fields[0].name;
        assert!(
            name.chars().count() <= 256,
            "rendered field name is {} chars, over the 256 limit",
            name.chars().count()
        );
        assert!(!name.is_empty(), "the field lost its name");
    }

    /// A template with one field whose name is `name_len` literal characters plus a
    /// `{username}` placeholder.
    fn field_template(name_len: usize, value: &str) -> String {
        format!(
            "{{\"embed\": {{\"fields\": [{{\"name\": \"{}{{username}}\", \"value\": \"{}\"}}]}}}}",
            "n".repeat(name_len),
            value
        )
    }

    /// The rendered footer stays inside the Discord footer limit, even when the
    /// template text is exactly at the limit and a placeholder expands it.
    #[test]
    fn the_rendered_footer_stays_within_the_limit() {
        let src = format!(
            "{{\"embed\": {{\"footer\": {{\"text\": \"{}{{username}}\"}}}}}}",
            "f".repeat(2038)
        );
        let t = WelcomeTemplate::load(&src, &id_map()).expect("template must load");
        let mut m = member();
        m.username = "x".repeat(5000);
        let out = render_welcome(&t, &m, &id_map());
        let footer = out
            .embed
            .footer_text
            .as_deref()
            .expect("footer must render");
        assert!(
            footer.chars().count() <= 2048,
            "rendered footer is {} chars, over the 2048 limit",
            footer.chars().count()
        );
    }

    /// The rendered author name stays inside the Discord author-name limit, even when
    /// the template text is exactly at the limit and a placeholder expands it.
    #[test]
    fn the_rendered_author_name_stays_within_the_limit() {
        let src = format!(
            "{{\"embed\": {{\"author\": {{\"name\": \"{}{{username}}\"}}}}}}",
            "a".repeat(246)
        );
        let t = WelcomeTemplate::load(&src, &id_map()).expect("template must load");
        let mut m = member();
        m.username = "x".repeat(5000);
        let out = render_welcome(&t, &m, &id_map());
        let author = out
            .embed
            .author_name
            .as_deref()
            .expect("author must render");
        assert!(
            author.chars().count() <= 256,
            "rendered author name is {} chars, over the 256 limit",
            author.chars().count()
        );
    }

    /// The clamp counts CHARACTERS, not bytes: a multi-byte name that has to be cut
    /// keeps whole characters and never leaves half a code point behind.
    #[test]
    fn the_clamp_counts_characters_not_bytes() {
        // The field name is 246 multi-byte characters plus a placeholder, so the
        // rendered name is longer than the 256 character limit and has to be cut.
        let src = format!(
            "{{\"embed\": {{\"fields\": [{{\"name\": \"{}{{username}}\", \"value\": \"v\"}}]}}}}",
            "é".repeat(246)
        );
        let t = WelcomeTemplate::load(&src, &id_map()).expect("template must load");
        let mut m = member();
        m.username = "x".repeat(5000);
        let out = render_welcome(&t, &m, &id_map());
        let name = &out.embed.fields[0].name;
        // The cut keeps the whole limit in characters: a byte-based cut would have
        // kept far fewer characters, because every one of them is two bytes.
        assert_eq!(
            name.chars().count(),
            256,
            "the cut kept {} characters instead of 256",
            name.chars().count()
        );
        // Every character is whole: the first 246 are the multi-byte letter and the
        // rest is the username. A byte-based cut would have split a code point and
        // left a replacement character behind.
        assert!(
            name.chars().take(246).all(|c| c == 'é'),
            "the name was cut mid-character"
        );
        assert!(
            name.chars().skip(246).all(|c| c == 'x'),
            "the name was cut mid-character"
        );
    }

    /// The same cut on a plain ASCII name keeps the whole limit too.
    #[test]
    fn the_clamp_keeps_the_whole_limit_in_characters() {
        let src = format!(
            "{{\"embed\": {{\"fields\": [{{\"name\": \"{}{{username}}\", \"value\": \"v\"}}]}}}}",
            "n".repeat(246)
        );
        let t = WelcomeTemplate::load(&src, &id_map()).expect("template must load");
        let mut m = member();
        m.username = "x".repeat(5000);
        let out = render_welcome(&t, &m, &id_map());
        let name = &out.embed.fields[0].name;
        assert_eq!(
            name.chars().count(),
            256,
            "the cut kept {} characters instead of 256",
            name.chars().count()
        );
    }
}
