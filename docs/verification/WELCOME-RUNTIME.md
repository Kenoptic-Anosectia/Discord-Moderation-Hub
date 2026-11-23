# WELCOME-RUNTIME - welcome runtime (join roles + welcome message)

Card WE2. Modules `src/welcome/runtime.rs` (runtime) and `src/welcome/config.rs`
(parser) - thin layer, Discord calls behind traits. `welcome::runtime::decide_join` is
the pure decision; `handle_join` grants roles via the role service (K03 eligibility,
K04 grant, RG2 RoleSource/RoleWriter) and posts the message rendered by WE1
`render::render_welcome`. `validate_join_roles(..)` is the startup check. `parse(lookup)`
reads the four keys through K06 helpers. `WelcomeFeature` implements `features::Feature`
(GuildMemberAddition → runtime with GuildLookup facts); NOT registered - W3 does that.

## Rules the code enforces
1. A bot that joins is skipped for roles AND message: no role granted, nothing posted.
   Check order: off → missing channel → missing template → bot.
2. A join role refused by K03 policy (privileged, protected, managed, @everyone, at/above
   bot, deleted) is refused at startup with key `JOIN_ROLE_IDS` and the policy code.
   Unresolvable role → `facts_unavailable`; unknown bot top → `bot_top_position_unknown`.
3. Allowed mentions = new member only: `users=[member]`, `roles=[]`, `everyone=false`.
4. Send failure (`welcome_send_forbidden/rate_limited/failed`) never blocks grant; grant
   failure never blocks message. Each item keeps its own outcome.
5. Template path must match EM1 `embeds::posts::validate_key` (no separator, `..`, drive
   letter, URI scheme, dot, or capital). Bad value → feature off, KEY named, not value.
6. Shadow mode sends nothing, grants nothing; produces a would-do record with counts and
   codes, no user content.
7. Both job kinds (`welcome_grant_roles`, `welcome_send_message`) are `AtMostOnce`: a
   crash abandons the job and never repeats it.

## Alert behaviour
Every distinct failure code of a join reaches the staff alert channel, once per code per
join: failed role grant (`forbidden`, `unknown_role`, `unknown_member`, `rate_limited`,
`other`), policy denial (`privileged_permission`, `protected_role`), `facts_unavailable`,
and send failure. Codes only - never a username, message, or id. Deduplicated within one
join: three roles failing for the same reason raise one alert. Cross-join flood control
is a W3 alerter precondition.

## Config: absent vs invalid keys
`parse` reads each key through `classify`, which uses `OsString::to_str()` on every platform
and returns `RawValue`: `Unset` (absent), `Text` (valid Unicode), `NotUnicode` (present, not
Unicode). `NotUnicode` is an INVALID key, never unset: it is named in
`WelcomeConfig::invalid_keys`, the feature stays off, and the value is never kept or echoed.
The tests build the value per platform (`cfg(windows)` unpaired surrogate, `cfg(unix)` bytes);
only the Windows build was run, the Unix test path was NOT executed.

## Tested (18 unit tests in `config.rs`, 47 in `runtime.rs`)
The four keys parsed, the join list deduped and sorted, shadow and an unknown mode, a
blank value as unset; a bad channel, template path or join list switching the feature
off and naming the key; the escaping path spellings refused; no value in the summary
line. A non-Unicode value classified as `NotUnicode`, the key named invalid, the
feature switched off, and the raw bytes never echoed. The bot skipped for both parts,
the human granted and posted, a privileged and a protected role refused by the role
service, a send failure not blocking the grant, a grant failure not blocking the
message, the two failure codes kept apart, the send code table (403, 429, the rest),
the alert carrying the code only, the mentions the new member only, shadow sending and
granting nothing, the unusable lookup granting nothing, the startup refusals with key
and code, the crash abandoning both jobs (real `jobs` queue), a seeded LCG sequence of
40 joins that always reaches its final state and never goes quiet, the same code
alerted once per join (dedupe), and the feature hook: a member join reaching the
runtime with the guild facts, another event kind ignored, a bot join ignored and the
feature off when its mode is off. RED evidence: commit `d24eb0e` ran this suite against
a non-functional scaffold - `test result: FAILED. 101 passed; 31 failed`; the whole
suite then was 1176 unit passed and 31 failed, the 52 integration tests passing.
Mutation evidence: 10 one-condition mutations, each caught by a named test.

## NOT done
No command, no feature registered, nothing called from `main.rs`, no `.env`/`config.rs`
change, no persistence of its own, no Discord call in production. The template file is
not read here: the wiring card loads it and hands the loaded template and its id map to
the runtime. The staff alert channel id, the protected role ids and the guild facts come
from the wiring card (the `GuildLookup` trait). `src/welcome/mod.rs` keeps the
module-level `#![allow(dead_code)]`; W3 removes it.
