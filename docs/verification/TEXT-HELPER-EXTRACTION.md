# A04 — Rust text helper extraction verification

## Scope and checkout

- Baseline / initial HEAD: `5fbffd5c97dd494d15d60756368efb9f8cc2cfc6`.
- Branch: `wt/skynet-text-helper-module`.
- Workspace: `C:/Users/anose/AppData/Local/hermes/projects/skynet-workers/text-helper-module`.
- Initial `git status --porcelain=v1`: empty (tracked and untracked clean).
- `.env` existence check: absent before and after extraction; no private files read.
- Toolchain: `cargo 1.99.1 (5f94df478 2026-08-27)`; `rustc 1.99.1 (58cf8f9c3 2026-10-13)`.
- Only production changes: `mod text_helpers`, four imported names, removal of four original definitions from `main.rs`, and their exact definitions in `src/text_helpers.rs` with `pub(crate)` visibility and the same Serenity `ReactionType` import.
- Seven existing helper tests moved verbatim into that module. Five age tests remain in main; one configuration test remains in config. No library crate, dependencies, or runtime behavior added.

## Executed commands and exits

All commands executed in the workspace above. `CARGO_BUILD_JOBS=4` was set on every Cargo invocation. Commands were run serially, with each exit printed explicitly, not inferred from the final shell exit.

| Command | Baseline exit | Candidate exit |
| --- | --- | --- |
| `CARGO_BUILD_JOBS=4 cargo fmt --all -- --check` | 0 | 0 |
| `CARGO_BUILD_JOBS=4 cargo check --locked --offline` | 0 | 0 |
| `CARGO_BUILD_JOBS=4 cargo build --locked --offline` | 0 | 0 |
| `CARGO_BUILD_JOBS=4 cargo test --locked --offline` | 0 | 0 |
| `CARGO_BUILD_JOBS=4 cargo clippy --locked --offline --all-targets` | 0 | 0 |

Other verification commands:

- `cd "$HERMES_KANBAN_WORKSPACE" && pwd && git rev-parse HEAD && git branch --show-current && git status --porcelain=v1 && if test -e .env; then printf '.env exists\n'; else printf '.env absent\n'; fi` — exit 0, expected path/HEAD/branch, empty status, `.env absent`.
- `cargo --version && rustc --version` — exit 0, versions above.
- `python .verify_text_extraction.py --extract` — exit 0; temporary mechanical extraction script using baseline line spans below, not an application/test implementation.
- `python .verify_text_extraction.py` — exit 0 on both candidate comparison runs. Asserted exact expected main and complete module text; printed all four helper equivalence checks, seven unchanged test bodies, complete unchanged protected blobs, and absent `.env`.
- `git diff --check`, `git diff --stat`, `git diff -- src/main.rs`, and `git status --short` — exit 0. Exact main diff and full new module reviewed. Git warns that LF will be replaced by CRLF on a later checkout; comparisons normalize only line endings.
- The first orientation command included `python -c` solely for an existence check; the tool approval gate refused the entire command (exit -1), so it did not execute. Replaced it with the shell `test -e` command above. No approval bypass or configuration change.

The temporary extraction/comparison script is removed before staging. The final commit and post-commit clean status are recorded in the card's review handoff; this report is included in that commit.

## Before/after executed tests

Both full test runs report:

```text
running 13 tests
test result: ok. 13 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

Seven unchanged characterization tests changed qualified prefix only from `tests::` to `text_helpers::tests::`:

- `embed_truncation_ascii_boundaries`
- `embed_truncation_unicode_scalar_boundaries`
- `embed_truncation_preserves_existing_small_limit_ellipsis`
- `emoji_input_normalization_cases`
- `reaction_type_normalization_cases`
- `emoji_input_matches_reaction_lookup_keys`
- `code_block_placeholder_and_fence_sanitization`

The other six tests keep their original paths:

- `tests::age_nonstaff_with_manage_roles_is_denied_before_writes`
- `tests::age_invalid_context_or_configuration_never_writes`
- `tests::age_staff_grants_are_ordered_additive_and_repeatable`
- `tests::age_failed_grants_report_progress_and_never_rollback`
- `tests::age_command_metadata_allows_staff_without_manage_roles`
- `config::tests::unusable_staff_configuration_disables_only_the_age_command`

These are baseline characterization/regression tests, not newly authored RED assertions. No new Rust tests or assertions were introduced, no mutation was applied, and a missing import is not claimed as behavioral RED. This is the refactor phase under the existing green safety net, as explicitly scoped by the task. The moved tests use `super::*` to call the same production module definitions imported by runtime main; there is no duplicate helper implementation. Tiny-limit three-dot output, scalar counting, fence replacement, and permissive emoji parsing are untouched. The existing age assertions still cover staff-only metadata, repeat grants of both roles, partial errors, and no rollback.

## Exact equivalence and protected-file evidence

The temporary comparator read `git show <baseline>:src/main.rs`, extracted baseline helper lines 370–380, 382–390, 633–645, and 647–660 inclusive, and compared complete candidate module text with those definitions prefixed only by `pub(crate) `. It also compared the entire baseline helper-test block (lines 227–367 inclusive) with the moved block, preserving every comment, assertion, literal, and case.

It reconstructed expected main by removing only these four definitions with separating blank lines and the moved seven-test block, then inserting only the module declaration/import. Whole-file equality with candidate main passed. Thus age tests, age authorization/orchestration, event dispatch/logging, database queries, tickets, and every other remaining main section are unchanged, not merely unexamined portions of a passing suite.

SHA-256 of complete baseline helper definitions (UTF-8, LF, including signature and trailing newline; candidate has the same digest after removing only `pub(crate) `):

| Helper | SHA-256 |
| --- | --- |
| `truncate_for_embed` | `f31dea04b7b6f438007549bb62dd88ab63551e6b25edfec010c94fa1d990bf33` |
| `code_block_or_placeholder` | `88c653e2f54ec1b2051fecc7e0ef690afc5d46c2bffadbcf24d77bb4c8696c60` |
| `normalize_reaction_type` | `ec909391a2dd7cc8fc1897a785e3fbf4afa9cef27fb631e8ccc8eb0c2609385f` |
| `parse_emoji_input` | `d5a61133d5546344d764d9c2d46fe3ad1bedd7f7fae7d82e3ab1084414f6b78b` |

For each tracked manifest, complete lockfile, `.github/` file, and other source file, `git hash-object --path=<path> <path>` equaled `git rev-parse <baseline>:<path>`. Complete baseline UTF-8 blob SHA-256 values:

| Unchanged file | SHA-256 |
| --- | --- |
| `Cargo.toml` | `1d25dba73b292c1c2fdb4c8c7a020d8fa429e56824d1f635a55f3483d1b0a1c7` |
| `Cargo.lock` | `032808789175510168ff4c1910071d7eeac8f2d1599d17b9d23598d4001e3db8` |
| `.github/workflows/rust.yml` | `dce944858efffa7e0e0d6df111ad036863d84d20eddcefb211ae5af9f735e2df` |
| `src/config.rs` | `53bf563e22769fc79514528b0d217c538c0dd97c2ab9ab25627fee631e97e06b` |

### Reproduce exact move comparison

Save the following as a temporary Python file **inside this worktree**, then run `python <file>` from the worktree (exit 0 required). This compares source text; it does not reimplement application behavior. Remove the temporary file afterward.

```python
from pathlib import Path
import subprocess

base = subprocess.check_output([
    'git', 'show', '5fbffd5c97dd494d15d60756368efb9f8cc2cfc6:src/main.rs'
]).decode('utf-8')
lines = base.splitlines(keepends=True)
helpers = [''.join(lines[a-1:b]) for a, b in
           [(370, 380), (382, 390), (633, 645), (647, 660)]]
tests = ''.join(lines[226:367])
expected_main = base
for helper in helpers:
    assert expected_main.count(helper + '\n') == 1
    expected_main = expected_main.replace(helper + '\n', '', 1)
expected_main = expected_main.replace('\n' + tests, '', 1)
expected_main = expected_main.replace(
    'mod config;\n', 'mod config;\nmod text_helpers;\n', 1)
expected_main = expected_main.replace(
    'use tokio::sync::RwLock;\n',
    'use text_helpers::{\n'
    '    code_block_or_placeholder, normalize_reaction_type, parse_emoji_input, truncate_for_embed,\n'
    '};\nuse tokio::sync::RwLock;\n', 1)
expected_module = 'use poise::serenity_prelude::model::channel::ReactionType;\n\n'
expected_module += '\n'.join('pub(crate) ' + h for h in helpers)
expected_module += ('\n#[cfg(test)]\nmod tests {\n    use super::*;\n'
                    '    use poise::serenity_prelude as serenity;\n\n' + tests + '}\n')
assert Path('src/main.rs').read_text(encoding='utf-8') == expected_main
assert Path('src/text_helpers.rs').read_text(encoding='utf-8') == expected_module
print('Exact helper/test move and unchanged remaining main: PASS')
```

## Warnings, review, and limitations

Baseline and candidate check/build/test each emit the same six warnings: deprecated `GuildChannel::permissions_for_user`, two unused ticket-button constants, unused `handle_ticket_button`, unused `next_ticket_number`, and the two unread ticket-category fields grouped as one warning. Clippy emits those plus three `result_large_err` warnings and one `type_complexity` warning: ten warnings for the binary and ten duplicates for the test target in both runs. Candidate line numbers shift due to extraction. No suppression or unrelated repairs were made.

Manual review of added Rust lines found no new credentials, shell execution, dynamic evaluation, deserialization, SQL, I/O, unsafe code, or external calls. Production logic is an exact move rather than a rewrite. Manifest, complete lockfile, CI, config, and unrelated main text comparisons provide scope evidence beyond tests.

This is local offline compile/test/source-equivalence evidence, not a live Discord or database integration test, proof of all possible future inputs, or independent QA approval. No `.env`, authentication/private profiles, main-checkout files, bot/model startup, `cargo run`, schema execution, migrations, deployment, push, or merge were used. Public Cargo cache access and local build artifacts only. Independent `qa` review on this same card must compare the exact bodies/cases and untouched sections and rerun the tests/checks before approval. Approval remains local-only; integration and broader epic work are not authorized by this card.
