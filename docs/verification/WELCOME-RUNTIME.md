# WELCOME-RUNTIME - welcome runtime (join roles + welcome message)

Card WE2. Modules `src/welcome/runtime.rs` (runtime) and `src/welcome/config.rs` (parser) - thin layer, Discord calls behind traits.

`welcome::runtime::decide_join(&JoinFacts, &WelcomeConfig) -> JoinDecision` is the pure decision. `welcome::runtime::WelcomeRuntime::handle_join(&JoinFacts) -> JoinOutcome` grants the join roles through the role service (K03 `eligibility::decide` on the `JoinRole` path, K04 `grant::apply`, RG2 `RoleSource`/`RoleWriter`) and posts the message rendered by WE1 `render::render_welcome`. `welcome::runtime::validate_join_roles(..) -> Vec<JoinRoleRefusal>` is the startup check of the join roles. `welcome::config::parse(lookup) -> WelcomeConfig` reads the four keys through the K06 helpers only. `welcome::runtime::WelcomeFeature` implements the registry's `features::Feature` hook (it reads a `GuildMemberAddition` event, takes the guild facts from the `GuildLookup` trait and calls the runtime); it is NOT registered - wiring card W3 does that.

## Rules the code enforces
1. A bot that joins is skipped for the roles AND for the message: no role is granted and nothing is posted. The check order is fixed: a feature that is off, then a missing channel, then a missing template, then a bot.
2. A join role that the K03 policy refuses (privileged permission, protected, managed, @everyone, at or above the bot, deleted) is refused at startup validation with the key `JOIN_ROLE_IDS` and the policy's own reason code. A role whose facts could not be looked up is refused with `facts_unavailable`; an unknown bot top position with `bot_top_position_unknown`.
3. The allowed mentions of the message are the new member only: `users` holds that one id, `roles` is empty and `everyone` is false.
4. A send failure is reported by a stable code (`welcome_send_forbidden`, `welcome_send_rate_limited`, `welcome_send_failed`) and never blocks the role grant; a grant failure never blocks the message. Each item keeps its own outcome, so a later failure does not overwrite an earlier one.
5. A template path that is not the one canonical spelling (EM1 `embeds::posts::validate_key`) is refused: a separator, a `..`, a drive letter, a URI scheme, a dot or a capital letter all fail. A bad value switches the feature off and the KEY is named, never the value.
6. Shadow mode sends nothing and grants nothing; it produces a would-do record with counts and reason codes and no user content.
7. The two job kinds (`welcome_grant_roles`, `welcome_send_message`) are `AtMostOnce`: a granted role and a posted message are user-visible and irreversible, so a crash abandons the job and never repeats it.

## Tested (46 unit tests: 14 in `config.rs`, 32 in `runtime.rs`)
The four keys parsed, the join list deduped and sorted, shadow and an unknown mode, a blank value as unset; a bad channel, template path or join list switching the feature off and naming the key; the escaping path spellings refused; no value in the summary line. The bot skipped for both parts, the human granted and posted, a privileged and a protected role refused by the role service, a send failure not blocking the grant, a grant failure not blocking the message, the two failure codes kept apart, the send code table (403, 429, the rest), the alert carrying the code only, the mentions the new member only, shadow sending and granting nothing, the unusable lookup granting nothing, the startup refusals with key and code, the crash abandoning both jobs (real `jobs` queue), a seeded LCG sequence of 40 joins that always reaches its final state and never goes quiet, and the feature hook: a member join reaching the runtime with the guild facts, another event kind ignored, a bot join ignored and the feature off when its mode is off.

RED evidence: commit `d24eb0e` ran this suite against a non-functional scaffold - `test result: FAILED. 101 passed; 31 failed`; the whole suite then was 1176 unit passed and 31 failed, the 52 integration tests passing. Mutation evidence: 10 one-condition mutations, each caught by a named test - the bot check removed (3 tests), the allowed mentions widened (1), a failed grant stopping the message (2), the shadow branch disabled (1), the privileged startup refusal skipped (2), the path-safety rule removed (3), one bad key not switching the feature off (3), a list accepted as the channel (1), the bot flag dropped in the event reader (1), the member count not taken from the guild lookup (1). A ninth attempt (the message sent before the roles) was first misplaced and survived; the corrected mutation was caught by 4 tests.

## Config keys (read by the wiring card, not by this module)
- `WELCOME_MODE`: default `off`. `off|shadow|live`; any other value is off.
- `WELCOME_CHANNEL_ID`: default unset. ONE id; unset or blank leaves the feature off.
- `WELCOME_TEMPLATE_PATH`: default unset. A plain template name; unset leaves the feature off.
- `JOIN_ROLE_IDS`: default empty. A comma list of role ids; a bad entry refuses the whole key.

## NOT done
No command, no feature registered, nothing called from `main.rs`, no `.env`/`config.rs` change, no persistence of its own, no Discord call in production. The template file is not read here: the wiring card loads it and hands the loaded template and its id map to the runtime. The staff alert channel id, the protected role ids and the guild facts come from the wiring card (the `GuildLookup` trait). `src/welcome/mod.rs` keeps the module-level `#![allow(dead_code)]`; W3 removes it.
