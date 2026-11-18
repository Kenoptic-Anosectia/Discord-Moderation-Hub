//! Pure string-formatting and emoji-key normalization helpers for Discord embeds and reaction roles.
//!
//! Responsible for bounding text to Discord embed limits, neutralizing triple-backtick code fences
//! in logged message bodies, and mapping custom/Unicode emojis to a shared `reaction_roles` key.
//! Deliberately performs no I/O, database queries, Discord API calls, or channel privacy checks
//! (see `docs/verification/TEXT-HELPER-EXTRACTION.md`).
//! Trust boundary: inputs are untrusted Discord message text, reaction payloads, and command strings;
//! callers must enforce private-channel gating before formatting message text into log embeds.

use poise::serenity_prelude::model::channel::ReactionType;

/// Truncates `input` to at most `max_len` Unicode scalar values (`char`), appending `"..."`
/// in place of the last three scalars when truncation occurs.
///
/// Caller must pass `max_len >= 3` if the returned string (including `"..."`) must stay within
/// `max_len` scalars. Infallible; for `max_len < 3` on an over-limit input, returns `"..."`.
pub(crate) fn truncate_for_embed(input: &str, max_len: usize) -> String {
    // Count Unicode scalar values (`char`) rather than UTF-8 bytes so multi-byte codepoints
    // are never split, and use saturating_sub(3) so limits below 3 cannot underflow.
    if input.chars().count() <= max_len {
        input.to_string()
    } else {
        input
            .chars()
            .take(max_len.saturating_sub(3))
            .collect::<String>()
            + "..."
    }
}

/// Wraps `content` in a fenced Discord markdown code block capped at 980 Unicode scalars,
/// or substitutes `"*No text content*"` when `content` is empty or whitespace-only.
///
/// Caller must guarantee the source channel is eligible for logging before passing message text.
/// Infallible; neutralizes inner triple-backtick fences so untrusted text cannot break out.
pub(crate) fn code_block_or_placeholder(content: &str) -> String {
    let cleaned = if content.trim().is_empty() {
        "*No text content*".to_string()
    } else {
        // Rewrite triple backticks to single quotes so user content cannot close the outer fence.
        content.replace("```", "'''")
    };

    // Cap body at 980 scalars so the surrounding fence fits inside Discord's 1024-char field limit.
    format!("```{}\n```", truncate_for_embed(&cleaned, 980))
}

/// Converts a gateway `ReactionType` into the canonical lookup key stored in `reaction_roles`
/// (`<unicode>` for standard emojis, `<name>:<id>` for custom guild emojis).
///
/// Caller needs no precondition. Infallible; falls back to `"emoji:<id>"` if Discord omits
/// the custom emoji name, or `"unknown"` for unrecognized variants so lookups fail closed.
pub(crate) fn normalize_reaction_type(emoji: &ReactionType) -> String {
    match emoji {
        ReactionType::Unicode(value) => value.clone(),
        ReactionType::Custom { id, name, .. } => {
            // Discord reaction payloads may omit `name`; fall back to a fixed prefix plus numeric id.
            format!(
                "{}:{}",
                name.clone().unwrap_or_else(|| "emoji".to_string()),
                id.get()
            )
        }
        // ReactionType is non-exhaustive; map any future variant to a key that matches no role.
        _ => "unknown".to_string(),
    }
}

/// Normalizes an operator-supplied emoji argument from `/reactionrole` commands into the
/// same key space produced by [`normalize_reaction_type`].
///
/// Caller is responsible for validating command permissions and target message/role IDs.
/// Infallible; extracts `name:id` from `<:name:id>` or `<a:name:id>` and otherwise returns
/// the trimmed input verbatim.
pub(crate) fn parse_emoji_input(input: &str) -> String {
    let trimmed = input.trim();

    // Both static (`<:name:id>`) and animated (`<a:name:id>`) custom emoji mentions have three
    // colon-separated fields once angle brackets are stripped; collapse both to `name:id`.
    if trimmed.starts_with("<:") || trimmed.starts_with("<a:") {
        let inner = trimmed.trim_start_matches('<').trim_end_matches('>');
        let parts: Vec<&str> = inner.split(':').collect();

        if parts.len() >= 3 {
            return format!("{}:{}", parts[1], parts[2]);
        }
    }

    trimmed.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use poise::serenity_prelude as serenity;

    /// Proves `truncate_for_embed` leaves strings at or below `max_len` untouched and replaces excess chars with `"..."`.
    /// Would fail if the boundary check used `<` instead of `<=` or miscomputed the kept prefix length.
    #[test]
    fn embed_truncation_ascii_boundaries() {
        for (input, limit, expected) in [
            ("", 0, ""),
            ("abc", 3, "abc"),
            ("abc", 4, "abc"),
            ("abcd", 3, "..."),
            ("abcdef", 5, "ab..."),
            ("abcdef", 6, "abcdef"),
        ] {
            assert_eq!(
                truncate_for_embed(input, limit),
                expected,
                "{input:?}, {limit}"
            );
        }
    }

    /// Proves `truncate_for_embed` counts Unicode scalar values (`char`) rather than UTF-8 bytes or grapheme clusters.
    /// Would fail if truncation sliced by byte offset or miscounted multi-byte/combining sequences.
    #[test]
    fn embed_truncation_unicode_scalar_boundaries() {
        // The existing helper counts Unicode scalar values, not UTF-8 bytes,
        // UTF-16 code units, or grapheme clusters.
        for (input, limit, expected) in [
            ("é界🦀", 3, "é界🦀"),
            ("é界🦀ab", 4, "é..."),
            ("🦀界éabc", 5, "🦀界..."),
            ("e\u{301}界🦀abc", 5, "e\u{301}..."),
            ("👩\u{200d}💻abc", 5, "👩\u{200d}..."),
        ] {
            let result = truncate_for_embed(input, limit);
            assert_eq!(result, expected, "{input:?}, {limit}");
            assert!(result.chars().count() <= limit);
        }
    }

    /// Proves `truncate_for_embed` returns `"..."` without underflow panic when `max_len <= 3` on over-limit input.
    /// Would fail if `max_len - 3` underflowed or if small limits truncated the ellipsis itself.
    #[test]
    fn embed_truncation_preserves_existing_small_limit_ellipsis() {
        // Characterize, rather than silently change, the existing behavior:
        // a truncated result always includes all three dots even below limit 3.
        for input in ["abcd", "é界🦀a"] {
            for limit in 0..=3 {
                assert_eq!(
                    truncate_for_embed(input, limit),
                    "...",
                    "{input:?}, {limit}"
                );
            }
        }
    }

    /// Proves `parse_emoji_input` trims whitespace, normalizes `<:name:id>` and `<a:name:id>` to `name:id`, and preserves other inputs.
    /// Would fail if animated custom emoji tags, Unicode whitespace, or incomplete `<:incomplete>` strings were mishandled.
    #[test]
    fn emoji_input_normalization_cases() {
        for (input, expected) in [
            ("", ""),
            (" \t\n", ""),
            (" 🦀 \n", "🦀"),
            ("\u{2003}👍🏽\u{2003}", "👍🏽"),
            ("<:wave:123>", "wave:123"),
            (" <a:dance:456> ", "dance:456"),
            ("wave:123", "wave:123"),
            ("<:incomplete>", "<:incomplete>"),
            ("plain text", "plain text"),
        ] {
            assert_eq!(parse_emoji_input(input), expected, "{input:?}");
        }
    }

    /// Proves `normalize_reaction_type` maps Unicode emojis verbatim and custom emojis to `name:id` (or `emoji:id` when unnamed).
    /// Would fail if the animated flag altered the key or if `name: None` panicked or omitted the fallback prefix.
    #[test]
    fn reaction_type_normalization_cases() {
        for (reaction, expected) in [
            (ReactionType::Unicode("👍🏽".into()), "👍🏽"),
            (
                ReactionType::Custom {
                    animated: false,
                    id: serenity::EmojiId::new(123),
                    name: Some("wave".into()),
                },
                "wave:123",
            ),
            (
                ReactionType::Custom {
                    animated: true,
                    id: serenity::EmojiId::new(456),
                    name: Some("dance".into()),
                },
                "dance:456",
            ),
            (
                ReactionType::Custom {
                    animated: false,
                    id: serenity::EmojiId::new(789),
                    name: None,
                },
                "emoji:789",
            ),
        ] {
            assert_eq!(normalize_reaction_type(&reaction), expected, "{reaction:?}");
        }
    }

    /// Proves `parse_emoji_input` and `normalize_reaction_type` produce identical keys for matching custom and Unicode emojis.
    /// Would fail if command-argument normalization and gateway-event normalization disagreed on key format.
    #[test]
    fn emoji_input_matches_reaction_lookup_keys() {
        for (input, reaction) in [
            (
                " <:wave:123> ",
                ReactionType::Custom {
                    animated: false,
                    id: serenity::EmojiId::new(123),
                    name: Some("wave".into()),
                },
            ),
            (
                "<a:dance:456>",
                ReactionType::Custom {
                    animated: true,
                    id: serenity::EmojiId::new(456),
                    name: Some("dance".into()),
                },
            ),
            (" 🦀 ", ReactionType::Unicode("🦀".into())),
        ] {
            assert_eq!(parse_emoji_input(input), normalize_reaction_type(&reaction));
        }
    }

    /// Proves `code_block_or_placeholder` renders a placeholder for blank text, replaces inner `"```"` with `"'''"`, and truncates at 980 scalars.
    /// Would fail if blank strings produced empty code blocks, inner fences escaped the block, or long Unicode strings exceeded 980 scalars.
    #[test]
    fn code_block_placeholder_and_fence_sanitization() {
        for (input, expected) in [
            ("", "```*No text content*\n```"),
            (" \t\n", "```*No text content*\n```"),
            ("hello 🦀", "```hello 🦀\n```"),
            ("a```b", "```a'''b\n```"),
        ] {
            assert_eq!(code_block_or_placeholder(input), expected, "{input:?}");
        }
        let long_unicode = "🦀".repeat(981);
        assert_eq!(
            code_block_or_placeholder(&long_unicode),
            format!("```{}...\n```", "🦀".repeat(977))
        );
    }
}
