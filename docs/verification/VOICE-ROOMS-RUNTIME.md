# VOICE-ROOMS-RUNTIME - user-owned voice rooms: runtime, table, config (VR2)
Card VR2: `src/voice_rooms/{config,repository,runtime}.rs` on top of the approved VR1 policy
(`policy.rs`, unchanged) and the KJ2 queue. Off by default, tested offline, NOT called from `main.rs`.
- **Parts.** `config.rs` parses the `VC_*` keys with the K06 helpers. `repository.rs` owns
  `voice_rooms(channel, guild, owner, created_at, locked, user_limit)` (own migration, no `storage.rs`
  edit). `runtime.rs` holds `VoiceRoomsFeature` (`Feature` hook), the traits `RoomPorts` and
  `StaffAlerter`, and `SerenityRoomPorts` (compiles; never run against Discord).
- **Scope.** Home guild only: an event of another guild (or none) is ignored before anything is read,
  written, sent or alerted. Mode on with the home guild, category or hub unset: `NotReady`, nothing
  happens, `enabled()` is false. Bot, own-id, unknown-member and user-0 events are ignored.
- **Join.** A hub join runs `decide_join` (global cap = `VC_MAX_ROOMS`, per-member cap and name are the
  VR1 defaults, cooldown). A denial is a stable code with no Discord call and no alert. Otherwise: read
  the category overwrites, create (category overwrites + the VR1 owner overwrite), write the row, THEN
  move the member. Creations are serialized, so two joins cannot both pass a cap.
- **Failures.** Create fails: member stays in the hub, no row. Move fails: the empty channel and its
  row are deleted; if that delete fails the row stays and a cleanup job is owed. Row write fails: the
  channel is deleted, nobody is moved. A failed attempt also starts the member's cooldown.
- **Cleanup.** A tracked room that becomes empty gets a `vc_cleanup` job due at empty + grace (restart
  policy `RunAgain`). The job is a wake-up: it re-reads the row and the live occupancy. Occupied
  (a rejoin) = nothing; no row, other guild = nothing; fired before the grace of the latest empty =
  re-armed; channel gone = row dropped; else delete the channel, then the row. A failed delete is
  retried with a growing delay (15 s doubling to 900 s), one alert, and after 8 tries it gives up
  with an alert and keeps the row for the next reconcile. A finished job under a dedupe key never
  swallows a later empty (key variants).
- **Reconcile** (`reconcile`, meant for `CacheReady`): walks the rows of the home guild only. Channel
  missing = row dropped; empty = channel then row deleted; occupied = kept; unknown or delete failed =
  cleanup job. A channel with no row is never listed, deleted or modified. Repeating it is harmless.
- **Alerts** are `vc_*` codes (`alert_text`): no id, name or message text. Job payloads: ids/counters.
## Tests (62 new unit; whole suite 1,603 unit + 52 integration, all pass)
Config 7, repository 8, runtime 47: create/move/row failure rules, caps, cooldown boundary (29 s
denied, 30 s allowed), two-guild, bots, rejoin during grace, early wake-up, burnt dedupe key, retry
chain and give-up, reconcile (missing / empty / busy / other-guild row / untracked look-alikes, run
twice), payload parsing, a `tokio` race of two joins, and a seeded liveness test: 6 seeds x 4 rounds x
40 random joins/leaves/moves/clock steps against the real queue, runner and a SQLite file, with 8-25 %
injected Discord failures, restarts and a claim-then-crash; it ends with no stray room, no row, no
unfinished job, only bot-made channels deleted, and asserts it created, deleted and met faults.
**RED** `5d36c58`: tests plus stub bodies (written after the design, not before it): 51 failed (config 4,
repository 6, runtime 41) / 144 passed; **GREEN** `c384744` (then one test-only fix commit). Mutants (`target/vr2-tests/mutate.sh`), all
killed: guild check, move-ok, no row, rejoin, untracked wake-up, busy reconcile, attempt stamp, burnt
key, alert per retry, no wake, no create lock, bot filter, hub-is-row, guild wake-up, empty-since,
leave handling, global cap. Two survived once (untracked, guild wake-up: clock not advanced); fixed.
## Config keys (read by the wiring card, not here; a bad value = mode off, error names the KEY only)
`VC_MODE` off|shadow|live (off; shadow only counts hub joins), `VC_HUB_CHANNEL_IDS` (empty = no hub),
`VC_CATEGORY_ID` (empty = no rooms; a list is refused), `VC_CREATE_COOLDOWN_SECS` 0-86400 (30),
`VC_MAX_ROOMS` 1-500 (25), `VC_EMPTY_GRACE_SECS` 0-3600 (10). Also needs `GUILD_ID` (home guild) and
the core database; wiring passes the bot's own user id, staff alert channel and `CacheReady` hook.
## NOT done / known limits
No command (VR3: rename, lock, permit, claim), no feature switched on, not registered, `main.rs`,
`config.rs`, `.env.example` untouched. The Serenity adapter has not met Discord. A leave is only seen
if serenity supplies the old voice state; otherwise the room waits for the next reconcile. Cooldown
memory of failed attempts is lost at restart (rows still count). A member joining between the
occupancy check and the delete is disconnected. If the row write and the cleanup delete both fail the
channel is orphaned and untracked, so never deleted. `locked`/`user_limit` are stored, unused. VR1's
doc lists `VOICE_ROOM_*` keys; the plan and this card use `VC_*`, which is what this module reads.
Enabling needs owner decisions D01 and D10.
