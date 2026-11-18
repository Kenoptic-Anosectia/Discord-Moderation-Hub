# Offline SQLite backup / verify / restore operator utility

Local-only A12 review report. Baseline: `74cbe30b2afd28bade3072acde914e2d60180eb9`.
Branch: `wt/skynet-offline-backup-tool`. Author: Alexandra (Hermes Agent).
The exact review commit is recorded in the Kanban review handoff (not self-referentially here).

## Scope and usage

Requires Python **3.12+** (uses `Path.is_junction`) and its bundled SQLite. Standard library only.
No bot/runtime changes, packages, config/environment discovery, scheduling, credentials,
encryption, automatic retention, or production-readiness claim.
All following paths are synthetic examples, not actual application databases. The operator
must supply both paths; the destination parent must already exist.

```console
python -B tools/offline_sqlite_backup.py --help
python -B tools/offline_sqlite_backup.py backup ./synthetic.sqlite ./synthetic-backup.sqlite
python -B tools/offline_sqlite_backup.py verify ./synthetic-backup.sqlite
python -B tools/offline_sqlite_backup.py restore ./synthetic-backup.sqlite ./synthetic-restored.sqlite
python -B tools/offline_sqlite_backup.py verify ./synthetic-restored.sqlite
```

`restore` uses exactly the same SQLite backup API pipeline as `backup`, targeting a **NEW**
path. It does not replace an application database or switch any running process to that path.
Success exits 0 and emits JSON with only `status`, `schema_objects`, and `tables` counts.
Failure exits 1 with `{"status": "failed"}` on stderr; parser errors exit 2 with
`{"status": "invalid_arguments"}`. No row data, schema names/SQL, paths, or exception
messages are printed. Counts include SQLite internal schema objects/tables when present;
these are not row counts and not assertions of application correctness.

## Safety model and limitations

- Open source using SQLite URI `mode=ro`; use `Connection.backup`, never a naive database
  file copy. No source SQL writes, migrations, checkpoint request, or journal-mode changes.
- The operation allocates a uniquely owned temporary file in the destination directory,
  closes the SQLite destination after backup and `PRAGMA integrity_check`, then publishes
  using `os.link`. Hard-link creation is atomic create-if-absent: no rename/replace fallback.
  Filesystems without hard-link support fail closed. Existing output, directories, links,
  and destination `-wal`, `-shm`, or `-journal` names are refused. This includes same paths,
  hard-link aliases, leaf symlinks (even dangling), parent symlinks and Windows junctions.
- Exception cleanup unlinks only the exact temporary filename allocated by this operation,
  not pre-existing backups or filename-pattern matches. No automatic purge, rotation,
  retention, directory sweeping, or deletion of failed/old backup generations.
- Use trusted local directories with suitable permissions/ACLs. A hostile same-account
  process can replace directories, sidecars, or the temporary file between checks. This
  utility is **not** a sandbox against that adversary. Sidecar checks are not an atomic
  multi-file reservation. Do not run concurrent destination writers. Source symlinks are
  resolved, so the operator must trust the explicit source and its directory ancestry.
- This is an **offline/quiescent** operator workflow: no concurrent source writers, schema
  changes, checkpoints, or file manipulation. No timeout guarantee for a competing SQLite
  writer; interrupt rather than use on a busy/live database. No durability guarantee against
  power loss, disk failure, or abrupt process termination (no directory fsync protocol).
  Interruption can leave an owned `.sqlite-backup-*.tmp`; inspect it manually, never glob-delete.
  An error during temporary unlink after publication can leave a valid NEW output despite
  a failure status; inspect/verify that output rather than overwrite or delete it automatically.
- Ordinary source access can update filesystem access timestamps. SQLite read-only WAL
  readers still use shared-memory lock/read-mark coordination; `mode=ro` is not an OS-level
  immutable-files guarantee for every WAL state. The synthetic quiescent WAL fixture checks
  main/WAL/SHM bytes unchanged, but this is not a general concurrent-filesystem guarantee.
  Use a controlled read-only source environment if policy requires filesystem-enforced
  immutability; unsupported WAL environments may fail rather than recover.

### Supported WAL states

SQLite backup incorporates committed WAL frames, unlike copying only the main DB. For a
WAL-mode header with existing sidecars, both regular non-symlink `-wal` and `-shm` files
must already exist; incomplete pairs are refused, not repaired. Keep the environment
quiescent and initialized for read-only access. The test holds an idle fixture connection
open to retain those files, commits a WAL-only row, and compares schema and rows after
backup, verify and restore; it also compares original main/WAL/SHM bytes.

If a WAL-mode main file has **neither** sidecar, it is read with `mode=ro&immutable=1`
under the strict offline assumption, preventing SQLite from creating source WAL/SHM.
This supports standalone completed backups and fully checkpointed closed fixtures.
`immutable=1` is **never** used when a WAL or SHM name exists, because it would ignore
WAL frames. Do not remove a WAL file to make a source pass this check. Any source rollback
`-journal` name is refused (including stale journals); recovery is a separate owner action.
Concurrent creation of a WAL after the absence check violates the offline/race assumptions
and can invalidate the immutable snapshot. No general live-database support is claimed.

## Recovery / restore procedure

1. Quiesce writers through the application's owner-approved procedure; this tool does not
   stop bots or operate on production. Preserve the original database and its sidecars.
2. Select an explicit backup generation and run `verify`. On failure, leave it intact and
   choose another independently verified generation or escalate for recovery. Never delete
   sidecars or change journal mode just to bypass a refusal.
3. Run `restore` to a new, unused file in a trusted directory, then `verify` that new file.
4. Independently validate schema, representative rows/relationships, and application
   invariants with owner-approved checks before any separate application cutover.
   Integrity checks prove neither semantic correctness nor that the desired generation
   was selected. The synthetic tests compare all fixture schema definitions and rows.
5. Leave originals and backups intact. Application cutover, access control, secure storage,
   encryption, and any eventual purge require separate operator decisions. **No auto-purge.**

## Verification record

Environment observed: Windows, Python 3.14.9. Root `.env` absence checked without reading
it. Branch/HEAD matched the baseline before work. Only the three allowed paths are staged.
No Cargo build, runtime startup, network operation, integration, merge, push or deployment.

This resumed workspace already contained an untracked utility and five tests. Their prior
creation order cannot be independently reconstructed from this run. Initial focused suite:
exit 1, destination-sidecar refusal failed. The assertion was retained and implementation
fixed, then suite exit 0. Subsequent production changes were preceded by observed failing
tests: portable parent-link refusal; WAL-source sidecar creation; WAL backup verify/restore
compatibility; standalone WAL journal refusal. Other added tests characterize already
implemented behavior, including injected integrity/publication failures and publication race.
This report does not retroactively claim provenance of inherited TDD steps.

Exact commands (run from the assigned worktree):

```console
python -B -m unittest discover -s tests/ops -p test_offline_sqlite_backup.py -v
python -B -m unittest discover -s tests/ops -p test_offline_sqlite_backup.py -k verify_and_restore_drill -v
python -B tools/offline_sqlite_backup.py --help
git diff --check
```

Final focused suite: **19 tests, 17 passed, 2 skipped, exit 0**. The isolated drill: **1
passed, exit 0**. CLI help: **exit 0**. `git diff --cached --check`: **exit 0**.
The exact commit is in the review handoff. The isolated drill
creates its own temporary SQLite fixture, launches actual CLI backup / verify / restore
subprocesses (each asserted exit 0), reopens backup and restored files, and independently
compares schema SQL and rows, without displaying row data. Help exits 0.
Two actual symlink-creation tests are skipped on this Windows account because the OS
refuses symlink creation. Portable detection tests exercise leaf/parent symlink and junction
rejection via patched predicates, not fabricated OS links; real symlink integration coverage
still needs a suitably privileged/Unix review environment. No tests were weakened to pass.
