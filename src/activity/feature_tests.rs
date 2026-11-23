use super::{serenity, ActivityFeature, Feature, Ports, CHECKPOINT_SECS};
use crate::activity::{config, policy::KEY_MODE};
use std::sync::{
    atomic::{AtomicI64, Ordering},
    Arc, Mutex,
};
use tokio_rusqlite::Connection;

#[derive(Clone, Default)]
struct TestPorts {
    clock: Arc<AtomicI64>,
    codes: Arc<Mutex<Vec<&'static str>>>,
}

impl Ports for TestPorts {
    fn now(&self) -> i64 {
        self.clock.load(Ordering::Relaxed)
    }

    fn channel_allowed(&self, _: u64, channel: u64) -> bool {
        channel != 6
    }

    fn report(&self, code: &'static str) {
        self.codes.lock().unwrap().push(code);
    }
}

fn feature(mode: &str, ports: &TestPorts) -> ActivityFeature {
    let settings = config::parse(|key| (key == KEY_MODE).then(|| mode.into()), Some(1)).unwrap();
    ActivityFeature::new(settings, ports.clone())
}

fn message(guild: u64, channel: u64) -> serenity::FullEvent {
    let mut m = serenity::Message::default();
    m.guild_id = Some(serenity::GuildId::new(guild));
    m.channel_id = serenity::ChannelId::new(channel);
    m.author.id = serenity::UserId::new(2);
    m.timestamp = serenity::Timestamp::from_unix_timestamp(0).unwrap();
    serenity::FullEvent::Message { new_message: m }
}

fn voice(
    guild: u64,
    user: u64,
    channel: u64,
    join: bool,
    bot: Option<bool>,
) -> serenity::FullEvent {
    let state = |connected| {
        serde_json::from_value(serde_json::json!({
            "guild_id": guild.to_string(), "user_id": user.to_string(),
            "channel_id": if connected { Some(channel.to_string()) } else { None },
            "session_id": "synthetic", "deaf": false, "mute": false,
            "self_deaf": false, "self_mute": false, "self_video": false, "suppress": false,
            "member": bot.map(|bot| serde_json::json!({
                "user": {"id": user.to_string(), "username": "synthetic", "bot": bot},
                "roles": [], "deaf": false, "mute": false, "flags": 0,
                "joined_at": "2020-01-01T00:00:00Z", "guild_id": guild.to_string()
            }))
        }))
        .unwrap()
    };
    serenity::FullEvent::VoiceStateUpdate {
        old: (!join).then(|| state(true)),
        new: state(join),
    }
}

async fn dispatch(feature: &ActivityFeature, event: serenity::FullEvent, routed: u64) {
    Feature::<(), ()>::handle(feature, &(), &event, &(), Some(routed), None).await;
}

async fn totals(db: &Connection) -> (i64, i64) {
    db.call(|conn| {
        conn.query_row(
            "SELECT coalesce(sum(messages),0), coalesce(sum(voice_secs),0) FROM activity_daily",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn ac2_feature_filters_private_channels_and_foreign_guilds() {
    let ports = TestPorts::default();
    let feature = feature("live", &ports);
    let db = Connection::open_in_memory().await.unwrap();
    feature.tick(Some(&db)).await.unwrap();
    // Home/public control, home/private, foreign payload, and wrong routing metadata.
    for (guild, channel, routed) in [(1, 3, 1), (1, 6, 1), (9, 3, 1), (1, 3, 9)] {
        dispatch(&feature, message(guild, channel), routed).await;
    }
    for join in [true, false] {
        ports
            .clock
            .store(if join { 1 } else { 11 }, Ordering::Relaxed);
        for (guild, user, channel, routed) in
            [(1, 2, 3, 1), (1, 3, 6, 1), (9, 4, 3, 1), (1, 5, 3, 9)]
        {
            dispatch(
                &feature,
                voice(guild, user, channel, join, Some(false)),
                routed,
            )
            .await;
        }
    }
    feature.shutdown(Some(&db)).await.unwrap();
    assert_eq!(totals(&db).await, (1, 10));
}

#[tokio::test]
async fn ac2_feature_off_and_shadow_never_migrate_database() {
    for (mode, expected_codes) in [
        ("off", &[][..]),
        ("shadow", &["activity_shadow_message"][..]),
    ] {
        let ports = TestPorts::default();
        let feature = feature(mode, &ports);
        let db = Connection::open_in_memory().await.unwrap();
        assert!(!Feature::<(), ()>::needs_core_db(&feature));
        feature.tick(Some(&db)).await.unwrap();
        dispatch(&feature, message(1, 3), 1).await;
        feature.shutdown(Some(&db)).await.unwrap();
        let tables: i64 = db
            .call(|conn| conn.query_row("SELECT count(*) FROM sqlite_schema", [], |row| row.get(0)))
            .await
            .unwrap();
        assert_eq!(tables, 0, "{mode}");
        assert_eq!(&*ports.codes.lock().unwrap(), expected_codes, "{mode}");
    }
}

#[tokio::test]
async fn ac2_feature_ignores_bot_voice() {
    assert_ignored_voice(Some(true), &[]).await;
}

#[tokio::test]
async fn ac2_feature_ignores_voice_without_member_and_reports_code() {
    assert_ignored_voice(None, &["activity_voice_member_missing"]).await;
}

async fn assert_ignored_voice(bot: Option<bool>, codes: &[&str]) {
    let ports = TestPorts::default();
    let feature = feature("live", &ports);
    let db = Connection::open_in_memory().await.unwrap();
    feature.tick(Some(&db)).await.unwrap();
    dispatch(&feature, voice(1, 2, 3, true, bot), 1).await;
    ports.clock.store(30, Ordering::Relaxed);
    feature.tick(Some(&db)).await.unwrap();
    let open: i64 = db
        .call(|conn| conn.query_row("SELECT count(*) FROM voice_sessions", [], |row| row.get(0)))
        .await
        .unwrap();
    assert_eq!(open, 0);
    feature.shutdown(Some(&db)).await.unwrap();
    assert_eq!(totals(&db).await, (0, 0));
    assert_eq!(&*ports.codes.lock().unwrap(), codes);
}

#[tokio::test]
async fn ac2_feature_memberless_leave_credits_ten_seconds_at_shutdown() {
    assert_memberless_close(false, false).await;
}

#[tokio::test]
async fn ac2_feature_memberless_leave_credits_ten_seconds_after_restart() {
    assert_memberless_close(true, false).await;
}

#[tokio::test]
async fn ac2_feature_memberless_move_closes_without_opening_destination() {
    assert_memberless_close(false, true).await;
}

async fn assert_memberless_close(restart: bool, move_channel: bool) {
    let ports = TestPorts::default();
    let activity = feature("live", &ports);
    let db = Connection::open_in_memory().await.unwrap();
    activity.tick(Some(&db)).await.unwrap();
    dispatch(&activity, voice(1, 2, 3, true, Some(false)), 1).await;
    ports.clock.store(10, Ordering::Relaxed);
    let serenity::FullEvent::VoiceStateUpdate { new, .. } = voice(1, 2, 4, move_channel, None)
    else {
        unreachable!();
    };
    // The gateway cache need not provide the previous state either.
    dispatch(
        &activity,
        serenity::FullEvent::VoiceStateUpdate { old: None, new },
        1,
    )
    .await;
    assert_eq!(
        &*ports.codes.lock().unwrap(),
        &["activity_voice_member_missing"]
    );
    for tick in 1..=3600 / CHECKPOINT_SECS {
        ports.clock.store(tick * CHECKPOINT_SECS, Ordering::Relaxed);
        activity.tick(Some(&db)).await.unwrap();
    }
    if restart {
        // Lose the runtime without flushing it; the replacement sees only SQLite state.
        drop(activity);
        ports.clock.store(7200, Ordering::Relaxed);
        feature("live", &ports).tick(Some(&db)).await.unwrap();
    } else {
        activity.shutdown(Some(&db)).await.unwrap();
    }
    assert_eq!(totals(&db).await, (0, 10));
}

#[tokio::test]
async fn ac2_feature_bot_join_then_memberless_leave_credits_nothing() {
    let ports = TestPorts::default();
    let activity = feature("live", &ports);
    let db = Connection::open_in_memory().await.unwrap();
    activity.tick(Some(&db)).await.unwrap();
    dispatch(&activity, voice(1, 2, 3, true, Some(true)), 1).await;
    ports.clock.store(10, Ordering::Relaxed);
    dispatch(&activity, voice(1, 2, 3, false, None), 1).await;
    ports.clock.store(30, Ordering::Relaxed);
    activity.tick(Some(&db)).await.unwrap();
    activity.shutdown(Some(&db)).await.unwrap();
    assert_eq!(totals(&db).await, (0, 0));
    let rows: i64 = db
        .call(|conn| {
            conn.query_row(
                "SELECT (SELECT count(*) FROM activity_daily) + (SELECT count(*) FROM voice_sessions)",
                [],
                |row| row.get(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(rows, 0);
}
