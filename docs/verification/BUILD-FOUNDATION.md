# A01 — Offline build and helper regression foundation

## Scope and isolation

- Baseline: `190b1b0619900052bfd9cdc824f005bebb2ecafe`.
- Branch: `wt/skynet-build-foundation`.
- Workspace: `C:/Users/anose/AppData/Local/hermes/projects/skynet-workers/build-foundation`.
- Initial `git rev-parse HEAD` matched the baseline; `git branch --show-current`
  matched the branch; `git status --short` was empty. `.env` existence was
  checked without opening it: **absent**.
- No access to the main checkout, credentials, private profiles or Discord API.
  No application startup, command registration, migration, deployment, push or
  merge. Only Cargo dependency fetching and local build/test execution occurred.
- `Cargo.toml`, `README.md`, `.env.example` and `.gitignore` remain unchanged.

Implementation commit: the local commit containing this report (use
`git log -1 --format=%H -- docs/verification/BUILD-FOUNDATION.md` to identify it).
The exact implementation SHA is also recorded on the card's QA handoff.

## Environment

Observed locally on Windows, target `x86_64-pc-windows-msvc`:

```text
rustc 1.99.1 (58cf8f9c3 2026-10-13)
cargo 1.99.1 (5f94df478 2026-08-27)
rustfmt 1.10.0-stable (58cf8f9c35 2026-10-13)
Python 3.14.9
```

## Reproduction before edits

All commands ran in the assigned worktree. Standard output and standard error
were redirected to the ignored `target/verification/` directory; the following
are the exit statuses of the Cargo commands, not of the logging wrapper.

| Exact command | Exit | Observed result |
| --- | ---: | --- |
| `cargo fmt --all -- --check` | 1 | Formatting differences in `src/main.rs` and `src/config.rs` |
| `cargo check --locked` | 101 | Lockfile update prohibited by `--locked` |
| `cargo test --locked` | 101 | Same lockfile error; tests could not yet execute |

Both locked commands reported:

```text
error: cannot update the lock file C:\Users\anose\AppData\Local\hermes\projects\skynet-workers\build-foundation\Cargo.lock because --locked was passed to prevent this
help: to generate the lock file without accessing the network, remove the --locked flag and use --offline instead.
```

Root cause: the manifest already named the package `discord-moderation-hub`,
but its lockfile entry still named it `skynet-network-core`. Changed that one
lockfile name, without running dependency resolution or `cargo update`.
Immediately after this single edit, `cargo check --locked` exited **0**.
It downloaded existing locked crates and compiled successfully, with the six
pre-existing warnings described below. This isolates the lock drift from the
unrelated formatting failures.

## Changes and regression scope

- One root package name corrected in `Cargo.lock`; manifest and all dependency
  records unchanged.
- Applied `cargo fmt --all` to both Rust source files.
- Added seven table-driven, credential-free unit tests in a `#[cfg(test)]`
  module in the existing binary source. They call real pure helpers; the test
  harness does **not** invoke the application's `main` function.
- CI uses Ubuntu 24.04 and Rust **1.99.1**, explicitly installs rustfmt, checks
  formatting, builds with `--locked`, and tests with `--locked --offline` after
  the build has populated the dependency cache. CI's token is read-only;
  commands contain no interpolation of untrusted GitHub event fields.

Observed tests:

1. `embed_truncation_ascii_boundaries`: empty input, exact limit, spare capacity,
   truncation at the ellipsis size and below the input length.
2. `embed_truncation_unicode_scalar_boundaries`: multibyte accented/CJK/emoji
   input, exact and over-limit boundaries, combining marks and ZWJ sequences;
   asserts exact content and scalar-count limit for limits at least three.
3. `embed_truncation_preserves_existing_small_limit_ellipsis`: limits 0–3 for
   ASCII and multibyte input explicitly characterize the existing three-dot
   result rather than changing behavior.
4. `emoji_input_normalization_cases`: empty/whitespace, Unicode whitespace,
   emoji including skin tone, static/animated custom syntax, canonical key,
   incomplete custom syntax and plain text.
5. `reaction_type_normalization_cases`: Unicode, named static/animated custom
   reactions and the unnamed custom-emoji fallback.
6. `emoji_input_matches_reaction_lookup_keys`: equivalent user input and
   reaction objects produce matching lookup keys.
7. `code_block_placeholder_and_fence_sanitization`: empty/blank placeholders,
   ordinary Unicode text, embedded fence replacement and a 981-emoji input
   truncated to a 980-scalar payload including ellipsis.

These are characterization tests for existing behavior, not a runtime feature
change. They passed on their first executable run (`cargo test --locked`, exit
0). No helper panic was reproduced and no production helper fix was required.

## After-change verification

| Exact command | Exit | Observed result |
| --- | ---: | --- |
| `cargo fmt --all` | 0 | Formatting applied |
| `cargo fmt --all -- --check` | 0 | No formatting differences |
| `cargo check --locked` | 0 | Compiles; six existing warnings |
| `cargo test --locked` | 0 | **7 passed, 0 failed, 0 ignored**, 0 filtered out |
| `cargo check --locked --offline` | 0 | Cached dependency build works offline |
| `cargo build --locked --verbose` | 0 | Actual CI build command passes locally |
| `cargo test --locked --offline --verbose` | 0 | Actual CI test command passes locally; **7 passed** |
| `git diff --check` | 0 | No whitespace errors |
| `python target/verification/audit.py` | 0 | Exact dependency and formatted-baseline comparisons pass |
| `python target/verification/precommit.py` | 0 | Documented audit recipe rerun successfully; added-line secret/injection/eval/deserialization/SQL scan has no matches |
| `git diff --exit-code -- Cargo.toml README.md .env.example .gitignore` | 0 | Public input files unchanged |

Actual test-harness summary:

```text
running 7 tests
test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s
```

## Exact dependency comparison

Compared parsed `Cargo.lock` package records from `git show
190b1b0619900052bfd9cdc824f005bebb2ecafe:Cargo.lock` and the working file using
Python `tomllib`. After excluding each root package, the complete lists of
**260 dependency records** are equal, including name, version, source,
checksum and dependency edges. The root records are equal after substituting
only the new name. The manifest text is identical to baseline.

SHA-256 of the unchanged dependency-record list, serialized with
`json.dumps(records, sort_keys=True, separators=(',', ':')).encode()`:

```text
before: 3e12fe7caf0cd630315732ad5d323abefd7c9b946e0c29aba0be913ce6214c2f
after:  3e12fe7caf0cd630315732ad5d323abefd7c9b946e0c29aba0be913ce6214c2f
```

| Direct dependency | Baseline locked version | After locked version |
| --- | --- | --- |
| dotenvy | 0.15.7 | 0.15.7 |
| poise | 0.6.2 | 0.6.2 |
| rusqlite | 0.37.0 | 0.37.0 |
| serde | 1.0.229 | 1.0.229 |
| serde_json | 1.0.151 | 1.0.151 |
| serenity | 0.12.5 | 0.12.5 |
| tokio | 1.53.1 | 1.53.1 |
| tokio-rusqlite | 0.6.1 | 0.6.1 |

Manifest requirements such as `poise = "0.6.1"` are semver ranges, not the
resolved versions. No framework version was upgraded by this task.

## Diff review and reproducible audit

Reviewed `git diff -- Cargo.lock .github/workflows/rust.yml` and the complete
`git diff --ignore-all-space -- src/main.rs src/config.rs`. Ignoring whitespace
alone still shows rustfmt import reordering and line wrapping. To avoid
mistaking those for behavioral changes, additionally formatted each baseline
source through stdin and compared it exactly with the current source, removing
only the added test module for `main.rs`. Both matched. Thus every production
source change is exactly rustfmt's output: authorization, routing, schema,
retention, persistence and logging behavior are unchanged.

The audit below is the substantive content of the executed ignored
`target/verification/audit.py`; it can be saved and rerun from this worktree
using `python <saved-script-path>`:

```python
import hashlib
import json
from pathlib import Path
import subprocess
import tomllib

BASE = '190b1b0619900052bfd9cdc824f005bebb2ecafe'
def baseline(path):
    return subprocess.check_output(
        ['git', 'show', f'{BASE}:{path}']
    ).decode('utf-8')

before = tomllib.loads(baseline('Cargo.lock'))['package']
after = tomllib.loads(Path('Cargo.lock').read_text(encoding='utf-8'))['package']
old = next(p for p in before if p['name'] == 'skynet-network-core')
new = next(p for p in after if p['name'] == 'discord-moderation-hub')
assert {**old, 'name': new['name']} == new
old_deps = [p for p in before if p is not old]
new_deps = [p for p in after if p is not new]
assert old_deps == new_deps
assert baseline('Cargo.toml') == Path('Cargo.toml').read_text(encoding='utf-8')
print(len(old_deps), 'identical dependency records')
print(hashlib.sha256(json.dumps(
    old_deps, sort_keys=True, separators=(',', ':')
).encode()).hexdigest())
for path in ['src/main.rs', 'src/config.rs']:
    formatted = subprocess.run(
        ['rustfmt', '--edition', '2021', '--config', 'skip_children=true',
         '--emit', 'stdout'],
        input=baseline(path), capture_output=True, text=True,
        encoding='utf-8', check=True,
    ).stdout
    current = Path(path).read_text(encoding='utf-8')
    if path == 'src/main.rs':
        start = current.index('#[cfg(test)]\nmod tests {')
        end = current.index('fn truncate_for_embed(', start)
        current = current[:start] + current[end:]
    assert formatted == current, path
print('AUDIT PASS')
```

## Limitations and remaining review

- Six pre-existing compiler warnings remain: deprecated
  `GuildChannel::permissions_for_user`, two unused ticket button constants,
  unused `handle_ticket_button`, unused `next_ticket_number`, and three unread
  ticket/staff config fields grouped into one warning. They were already
  observed immediately after the lock-only fix. They are not suppressed or
  repaired because ticket/authorization changes are outside this slice.
- Truncation counts Unicode scalar values, **not** UTF-16 units or graphemes.
  For limits below three, truncating still yields `...`, exceeding that small
  limit. These are documented existing semantics, not a new guarantee of all
  Discord length constraints. Current callers use limits 980 or 1000.
- The emoji parser's existing permissive handling is unchanged; these tests
  are not comprehensive custom-emoji validation or a permission-policy audit.
- Cached offline commands succeeded after fetching locked dependencies. This
  is not a vendored, air-gapped first-build guarantee.
- CI commands were exercised on Windows; the hosted Ubuntu workflow has not
  been run or published. Ubuntu 24.04 runner images and `actions/checkout@v4`
  can receive upstream updates; compiler version and Cargo dependency records
  are pinned, not the entire OS/action supply chain.
- Build artifacts, raw command logs and audit script are ignored under
  `target/verification/`; this tracked report preserves the evidence and audit
  recipe. No generated build artifacts are committed.
- Independent QA must rerun formatting, locked check and locked tests and
  inspect baseline-to-implementation diff. Local review approval is not merge
  or deployment authorization and does not close the held roadmap epic.
