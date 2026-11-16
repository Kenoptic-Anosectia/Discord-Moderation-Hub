pub mod controller;
pub mod policy;
pub mod repository;
pub mod runtime;

// Conservative A06 fallback. No cache lookup, channel fetch, topic construction,
// embed copy or general log is allowed to race channel attachment while enabled.
pub fn allow_general_events(lockdown: bool, event: &serenity::all::FullEvent) -> bool {
    !lockdown
        || matches!(
            event,
            serenity::all::FullEvent::InteractionCreate { .. }
                | serenity::all::FullEvent::ReactionAdd { .. }
                | serenity::all::FullEvent::ReactionRemove { .. }
        )
}

#[cfg(test)]
mod tests {
    #[test]
    fn actual_event_routing_fails_closed_without_channel_or_database_metadata() {
        use serenity::all::*;
        let events = [
            FullEvent::Message {
                new_message: Message::default(),
            },
            FullEvent::MessageUpdate {
                old_if_available: None,
                new: None,
                event: serde_json::from_value(serde_json::json!({"id":"2","channel_id":"1"}))
                    .unwrap(),
            },
            FullEvent::MessageDelete {
                channel_id: ChannelId::new(1),
                deleted_message_id: MessageId::new(2),
                guild_id: None,
            },
            FullEvent::MessageDeleteBulk {
                channel_id: ChannelId::new(1),
                multiple_deleted_messages_ids: vec![MessageId::new(2)],
                guild_id: None,
            },
            FullEvent::ChannelCreate {
                channel: GuildChannel::default(),
            },
            FullEvent::ChannelUpdate {
                old: None,
                new: GuildChannel::default(),
            },
        ];
        for event in events {
            assert!(!super::allow_general_events(true, &event));
            assert!(super::allow_general_events(false, &event));
        }
        const {
            assert!(
                !super::runtime::OPENING_APPROVED,
                "A07 must explicitly release the gate, with privacy/recovery evidence"
            )
        };
    }
}
