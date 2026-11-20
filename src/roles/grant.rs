//! Role grant executor with observable partial failures (pure, Discord-free).
//!
//! The caller has already run K03 (`eligibility::decide`) once per role and passes each
//! `Decision` in. `apply` writes only `Allow`ed, not-yet-held roles through an injected
//! `RoleWriter`, retries a 429 a limited number of times through an injected `Sleeper`, and
//! returns one outcome per requested role. The report carries ids and stable codes only.
//!
//! Traits are synchronous on purpose: the core has no runtime and tests drive it directly.
//! A real async Discord writer (card RG2) has to bridge into this sync shape.

use super::eligibility::{Decision, DenyReason};

/// Retries after the first attempt, so a role costs at most 4 writer calls.
pub const MAX_RATE_LIMIT_RETRIES: u8 = 3;
/// Upper clamp on a single sleep request, whatever the writer reports.
pub const MAX_SLEEP_MS: u64 = 30_000;
/// Sleep for retry `n` (1-based) is `n * DEFAULT_BACKOFF_MS` when the writer gave no hint.
pub const DEFAULT_BACKOFF_MS: u64 = 1_000;

pub const API_CODE_UNKNOWN_MEMBER: u32 = 10007;
pub const API_CODE_UNKNOWN_USER: u32 = 10013;
pub const API_CODE_UNKNOWN_ROLE: u32 = 10011;

/// What a writer reports on failure: numbers only, no message text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteError {
    pub status: u16,
    pub api_code: Option<u32>,
    pub retry_after_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailCode {
    Forbidden,
    UnknownRole,
    UnknownMember,
    RateLimited,
    Other,
}

impl FailCode {
    pub fn code(self) -> &'static str {
        match self {
            FailCode::Forbidden => "forbidden",
            FailCode::UnknownRole => "unknown_role",
            FailCode::UnknownMember => "unknown_member",
            FailCode::RateLimited => "rate_limited",
            FailCode::Other => "other",
        }
    }
}

pub fn classify(_error: &WriteError) -> FailCode {
    FailCode::Other
}

pub trait RoleWriter {
    fn add_role(&mut self, member_id: u64, role_id: u64) -> Result<(), WriteError>;
}

pub trait Sleeper {
    fn sleep_ms(&mut self, ms: u64);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrantItem {
    pub role_id: u64,
    pub decision: Decision,
    pub already_held: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct GrantRequest<'a> {
    pub member_id: u64,
    pub items: &'a [GrantItem],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoleOutcome {
    Done,
    NoOp,
    Denied(DenyReason),
    Failed(FailCode),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RoleResult {
    pub role_id: u64,
    pub outcome: RoleOutcome,
    pub writer_calls: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantStatus {
    Empty,
    AllDone,
    Partial,
    AllFailed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantReport {
    pub member_id: u64,
    pub results: Vec<RoleResult>,
}

impl GrantReport {
    pub fn status(&self) -> GrantStatus {
        GrantStatus::Empty
    }

    pub fn summary_line(&self) -> String {
        String::new()
    }
}

pub fn apply<W: RoleWriter, S: Sleeper>(
    _writer: &mut W,
    _sleeper: &mut S,
    request: &GrantRequest,
) -> GrantReport {
    GrantReport {
        member_id: request.member_id,
        results: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{HashMap, HashSet, VecDeque};

    const MEMBER: u64 = 900_001;
    const R1: u64 = 800_001;
    const R2: u64 = 800_002;
    const R3: u64 = 800_003;

    fn http(status: u16) -> WriteError {
        WriteError {
            status,
            api_code: None,
            retry_after_ms: None,
        }
    }
    fn forbidden() -> WriteError {
        http(403)
    }
    fn rate(retry_after_ms: Option<u64>) -> WriteError {
        WriteError {
            status: 429,
            api_code: None,
            retry_after_ms,
        }
    }

    /// Scripted writer: per-role queue of results, `Ok` once the queue is empty.
    #[derive(Default)]
    struct MockWriter {
        script: HashMap<u64, VecDeque<Result<(), WriteError>>>,
        calls: Vec<(u64, u64)>,
        granted: HashSet<u64>,
    }
    impl MockWriter {
        fn with(mut self, role: u64, results: Vec<Result<(), WriteError>>) -> Self {
            self.script.insert(role, results.into());
            self
        }
        fn calls_for(&self, role: u64) -> usize {
            self.calls.iter().filter(|c| c.1 == role).count()
        }
    }
    impl RoleWriter for MockWriter {
        fn add_role(&mut self, member_id: u64, role_id: u64) -> Result<(), WriteError> {
            self.calls.push((member_id, role_id));
            let next = self
                .script
                .get_mut(&role_id)
                .and_then(|q| q.pop_front())
                .unwrap_or(Ok(()));
            if next.is_ok() {
                self.granted.insert(role_id);
            }
            next
        }
    }

    #[derive(Default)]
    struct MockSleeper {
        slept: Vec<u64>,
    }
    impl Sleeper for MockSleeper {
        fn sleep_ms(&mut self, ms: u64) {
            self.slept.push(ms);
        }
    }

    fn item(role_id: u64) -> GrantItem {
        GrantItem {
            role_id,
            decision: Decision::Allow,
            already_held: false,
        }
    }
    fn run(w: &mut MockWriter, s: &mut MockSleeper, items: &[GrantItem]) -> GrantReport {
        apply(
            w,
            s,
            &GrantRequest {
                member_id: MEMBER,
                items,
            },
        )
    }
    fn outcomes(r: &GrantReport) -> Vec<RoleOutcome> {
        r.results.iter().map(|x| x.outcome).collect()
    }

    #[test]
    fn all_ok_is_all_done() {
        let (mut w, mut s) = (MockWriter::default(), MockSleeper::default());
        let r = run(&mut w, &mut s, &[item(R1), item(R2)]);
        assert_eq!(outcomes(&r), vec![RoleOutcome::Done, RoleOutcome::Done]);
        assert_eq!(r.status(), GrantStatus::AllDone);
        assert_eq!(w.calls, vec![(MEMBER, R1), (MEMBER, R2)]);
        assert!(s.slept.is_empty());
    }

    #[test]
    fn first_fails_second_ok_is_partial_never_success() {
        let mut w = MockWriter::default().with(R1, vec![Err(forbidden())]);
        let mut s = MockSleeper::default();
        let r = run(&mut w, &mut s, &[item(R1), item(R2)]);
        assert_eq!(
            outcomes(&r),
            vec![RoleOutcome::Failed(FailCode::Forbidden), RoleOutcome::Done]
        );
        assert_eq!(r.status(), GrantStatus::Partial);
        assert_ne!(r.status(), GrantStatus::AllDone);
        // the failure did not stop the later role from being attempted
        assert_eq!(w.calls_for(R2), 1);
    }

    #[test]
    fn first_ok_second_fails_is_also_partial() {
        let mut w = MockWriter::default().with(R2, vec![Err(http(500))]);
        let mut s = MockSleeper::default();
        let r = run(&mut w, &mut s, &[item(R1), item(R2)]);
        assert_eq!(r.status(), GrantStatus::Partial);
    }

    #[test]
    fn every_role_failing_is_all_failed() {
        let mut w = MockWriter::default()
            .with(R1, vec![Err(forbidden())])
            .with(R2, vec![Err(http(500))]);
        let mut s = MockSleeper::default();
        let r = run(&mut w, &mut s, &[item(R1), item(R2)]);
        assert_eq!(r.status(), GrantStatus::AllFailed);
    }

    #[test]
    fn writer_errors_map_to_stable_codes_through_apply() {
        let cases = [
            (forbidden(), FailCode::Forbidden, "forbidden"),
            (
                WriteError {
                    status: 404,
                    api_code: Some(API_CODE_UNKNOWN_ROLE),
                    retry_after_ms: None,
                },
                FailCode::UnknownRole,
                "unknown_role",
            ),
            (
                WriteError {
                    status: 404,
                    api_code: Some(API_CODE_UNKNOWN_MEMBER),
                    retry_after_ms: None,
                },
                FailCode::UnknownMember,
                "unknown_member",
            ),
            (
                WriteError {
                    status: 404,
                    api_code: Some(API_CODE_UNKNOWN_USER),
                    retry_after_ms: None,
                },
                FailCode::UnknownMember,
                "unknown_member",
            ),
            (http(500), FailCode::Other, "other"),
        ];
        for (err, code, text) in cases {
            let mut w = MockWriter::default().with(R1, vec![Err(err)]);
            let mut s = MockSleeper::default();
            let r = run(&mut w, &mut s, &[item(R1)]);
            assert_eq!(outcomes(&r), vec![RoleOutcome::Failed(code)], "{err:?}");
            assert_eq!(code.code(), text);
            assert_eq!(r.status(), GrantStatus::AllFailed);
            assert_eq!(w.calls_for(R1), 1, "{err:?} must not be retried");
        }
    }

    #[test]
    fn classify_is_conservative_on_ambiguous_or_hostile_errors() {
        // 404 with no / unrelated api code is not guessed to be a role or a member.
        assert_eq!(classify(&http(404)), FailCode::Other);
        let unrelated = WriteError {
            status: 404,
            api_code: Some(10003),
            retry_after_ms: None,
        };
        assert_eq!(classify(&unrelated), FailCode::Other);
        // a member/role api code on a non-404 status is not trusted.
        let wrong_status = WriteError {
            status: 500,
            api_code: Some(API_CODE_UNKNOWN_ROLE),
            retry_after_ms: None,
        };
        assert_eq!(classify(&wrong_status), FailCode::Other);
        assert_eq!(classify(&http(0)), FailCode::Other);
        assert_eq!(classify(&http(u16::MAX)), FailCode::Other);
        assert_eq!(classify(&http(401)), FailCode::Other);
        assert_eq!(classify(&http(429)), FailCode::RateLimited);
        assert_eq!(classify(&http(403)), FailCode::Forbidden);
    }

    #[test]
    fn rate_limit_is_retried_three_times_then_fails() {
        let always_429 = vec![Err(rate(None)); 10];
        let mut w = MockWriter::default().with(R1, always_429);
        let mut s = MockSleeper::default();
        let r = run(&mut w, &mut s, &[item(R1)]);
        assert_eq!(
            outcomes(&r),
            vec![RoleOutcome::Failed(FailCode::RateLimited)]
        );
        assert_eq!(r.status(), GrantStatus::AllFailed);
        assert_eq!(w.calls_for(R1), 4, "1 attempt + 3 retries");
        assert_eq!(r.results[0].writer_calls, 4);
        assert_eq!(
            s.slept,
            vec![1_000, 2_000, 3_000],
            "no sleep after the last 429"
        );
    }

    #[test]
    fn rate_limit_then_success_is_done_and_uses_retry_after_hint() {
        let mut w = MockWriter::default()
            .with(R1, vec![Err(rate(Some(250))), Err(rate(Some(500))), Ok(())]);
        let mut s = MockSleeper::default();
        let r = run(&mut w, &mut s, &[item(R1)]);
        assert_eq!(outcomes(&r), vec![RoleOutcome::Done]);
        assert_eq!(s.slept, vec![250, 500]);
        assert_eq!(w.calls_for(R1), 3);
    }

    #[test]
    fn success_on_the_fourth_call_is_still_done_but_a_fifth_is_never_made() {
        let mut w = MockWriter::default().with(
            R1,
            vec![Err(rate(None)), Err(rate(None)), Err(rate(None)), Ok(())],
        );
        let mut s = MockSleeper::default();
        let r = run(&mut w, &mut s, &[item(R1)]);
        assert_eq!(outcomes(&r), vec![RoleOutcome::Done]);
        assert_eq!(s.slept.len(), 3);
    }

    #[test]
    fn hostile_retry_after_is_clamped() {
        let mut w = MockWriter::default().with(
            R1,
            vec![Err(rate(Some(u64::MAX))), Err(rate(Some(0))), Ok(())],
        );
        let mut s = MockSleeper::default();
        let r = run(&mut w, &mut s, &[item(R1)]);
        assert_eq!(outcomes(&r), vec![RoleOutcome::Done]);
        assert_eq!(s.slept, vec![MAX_SLEEP_MS, 0]);
    }

    #[test]
    fn rate_limit_on_one_role_does_not_skip_the_next() {
        let mut w = MockWriter::default().with(R1, vec![Err(rate(None)); 10]);
        let mut s = MockSleeper::default();
        let r = run(&mut w, &mut s, &[item(R1), item(R2)]);
        assert_eq!(
            outcomes(&r),
            vec![
                RoleOutcome::Failed(FailCode::RateLimited),
                RoleOutcome::Done
            ]
        );
        assert_eq!(r.status(), GrantStatus::Partial);
    }

    #[test]
    fn already_held_role_is_noop_without_api_call() {
        let mut held = item(R1);
        held.already_held = true;
        let (mut w, mut s) = (MockWriter::default(), MockSleeper::default());
        let r = run(&mut w, &mut s, &[held]);
        assert_eq!(outcomes(&r), vec![RoleOutcome::NoOp]);
        assert_eq!(r.results[0].writer_calls, 0);
        assert!(w.calls.is_empty());
        assert_eq!(r.status(), GrantStatus::AllDone);
    }

    #[test]
    fn noop_mixed_with_done_is_all_done_but_noop_with_failure_is_partial() {
        let mut held = item(R1);
        held.already_held = true;
        let (mut w, mut s) = (MockWriter::default(), MockSleeper::default());
        assert_eq!(
            run(&mut w, &mut s, &[held, item(R2)]).status(),
            GrantStatus::AllDone
        );
        let mut w = MockWriter::default().with(R2, vec![Err(forbidden())]);
        assert_eq!(
            run(&mut w, &mut s, &[held, item(R2)]).status(),
            GrantStatus::Partial
        );
    }

    #[test]
    fn k03_deny_never_reaches_the_writer_even_if_held() {
        let reasons = [
            DenyReason::RoleDeleted,
            DenyReason::EveryoneRole,
            DenyReason::ManagedRole,
            DenyReason::AtOrAboveBot,
            DenyReason::PrivilegedPermission,
            DenyReason::ProtectedRole,
            DenyReason::SafewordRestricted,
            DenyReason::MissingStaffContext,
            DenyReason::ActorNotAboveRole,
            DenyReason::ActorNotAboveTarget,
        ];
        for reason in reasons {
            for held in [false, true] {
                let denied = GrantItem {
                    role_id: R1,
                    decision: Decision::Deny(reason),
                    already_held: held,
                };
                let (mut w, mut s) = (MockWriter::default(), MockSleeper::default());
                let r = run(&mut w, &mut s, &[denied]);
                assert_eq!(outcomes(&r), vec![RoleOutcome::Denied(reason)]);
                assert_eq!(r.status(), GrantStatus::AllFailed);
                assert!(w.calls.is_empty(), "{reason:?} held={held} reached writer");
            }
        }
    }

    #[test]
    fn deny_plus_allowed_role_is_partial_and_only_allowed_is_written() {
        let denied = GrantItem {
            role_id: R1,
            decision: Decision::Deny(DenyReason::ProtectedRole),
            already_held: false,
        };
        let (mut w, mut s) = (MockWriter::default(), MockSleeper::default());
        let r = run(&mut w, &mut s, &[denied, item(R2)]);
        assert_eq!(r.status(), GrantStatus::Partial);
        assert_eq!(w.calls, vec![(MEMBER, R2)]);
    }

    #[test]
    fn duplicate_role_is_written_once_and_a_deny_anywhere_wins() {
        let (mut w, mut s) = (MockWriter::default(), MockSleeper::default());
        let r = run(&mut w, &mut s, &[item(R1), item(R1)]);
        assert_eq!(outcomes(&r), vec![RoleOutcome::Done, RoleOutcome::NoOp]);
        assert_eq!(w.calls_for(R1), 1);

        let denied = GrantItem {
            role_id: R1,
            decision: Decision::Deny(DenyReason::ManagedRole),
            already_held: false,
        };
        for items in [[item(R1), denied], [denied, item(R1)]] {
            let (mut w, mut s) = (MockWriter::default(), MockSleeper::default());
            let r = run(&mut w, &mut s, &items);
            assert!(w.calls.is_empty(), "deny on a duplicate reached writer");
            assert_eq!(
                outcomes(&r),
                vec![RoleOutcome::Denied(DenyReason::ManagedRole); 2]
            );
        }

        // a failed first occurrence is reported again, not retried or turned into success
        let mut w = MockWriter::default().with(R1, vec![Err(forbidden())]);
        let r = run(&mut w, &mut s, &[item(R1), item(R1)]);
        assert_eq!(
            outcomes(&r),
            vec![RoleOutcome::Failed(FailCode::Forbidden); 2]
        );
        assert_eq!(w.calls_for(R1), 1);
    }

    #[test]
    fn empty_request_is_empty_not_success() {
        let (mut w, mut s) = (MockWriter::default(), MockSleeper::default());
        let r = run(&mut w, &mut s, &[]);
        assert_eq!(r.status(), GrantStatus::Empty);
        assert!(w.calls.is_empty());
    }

    #[test]
    fn results_keep_request_order_and_one_per_item() {
        let (mut w, mut s) = (MockWriter::default(), MockSleeper::default());
        let r = run(&mut w, &mut s, &[item(R3), item(R1), item(R2)]);
        let ids: Vec<u64> = r.results.iter().map(|x| x.role_id).collect();
        assert_eq!(ids, vec![R3, R1, R2]);
        assert_eq!(r.member_id, MEMBER);
    }

    #[test]
    fn report_carries_ids_and_codes_only() {
        let mut held = item(R3);
        held.already_held = true;
        let denied = GrantItem {
            role_id: R2,
            decision: Decision::Deny(DenyReason::ProtectedRole),
            already_held: false,
        };
        let mut w = MockWriter::default().with(R1, vec![Err(forbidden())]);
        let mut s = MockSleeper::default();
        let r = run(&mut w, &mut s, &[item(R1), denied, held, item(4)]);
        assert_eq!(
            r.summary_line(),
            "grant member=900001 status=partial roles=800001:failed:forbidden,800002:denied:protected_role,800003:noop,4:done"
        );
    }

    #[test]
    fn status_codes_are_stable_in_summary() {
        let (mut w, mut s) = (MockWriter::default(), MockSleeper::default());
        assert_eq!(
            run(&mut w, &mut s, &[item(R1)]).summary_line(),
            "grant member=900001 status=all_done roles=800001:done"
        );
        assert_eq!(
            run(&mut w, &mut s, &[]).summary_line(),
            "grant member=900001 status=empty roles="
        );
        let mut w = MockWriter::default().with(R1, vec![Err(rate(None)); 9]);
        assert_eq!(
            run(&mut w, &mut s, &[item(R1)]).summary_line(),
            "grant member=900001 status=all_failed roles=800001:failed:rate_limited"
        );
    }

    /// Seeded LCG; no dependency, no real randomness.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, bound: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) % bound
        }
    }

    /// Liveness / honesty sweep: random decisions, held flags and writer scripts. Every item
    /// must get exactly one result in order, the writer is called at most 4 times per role and
    /// never for deny/held roles, sleeps are at most 3 per role, `Done` is reported iff the
    /// writer actually accepted that role, and the status matches the per-role outcomes.
    #[test]
    fn seeded_sweep_always_terminates_and_reports_exactly_what_happened() {
        let mut rng = Lcg(0x5EED_CAFE);
        let deny_reasons = [DenyReason::ProtectedRole, DenyReason::AtOrAboveBot];
        for case in 0..3_000u32 {
            let n = rng.next(6) as usize;
            let mut items = Vec::new();
            let mut w = MockWriter::default();
            for i in 0..n {
                let role = 700_000 + i as u64; // distinct roles; duplicates have their own test
                let decision = if rng.next(5) == 0 {
                    Decision::Deny(deny_reasons[rng.next(2) as usize])
                } else {
                    Decision::Allow
                };
                items.push(GrantItem {
                    role_id: role,
                    decision,
                    already_held: rng.next(4) == 0,
                });
                let script: Vec<Result<(), WriteError>> = (0..rng.next(7))
                    .map(|_| match rng.next(5) {
                        0 | 1 => Err(rate(if rng.next(2) == 0 {
                            Some(rng.next(100))
                        } else {
                            None
                        })),
                        2 => Err(http([403, 404, 500][rng.next(3) as usize])),
                        _ => Ok(()),
                    })
                    .collect();
                w.script.insert(role, script.into());
            }
            let mut s = MockSleeper::default();
            let r = run(&mut w, &mut s, &items);

            assert_eq!(r.results.len(), items.len(), "case {case}");
            let mut total_sleeps = 0usize;
            for (it, res) in items.iter().zip(&r.results) {
                assert_eq!(it.role_id, res.role_id, "case {case}");
                let calls = w.calls_for(it.role_id);
                assert_eq!(calls, res.writer_calls as usize, "case {case}");
                assert!(calls <= 4, "case {case}: {calls} calls");
                match (it.decision, it.already_held, res.outcome) {
                    (Decision::Deny(reason), _, out) => {
                        assert_eq!(out, RoleOutcome::Denied(reason), "case {case}");
                        assert_eq!(calls, 0, "case {case}");
                    }
                    (Decision::Allow, true, out) => {
                        assert_eq!(out, RoleOutcome::NoOp, "case {case}");
                        assert_eq!(calls, 0, "case {case}");
                    }
                    (Decision::Allow, false, RoleOutcome::Done) => {
                        assert!(w.granted.contains(&it.role_id), "case {case}: false Done");
                    }
                    (Decision::Allow, false, RoleOutcome::Failed(_)) => {
                        assert!(
                            !w.granted.contains(&it.role_id),
                            "case {case}: hidden grant"
                        );
                        assert!(calls >= 1, "case {case}");
                    }
                    other => panic!("case {case}: unexpected {other:?}"),
                }
                if let RoleOutcome::Failed(FailCode::RateLimited) = res.outcome {
                    assert_eq!(calls, 4, "case {case}: gave up on 429 early");
                }
                if calls > 1 {
                    total_sleeps += calls - 1;
                }
            }
            assert_eq!(s.slept.len(), total_sleeps, "case {case}");
            let ok = r
                .results
                .iter()
                .filter(|x| matches!(x.outcome, RoleOutcome::Done | RoleOutcome::NoOp))
                .count();
            let expected = if r.results.is_empty() {
                GrantStatus::Empty
            } else if ok == r.results.len() {
                GrantStatus::AllDone
            } else if ok == 0 {
                GrantStatus::AllFailed
            } else {
                GrantStatus::Partial
            };
            assert_eq!(r.status(), expected, "case {case}");
        }
    }
}
