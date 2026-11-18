# ARCHIVE-REMOVE — verified, crash-safe removal of ONE archive directory

Slice **A19**. Code: `src/tickets/archive_remove.rs`. This is an offline building block of the
A08 ticket-archive work. **Nothing here is wired to the running bot, to configuration, or to a
scheduler, and nothing here decides *whether* an archive may be deleted.** Deciding eligibility
(age, count, per-guild policy) is the retention job's job (A15). This module only answers
"given that you have decided to delete *this specific* archive, do it safely or refuse".

---

## Public API

```rust
pub enum RemoveError { BadName, NotFound, Unsafe, Verify, Identity, Io }
pub struct Removed { pub dir_name: String, pub files: u64, pub bytes: u64 }
pub struct RemoveRequest<'a> {
    pub dir_name: &'a str,
    pub expected_manifest_sha: &'a str,
    pub expected_ticket: i64,
    pub expected_channel: u64,
    pub expected_messages: u64,
    pub expected_assets: u64,
    pub nonce: u64,
}
pub fn remove_archive_dir(root: &OwnedRoot, req: &RemoveRequest<'_>) -> Result<Removed, RemoveError>;
pub fn finish_pending_removals(root: &OwnedRoot) -> Result<usize, RemoveError>;
```

`RemoveRequest` is a struct rather than a long argument list (a flat 8-argument signature trips
`clippy::too_many_arguments`, and `#[allow]` is forbidden in this codebase). The behaviour is
exactly the specified one; only the call shape differs.

`nonce` is supplied by the caller so that two removals in the same process cannot pick the same
trash name. `finish_pending_removals` does not need it.

---

## Exact order of steps, and why

`remove_archive_dir` performs these steps **in this order**. Every step before the rename is
read-only with respect to the archive, so every failure up to and including step 3 leaves the
archive byte-for-byte untouched.

### 1. Name validation → `BadName`

`dir_name` must parse as exactly `ticket-<digits>-<digits>`. The parser accepts only ASCII
digits in both numeric fields and rejects everything else: `..`, `../x`, absolute paths, drive
letters (`C:\...`), path separators, leading dots, `ticket-x-1`, `ticket--1`, `ticket-5-`,
`ticket-5-1-1`, the empty string, trailing whitespace, `+5`, and any trailing `/` or `.\`.

**Why first:** a name is the only fully caller-controlled input, and it is the only input that
could escape the root or reach a path the caller did not mean to touch. Rejecting it before any
filesystem call means a hostile or mistaken name cannot cause a single read, let alone a delete.
The numeric fields are also parsed here and become the ticket/seq embedded in the trash name.

### 2. Entry type check → `NotFound` / `Unsafe`

`symlink_metadata` (not `metadata`) is used, so the link itself is inspected and the link target
is never followed:

* entry absent → `NotFound`;
* entry is a symlink or junction → `Unsafe`;
* entry is a regular file → `Unsafe`;
* entry is a real directory → continue.

**Why:** a symlinked `ticket-5-1` pointing at some other directory must never be deleted through
the link. `remove_dir_all` on a symlink is a footgun on some platforms, and deleting the *target*
would destroy data the caller never named. A plain file that happens to be called `ticket-5-1` is
not an archive and must not be removed either. The link target is left completely untouched.

### 3. Verification and identity → `Verify` / `Identity`

`archive::verify(root, dir, Some(expected_manifest_sha), &[])` must succeed. That re-reads the
real bytes on disk and checks the manifest hash, the transcript length and hash, every asset's
length and hash, that the set of entries is exactly `manifest.json` + `transcript.html` +
`assets/`, that no entry is a symlink, and that the manifest parses.

Then the verified manifest must satisfy all of:

| field | required value |
|---|---|
| `format` | `1` |
| `ticket` | `expected_ticket` |
| `channel` | `expected_channel` |
| `message_count` | `expected_messages` |
| `assets.len() as u64` | `expected_assets` |

**Why two distinct errors:** `Verify` means *the bytes on disk are not what the manifest
promises* — corruption, a partial write, a tampered transcript or asset. `Identity` means *the
bytes are internally consistent but they are not the archive the caller named* — a caller bug, or
a manifest row that points at a different ticket/channel/count. Separating them lets the caller
tell "this archive is damaged, escalate" from "you asked me to delete the wrong thing", which
are very different operational situations.

**Why verification precedes deletion, always:** deleting first and verifying afterwards would
make every corruption case unrecoverable. Verifying first means the only way data is destroyed
is when it was already exactly the archive the caller intended to destroy.

The removal counts (`files`, `bytes`) are computed **here**, from the verified manifest, plus the
transcript length and the on-disk length of `manifest.json` — both read *before* anything is
renamed or deleted. (Reading the manifest length after the delete would silently report zero.)

### 4. Atomic rename, then delete → `Io`

The archive directory is renamed to `.trash-<ticket>-<seq>-<nonce>` **inside the same root**, and
only then is the trash directory removed with `remove_dir_all`.

* If the trash name already exists, the call fails with `Io` **before** anything is renamed —
  an existing trash entry is never overwritten.
* The rename is a single filesystem operation, so the name change is atomic.

**Why rename-then-delete rather than delete-in-place:** `remove_dir_all` on the archive itself
is not atomic. If the process dies half way through, the archive directory is left in a
partially-deleted state — some files gone, some present, no record of what the archive was. With
rename-then-delete, a crash can only leave a *whole* directory under a `.trash-*` name. There is
never a half-deleted `ticket-*` archive, and recovery is a single directory removal.

**Why the trash directory lives in the same root:** the rename must be within one filesystem for
it to be atomic. A trash directory on another volume would turn the rename into a copy, which is
neither atomic nor cheap.

### 5. Result

`Removed { dir_name, files, bytes }` where `files` is the number of regular files removed
(`transcript.html` + `manifest.json` + one per asset) and `bytes` is their total size.

---

## Error meanings

| error | meaning | archive state |
|---|---|---|
| `BadName` | `dir_name` is not exactly `ticket-<digits>-<digits>` | untouched; filesystem never consulted |
| `NotFound` | no such entry in the root | n/a |
| `Unsafe` | the entry is a symlink, junction or regular file, not a real directory | untouched; link target untouched |
| `Verify` | byte-level re-verification failed, or the manifest file could not be measured | untouched |
| `Identity` | the verified manifest does not describe the archive the caller expected | untouched |
| `Io` | a filesystem operation failed: trash-name collision, rename failure, delete failure | see below |

`Io` is the only error that can be raised after a destructive step. Specifically: if the rename
succeeded but `remove_dir_all` then failed, the archive is already parked under
`.trash-<ticket>-<seq>-<nonce>` and the caller gets `Io`. `finish_pending_removals` will finish
that deletion later. The caller must therefore treat `Io` as "the archive may or may not still be
present under its `ticket-*` name"; it must not treat `Io` as "nothing happened".

Every error other than `Io` means the archive directory is exactly as it was.

---

## Crash behaviour

The only destructive window is between the rename (step 4a) and the delete (step 4b).

| crash point | on-disk state | recovery |
|---|---|---|
| before the rename | `ticket-<id>-<seq>` fully intact | nothing to do; the call simply did not happen |
| between rename and delete | `.trash-<id>-<seq>-<nonce>` holding the whole archive; no `ticket-*` name | `finish_pending_removals(root)` removes it |
| during `remove_dir_all` | a partially-deleted `.trash-*` directory | `finish_pending_removals(root)` removes the rest |

In every case the invariant holds: **a `ticket-*` archive directory is never observed
half-deleted.** Either it is complete, or it has been renamed away and is being cleaned up under
a name that says so.

---

## `finish_pending_removals` — crash recovery

```rust
pub fn finish_pending_removals(root: &OwnedRoot) -> Result<usize, RemoveError>;
```

Scans the **immediate children** of `root` and removes only those that are:

* named exactly `.trash-<digits>-<digits>-<digits>` — three numeric fields, nothing else;
* **real directories** — `symlink_metadata` is used, so a symlink or junction is skipped, never
  followed and never deleted through.

It returns how many directories it removed. It is idempotent: a second call on a clean root
returns `0`. It never touches a `ticket-*` archive, a `.staging-*` directory, a stray file, a
look-alike name (`.trash-x`, `.trash-1-1`, `trash-1-1-1`, `.trash-1-1-1-1`), or a plain file that
happens to be named `.trash-<n>-<n>-<n>`.

---

## Repeatability

Removing the same archive twice is safe. The second call finds no entry and returns `NotFound`.
It creates no trash directory and does not touch any sibling.

---

## Verification performed

All four CI gates were run on this branch with `CARGO_BUILD_JOBS=2`:

| command | result |
|---|---|
| `cargo fmt --all -- --check` | clean, exit 0 |
| `cargo check --locked --offline --all-targets` | exit 0, no warnings |
| `cargo test --locked --offline` | exit 0 — 112 + 30 + 7 + 5 = **154 passed, 0 failed** |
| `cargo clippy --all-targets --locked --offline -- -D warnings -D deprecated -D clippy::result_large_err -D clippy::type_complexity` | exit 0, no diagnostics |

The 16 tests in `archive_remove` build **real archives** with `archive::publish` (the same
production publication path the bot uses) into scratch roots under `target/a08-tests/remove/`,
and cover: successful removal with correct file/byte counts; sibling archives and unrelated files
unchanged byte-for-byte; wrong manifest SHA, tampered transcript and tampered asset → `Verify`
with the directory intact; foreign ticket / channel / message count / asset count → `Identity`
with the directory intact; 21 malformed names → `BadName` with nothing outside the root touched;
absent archive → `NotFound`; a regular file named `ticket-5-1` → `Unsafe`; a symlinked (or
Windows junction) `ticket-5-1` → `Unsafe` with the link target's contents intact; a pre-existing
`.trash-5-1-7` → `Io` with the original archive intact; crash simulation where a hand-made
`.trash-1-1-7` is removed and counted as 1, a second call returns 0, and a `ticket-*` archive,
a `.staging-*` directory, a stray file and four look-alike names are all left alone; a symlinked
trash entry is never followed; and repeating a removal returns `NotFound` the second time.

### Platform note (symlink tests)

This host is Windows 11. Creating a real symbolic link there requires
`SeCreateSymbolicLinkPrivilege`, which the test process does not hold (`os error 1314`). The two
symlink tests therefore fall back to a **Windows directory junction** (`mklink /J`), which needs
no elevated privilege. A junction is reported by `symlink_metadata` as `is_symlink() == true` and
`is_dir() == false`, so it exercises exactly the same guard as a POSIX symlink. **Both symlink
tests ran and passed — neither was skipped.** On a host that grants the privilege the helper
uses a true symlink instead.

---

## Scope and honest limits

* This module is **not wired** to the bot, to `config.rs`, to any scheduler, or to any command.
  Nothing calls it except its own tests. Ticket opening remains disabled.
* This module makes **no eligibility decision**. It never reads a retention policy, an age
  threshold, a per-guild setting, or the `ticket_archives` table. The caller supplies the
  expected manifest SHA and identity; the module only checks that the directory matches.
* `nonce` must be unique per removal attempt within a root. A collision is reported as `Io` and
  changes nothing — the caller must supply a fresh nonce to retry.
* `finish_pending_removals` deletes *any* `.trash-<n>-<n>-<n>` directory it finds in the root,
  including one left by an unrelated source, as long as the name matches exactly. The name space
  is reserved for this primitive.
* Like the rest of `archive`, the primitive cannot defend against a concurrent same-account
  process that swaps a directory for a junction between the check and the rename (std offers no
  handle-relative `openat` on Windows). The bound is the same as the one documented on
  `archive::OwnedRoot`.
* No network, no Discord, no live data, no production data and no `.env` were involved. All test
  fixtures are synthetic and all scratch state lives under `<worktree>/target/a08-tests/`.
