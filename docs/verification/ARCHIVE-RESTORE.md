# A16 — Offline ticket archive restore

An authorized caller can copy one verified archive to a fresh directory for offline inspection. Call `restore_to_staging(&archive_root, &staging_root, &request, &limits)` with two owned local roots. The `RestoreRequest` carries the archive directory name (`dir_name`), the manifest SHA-256, ticket ID, and channel ID from a trusted record (`expected_manifest_sha`, `expected_ticket`, `expected_channel`), and the caller-chosen `nonce`. This helper does not authorize callers and is not wired into the bot.

The source must be a direct child of the archive root named `ticket-<digits>-<digits>` (ASCII digits). The destination is `restore-<ticket>-<seq>-<nonce>` under the staging root. The staging root must not be the archive root or a directory inside it. An existing destination is never reused; retry with a different nonce. Success means the copy passed the archive verifier with the original manifest hash. The source is not changed.

## Limits and errors

The default is **10,000 files and 2 GiB** of file contents per restore. Both limits are inclusive and count `manifest.json`, `transcript.html`, and all manifest-listed assets. Directory entries do not count as files. The totals are checked before creating the destination. These limits bound the copy, not verification memory: the existing verifier runs first and reads files into memory. The copy itself streams no more than the verified budget per file.

| Error | Meaning |
| --- | --- |
| `BadName` | The archive name does not match the exact ASCII grammar. No filesystem lookup is made. |
| `Verify` | The source is missing or cannot be verified, the expected hash or archive bytes/file set do not match, the copy detects a source length mismatch, or the completed copy fails verification. |
| `Identity` | The verified manifest is not format 1, or its ticket or channel does not match the caller's expected identity. |
| `Exists` | The requested destination name already exists. It is left untouched. |
| `TooLarge` | The file count or total byte budget is exceeded, or a total overflows. |
| `Unsafe` | The staging root equals or lies inside the archive root, or a path safety check detects a link, Windows reparse point (including junctions), wrong entry type, or unexpected directory resolution. |
| `Io` | A filesystem operation such as opening a root or a copy file, creating the destination, writing, syncing, or cleaning up failed. A cleanup error is reported rather than claiming any partial files are gone. |

On failure after creating a destination, the helper attempts to remove only the directory created for this invocation. Do not use partial output after an error. Use operator-owned roots on a local filesystem and keep their paths, ancestors, and contents stable during restore. Path checks and re-verification do not provide race-proof containment against a local process concurrently changing these directories; the existing verifier uses path-based filesystem operations. A crash or OS refusal to remove the destination can leave partial output.

## What this does not do

No Discord republishing, channel recreation, network access, or decompression. The helper does not log file contents. Restored files are plain files on the PC. This code does not protect them from the Windows account itself; restrict access to the staging root through OS permissions and treat the copy as private ticket data.

## Local verification

- Initial behavior tests against the stub: exit 101; 11 failed at the missing restore behavior. For the API correction, updating the test callers first made `cargo check --locked --offline --tests` exit 101 as expected: `RestoreRequest` and the four-argument signature did not exist yet.
- With `RestoreRequest` in place and `CARGO_BUILD_JOBS=2`, `cargo fmt --all -- --check`, `cargo check --locked --offline --all-targets`, and `cargo test --locked --offline` each exited 0. The full suite reported 108 unit and 42 integration tests passed (the unit count includes the symlink skip below).
- `cargo clippy --all-targets --locked --offline -- -D warnings -D deprecated -D clippy::result_large_err -D clippy::type_complexity`: exit 0, with no lint suppression.
- `cargo test --locked --offline tickets::restore::tests::refuses_source_symlink -- --nocapture`: exit 0. The source-symlink assertion was **NOT RUN**: Windows denied symlink creation (error 1314), and the test printed an explicit skip message.
