use poise::serenity_prelude::model::channel::ReactionType;

pub(crate) fn truncate_for_embed(input: &str, max_len: usize) -> String {
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

pub(crate) fn code_block_or_placeholder(content: &str) -> String {
    let cleaned = if content.trim().is_empty() {
        "*No text content*".to_string()
    } else {
        content.replace("```", "'''")
    };

    format!("```{}\n```", truncate_for_embed(&cleaned, 980))
}

pub(crate) fn normalize_reaction_type(emoji: &ReactionType) -> String {
    match emoji {
        ReactionType::Unicode(value) => value.clone(),
        ReactionType::Custom { id, name, .. } => {
            format!(
                "{}:{}",
                name.clone().unwrap_or_else(|| "emoji".to_string()),
                id.get()
            )
        }
        _ => "unknown".to_string(),
    }
}

pub(crate) fn parse_emoji_input(input: &str) -> String {
    let trimmed = input.trim();

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
