# Architecture map of SKYNET // NETWORK_CORE

This page is a picture of how the Rust code in `src/` fits together. It is written for a reader who is learning Rust. Every box, arrow and `file:line` reference was checked against commit `920c48b`. If you read this at a later commit, line numbers may have moved.

The Mermaid diagrams render on GitHub. If you read this as plain text, each diagram is followed by a short list that says the same thing in words.

## 1. How to read this

- A **box** is one real file or folder. A box name like `src/tickets/closer.rs` is the exact path.
- A **solid arrow** `A --> B` means: code in A really uses or calls code in B (a `use` line or a call). The arrow points at the thing being used.
- A **dashed arrow** `A -.-> B` means: A declares B with a `mod` line, but nothing calls B. A dashed box is a module in that state.
- **WIRED** means the code is called from `main.rs` at startup, in a slash command or in an event handler today. **NOT WIRED** means the module exists and has tests, but only its own tests call it (no production caller). **PLANNED** means it exists only in `consolidation-plan/PLAN.md`, not in the code.
- A *module* is Rust's word for a unit of code, usually one file or one folder with a `mod.rs`. A *pure* function does no I/O: same inputs always give the same output. *I/O* means touching Discord, SQLite, the disk or the clock.

## 2. Module map

### 2a. All areas (tickets shown as one box)

```mermaid
flowchart TB
  subgraph SG_MAIN["Entry and config - WIRED"]
    main["src/main.rs<br/>startup, commands, events"]
    config["src/config.rs<br/>reads environment variables"]
  end
  subgraph SG_TICKETS["Tickets - WIRED"]
    tickets["src/tickets/<br/>16 code files, see 2b"]
  end
  subgraph SG_ROLES["Roles"]
    elig["src/roles/eligibility.rs<br/>not wired"]
  end
  subgraph SG_SAFEWORD["Safeword"]
    sw["src/safeword/state.rs<br/>not wired"]
  end
  subgraph SG_FLASH["Flash"]
    flash["src/flash/policy.rs<br/>not wired"]
  end
  subgraph SG_STORAGE["Storage"]
    storage["src/storage.rs<br/>not wired"]
  end
  subgraph SG_HELPERS["Helpers - WIRED"]
    text["src/text_helpers.rs<br/>pure text formatting"]
    del["src/deletion_attribution.rs<br/>pure, who deleted a message"]
    safety["src/archive_safety.rs<br/>pure URL and filename checks"]
  end
  subgraph SG_BACKUP["Backup tool"]
    backup["tools/offline_sqlite_backup.py<br/>run by hand, no Rust caller"]
  end

  main -->|"Config::from_env, load_env"| config
  main -->|"command and event entry points"| tickets
  main -->|"embed text helpers"| text
  main -->|"attribute_deletion"| del
  config -->|"Settings, retention parser"| tickets
  tickets -->|"archive.rs checks URLs"| safety
  main -.->|"mod line only"| elig
  main -.->|"mod line only"| sw
  main -.->|"mod line only"| flash
  main -.->|"mod line only"| storage

  classDef unwired stroke-dasharray: 5 5,stroke:#888888,color:#555555
  classDef tool stroke:#336699
  class elig,sw,flash,storage unwired
  class backup tool
```

In words:

- `main.rs` is the root. It declares nine modules (`main.rs:1-9`) and is the only file that talks to the Poise framework and Serenity (the Discord library) directly.
- `config.rs` reads environment variables into a `Config` value. It builds the ticket `Settings` through `src/tickets/`.
- WIRED: `config`, `tickets`, `text_helpers`, `deletion_attribution`, `archive_safety`. Four are called from `main.rs`. `archive_safety` is called only by `tickets/archive.rs` (`archive.rs:11`).
- NOT WIRED: `roles/eligibility.rs`, `safeword/state.rs`, `flash/policy.rs`, `storage.rs`. `main.rs` has a `mod` line for each, and no code anywhere outside their own tests uses them. Each carries `#![allow(dead_code)]` to silence the compiler warning about unused code (`roles/mod.rs:3`, `safeword/mod.rs:2`, `flash/mod.rs:14`, `storage.rs:11`).
- The backup tool is a Python script, not Rust. No `.rs` file calls it. You run it yourself from a terminal.
- Not drawn, to keep the picture readable: `config.rs`, `tickets/` and `deletion_attribution.rs` each `use crate::Error` or `crate::Context` or `crate::AUDIT_MATCH_WINDOW_MS` from `main.rs` (for example `deletion_attribution.rs:4`). These are arrows back up to `main.rs`.
- Test-only: `tests/archive_input_safety.rs` and `tests/archive_input_properties.rs` pull `archive_safety.rs` in with `#[path]` (not a production caller).

### 2b. Inside `src/tickets/`

`policy.rs` (ticket settings and kinds) is used by almost every file here, so its arrows are left out. The three `*_tests.rs` files and `mod.rs` are also left out.

```mermaid
flowchart TB
  main["src/main.rs"]
  safety["src/archive_safety.rs"]

  subgraph SG_ENTRY["Command and event entry points - Discord I/O"]
    runtime["runtime.rs<br/>ticket panel and open buttons"]
    rclose["runtime_close.rs<br/>/ticket-close, /ticket-recover"]
    rarch["runtime_archives.rs<br/>/ticket-archives"]
    privacy["privacy.rs<br/>log privacy guard"]
  end

  subgraph SG_LOGIC["Decisions and flows"]
    controller["controller.rs<br/>open-ticket flow"]
    closer["closer.rs<br/>close, reconcile, recover"]
    manage["archive_manage.rs<br/>list, health, retention, restore"]
    index["archive_index.rs<br/>archive ids, listing"]
  end

  subgraph SG_ARCHIVE["Archive files on disk"]
    archive["archive.rs<br/>fetch, publish, verify"]
    scan["archive_scan.rs"]
    restore["restore.rs"]
    retention["retention.rs<br/>pure retention rules"]
    remove["archive_remove.rs<br/>not wired"]
  end

  subgraph SG_DB["SQLite"]
    repo["repository.rs<br/>open, tickets, panels"]
    rclosedb["repository_close.rs<br/>close states, history"]
  end

  main --> runtime
  main --> rclose
  main --> rarch
  main --> privacy
  main --> repo
  runtime --> controller
  runtime --> repo
  rclose --> closer
  rclose --> archive
  rclose --> repo
  rarch --> manage
  rarch --> closer
  rarch --> rclose
  rarch --> retention
  controller --> repo
  closer --> archive
  closer --> controller
  closer --> repo
  closer --> rclosedb
  manage --> archive
  manage --> index
  manage --> scan
  manage --> restore
  manage --> closer
  manage --> retention
  manage --> repo
  index --> archive
  index --> closer
  index --> repo
  scan --> archive
  restore --> archive
  privacy --> repo
  repo -->|"migrate on open"| rclosedb
  rclosedb --> repo
  archive --> safety
  remove -.->|"uses verify, OwnedRoot"| archive

  classDef unwired stroke-dasharray: 5 5,stroke:#888888,color:#555555
  class remove unwired
```

In words:

- **Entry points** are the files `main.rs` calls: `runtime.rs` (ticket panel and buttons), `runtime_close.rs` (`/ticket-close`, `/ticket-recover`, startup reconcile), `runtime_archives.rs` (`/ticket-archives`) and `privacy.rs` (a guard around the logger). `main.rs` also opens the `Repository` itself at startup (`main.rs:2971`).
- **Logic** files hold the rules. `closer.rs` decides who may close a ticket and in what order the steps run. Discord work is hidden behind a trait (`CloseIo`, see glossary). `runtime_close.rs` supplies the real Discord version of that trait.
- **Archive** files read and write the archive folder on disk. `retention.rs` is pure. `archive_remove.rs` is the one NOT WIRED file here: nothing calls `remove_archive_dir`, and `tickets/mod.rs:5` marks it `#[allow(dead_code)]`.
- **SQLite** access is only in `repository.rs` and `repository_close.rs`. They are two files for one type: `Repository` is defined in `repository.rs`, and `repository_close.rs` adds more methods to it. Each file imports the other.
- `config.rs` also uses `tickets/policy.rs` and `tickets/retention.rs` (see 2a).

## 3. One wired path end to end: `/ticket-close`

`/ticket-close` is a real slash command. It is registered only when ticket settings are valid and enabled (`main.rs:2922-2931`, called from `main.rs:2954`). Ticket *opening* is switched off by a compile-time constant (`runtime.rs:28`, `OPENING_APPROVED = false`), but close and archive are wired.

In the diagram, a step marked **pure** does no I/O. A step marked **I/O** touches Discord, SQLite, the disk or the clock.

```mermaid
sequenceDiagram
  autonumber
  actor Staff as Staff member in Discord
  participant Poise as Poise framework
  participant Cmd as main.rs ticket_close
  participant RC as runtime_close.rs close
  participant Closer as closer.rs close_ticket
  participant Repo as Repository
  participant DB as tokio-rusqlite thread and SQLite file
  participant Disc as Discord API

  Staff->>Poise: /ticket-close
  Poise->>Cmd: ticket_close (main.rs:2887)
  Cmd->>RC: close(ctx) (runtime_close.rs:190)
  RC->>Disc: defer_ephemeral (I/O)
  RC->>Disc: actor(ctx) reads guild owner and roles (I/O, runtime_close.rs:161)
  RC->>Repo: ticket_by_channel (repository_close.rs:239)
  Repo->>DB: db.call SELECT (I/O)
  DB-->>Repo: Option of Ticket
  RC->>RC: archive_root and Live::new (I/O: opens archive folder, builds HTTP client)
  RC->>Closer: close_ticket(env, actor, id, token) (closer.rs:203)
  Note over Closer: authorized (closer.rs:85) is pure
  Closer->>Repo: claim_close (repository_close.rs:288)
  Repo->>DB: transaction, UPDATE status=closing (I/O)
  Closer->>Disc: CloseIo::freeze (I/O, runtime_close.rs:62)
  Closer->>Repo: advance closing to archiving (repository_close.rs:358)
  Closer->>Disc: archive::fetch_all reads history (I/O, archive.rs:109)
  Closer->>Closer: archive::publish writes folder (I/O, archive.rs:722)
  Closer->>Repo: record_archive then advance to verified (repository_close.rs:436)
  Closer->>Closer: recorded_manifest re-reads the folder (I/O, closer.rs:162)
  Closer->>Disc: CloseIo::delete channel (I/O, runtime_close.rs:86)
  Closer->>Repo: finish advances to closed (closer.rs:473)
  Closer-->>RC: CloseOutcome
  Note over RC: CloseOutcome::reply (closer.rs:112) is pure
  RC->>Disc: reply(ctx, text) ephemeral message (I/O, runtime_close.rs:177)
  Disc-->>Staff: result message
```

In words:

1. **Poise to the command.** Poise matches `/ticket-close` to the function `ticket_close` (`main.rs:2886-2889`), which does nothing except call `tickets::runtime_close::close`.
2. **Adapter (`runtime_close::close`).** Does I/O. It acknowledges the interaction, builds an `Actor` (who is asking) and reads the ticket row by channel. If settings or the repository are missing, it replies "unavailable" and stops.
3. **Decision logic (`closer::close_ticket`).** Mostly a sequence of checks, then calls. `authorized` is pure. The ordering is the safety rule: authorize, claim, freeze, archive, re-read the archive from disk, re-check the live channel, only then delete. Any failure returns `Retained`, and the channel stays.
4. **Repository.** Each method (`claim_close`, `advance`, `record_archive` and so on) runs one short SQLite transaction through `self.db.call(...)`. `db` is a `tokio_rusqlite::Connection`, which runs SQLite on its own thread so the async bot is not blocked. No transaction is held while waiting on Discord.
5. **Reply.** `CloseOutcome::reply` turns the result into a fixed sentence (pure). `reply` sends it as a message only the user sees (I/O).

## 4. Safeword state machine (`src/safeword/state.rs`) - NOT WIRED

The safeword is a feature that takes away a member's restricted roles (so NSFW areas are hidden) and gives them back later. The code here is only the **decision core**. It holds no state, does no I/O and uses no Discord types (`state.rs:1-6`). A future runtime would call `step(&State, Event, &Context) -> (State, Vec<Action>)` (`state.rs:478`), then carry out the returned `Action`s and report back with `ActionDone` or `ActionFailed`. Nothing calls `step` outside the tests today.

```mermaid
stateDiagram-v2
  [*] --> Inactive
  Inactive --> Activating : ReactionAdded
  Activating --> Active : ActionDone makes all 4 activation steps done
  Activating --> Releasing : all activation steps done, then ReactionRemoved now or ActionDone with a release pending
  Active --> Releasing : ReactionRemoved
  Releasing --> Inactive : ActionDone makes all 4 release steps done
  Releasing --> Activating : all release steps done, then ReactionAdded now or ActionDone with a re-activate pending

  Activating --> Activating : ReactionAdded retry, Strike, RoleDeleted, GrantDuringActive, ActionFailed
  Active --> Active : Strike, RoleDeleted, GrantDuringActive emit SaveSnapshot
  Releasing --> Releasing : ReactionRemoved retry, Strike, RoleDeleted, GrantDuringActive, ActionFailed

  note right of Inactive
    NOT WIRED
    no production caller
  end note
```

In words:

- **Four states** (`State`, `state.rs:143-148`): `Inactive`, `Activating(Snapshot)`, `Active(Snapshot)`, `Releasing(Snapshot)`. `Snapshot` is the saved data: roles to restore, roles struck by staff, and a flag for each step (`Snapshot`, `state.rs:102`).
- **Events** (`Event`, `state.rs:90`): `ReactionAdded`, `ReactionRemoved`, `Strike(role)`, `RoleDeleted(role)`, `GrantDuringActive(role)`, `ActionDone(id)`, `ActionFailed(id, code)`.
- **Actions** the machine asks for (`Action`, `state.rs:62`): save a snapshot, remove roles, add the safeword role, send a DM, then the reverse on release.
- Moving from `Activating` to `Active` needs all four activation steps acknowledged (`activation_complete`, `state.rs:250`). If the reaction was removed in the meantime (`pending_release`), it goes straight on to `Releasing`. `Releasing` back to `Inactive` works the same way with the four release steps.
- `restart_action` (`state.rs:748`) says what to do after a restart. It is not drawn.
- Do not confuse this with `SafewordState` in `roles/eligibility.rs:36`. That is a different, two-value enum (`Inactive`, `Active`) that the roles rules take as input. No code links the two today.

## 5. PLANNED target flow (does not exist yet)

**Everything in this diagram marked PLANNED is a design, not code.** It comes from `consolidation-plan/PLAN.md` section 5 (kept outside this repository, in the verification project). Boxes marked "exists, not wired" are real files from section 2.

```mermaid
flowchart TB
  subgraph SG_IN["Discord input - exists today"]
    cmd["Poise slash commands"]
    evt["event_handler, main.rs:1553"]
  end

  cfg["PLANNED: config.rs gets one field, features<br/>each feature mode is off, shadow or live"]
  disp["PLANNED: features::dispatch<br/>runs before the logging privacy guard"]

  subgraph SG_PURE["Pure cores - no I/O"]
    elig["roles/eligibility.rs<br/>exists, not wired"]
    sw["safeword/state.rs<br/>exists, not wired"]
    fl["flash/policy.rs<br/>exists, not wired"]
    more["PLANNED: bump, activity, voice rooms and others<br/>files do not exist"]
  end

  subgraph SG_GLUE["PLANNED: runtime glue"]
    rt["per-feature runtime.rs<br/>Serenity calls behind injected traits"]
    roles["shared role service, roles/grant.rs<br/>every role grant goes through it"]
    jobs["jobs queue and runner<br/>timers survive a restart"]
  end

  subgraph SG_PERSIST["Persistence"]
    prepo["PLANNED: per-feature repository.rs<br/>for example safeword/repository.rs"]
    st["storage.rs: open and versioned migrations<br/>exists, not wired"]
    db[("PLANNED: core.sqlite<br/>one shared file")]
  end

  disc["Discord API"]

  cmd --> disp
  evt --> disp
  cfg -->|"mode gates each feature"| disp
  disp -->|"plain facts in"| SG_PURE
  SG_PURE -->|"decisions and actions out"| rt
  rt --> roles
  roles -->|"decide"| elig
  roles -->|"defer grants while active"| sw
  rt -->|"writes, shadow mode skips these"| disc
  rt --> prepo
  fl -->|"delete after delay"| jobs
  jobs --> prepo
  prepo --> st
  st --> db

  classDef planned stroke-dasharray: 3 3,stroke:#cc6600
  classDef unwired stroke-dasharray: 5 5,stroke:#888888,color:#555555
  class cfg,disp,more,rt,roles,jobs,prepo,db planned
  class elig,sw,fl,st unwired
```

In words:

- The plan keeps the shape tickets already have: a pure `policy` core, a `repository` for SQLite, a `runtime` for Discord calls, then commands. Pure cores come first, and one dedicated "wiring" card connects them to `main.rs`.
- Every feature reads a mode: `off`, `shadow` (log what it would do, make no Discord writes) or `live`. Any other value means off.
- All new state goes into one new SQLite file, `core.sqlite`, opened by `storage.rs`. `tickets.sqlite` and `reaction_roles.sqlite` stay as they are.
- Timers (flash delete, bump reminder, voice room cleanup) would move into one durable `jobs` table, so a restart does not lose them.
- Every role grant would go through one shared service that uses `roles/eligibility.rs` and the safeword rules. The plan notes that today's reaction-role path has no such guard (PLAN.md section 5, item 6).

**Where the code and the plan disagree.** This page draws the code. At commit `920c48b`:

- The plan says each feature has `config.rs` and `command.rs` files, like tickets. The tickets folder has neither. Its settings parser is `ticket_settings` in `src/config.rs:152`, and its slash commands live in `main.rs:2878-2920`.
- The plan says the "unwired" allow sits next to the `mod` line in `main.rs`. In the code each module carries its own `#![allow(dead_code)]` (lines listed in 2a). `archive_remove` has `#[allow(dead_code)]` at `tickets/mod.rs:5`.
- The plan cites `main.rs:2592-2601` (command list), `main.rs:1247` (privacy guard) and `main.rs:2637` (global command registration). Today these are `main.rs:2922-2931`, `main.rs:1561` (the `tickets::privacy::guarded` call inside `event_handler`) and `main.rs:2967`.
- `CORE_DB_PATH`, `features.rs`, `jobs` and the other planned modules appear nowhere in `src/`.

## 6. Rust glossary for this codebase

Each entry points at a real example. Lines are at commit `920c48b`.

1. **Enum with data**: a type that is one of several shapes, and some shapes carry values. `State::Activating(Snapshot)` holds a `Snapshot` (`src/safeword/state.rs:143-148`). `Action::RemoveRoles(Vec<RoleId>)` carries a list (`state.rs:62-64`).
2. **`Result` and `?`**: `Result<T, E>` is "a value or an error". `?` returns the error early or unwraps the value. See `HttpDownloader::new(...)?` (`src/tickets/runtime_close.rs:34`) and `ctx.defer_ephemeral().await?` (`runtime_close.rs:191`).
3. **Trait**: a list of methods a type promises to have. `CloseIo` (`src/tickets/closer.rs:69`) lists what the closer needs from Discord. `Live` implements it for real (`runtime_close.rs:61`), and tests can implement it with a fake.
4. **`impl` block**: where a type's methods are written. `impl Repository` (`src/tickets/repository.rs:66`). A type may have several blocks, in several files: a second one is at `src/tickets/repository_close.rs:213`.
5. **`Option`**: "a value, or nothing" (`Some(x)` or `None`). `ticket_by_id` returns `Result<Option<Ticket>, Error>`, so "database error" and "no such ticket" are different answers (`repository_close.rs:225`). `State::snapshot` returns `Option<&Snapshot>` (`state.rs:153`).
6. **Ownership and borrowing**: every value has one owner. `&T` borrows it read-only, `&mut T` borrows it so you may change it. `step(state: &State, ...)` only reads the old state and returns a new one (`state.rs:478`). `retry_release_actions(&mut self)` changes the snapshot in place (`state.rs:301`).
7. **`Arc`**: a shared, reference-counted pointer, so many tasks can hold one value safely. `Repository` holds `db: Arc<Connection>` (`src/tickets/repository.rs:24`). `Data` holds `config: Arc<Config>` (`src/main.rs:44`).
8. **`async` / `.await`**: an `async fn` can pause while it waits (for Discord or SQLite) and let other work run. `.await` is the pause point. `pub async fn close(...)` (`runtime_close.rs:190`) awaits at its first line (`:191`).
9. **`#[derive(...)]`**: asks the compiler to write common trait code for you (`Clone`, `Debug`, `PartialEq` and so on). `#[derive(Clone)]` on `Repository` (`repository.rs:22`). `#[derive(Clone, Debug, Default, PartialEq, Eq)]` on `State` (`state.rs:142`).
10. **`mod` and `pub`**: `mod x;` declares a module, so the compiler reads `x.rs`. Without `pub`, things are private. `mod archive_safety;` (`src/main.rs:1`) is private to the crate root. `pub mod archive;` (`src/tickets/mod.rs:1`) lets sibling modules use it. `pub(super)` limits something to the parent module (`runtime_close.rs:161`).
11. **Lifetimes and generics** (bonus): `Env<'a, I: CloseIo>` (`closer.rs:123`) borrows its parts for a lifetime `'a` and works with any type `I` that implements `CloseIo`.

## How this page was checked

See the review request on card `t_449a11fe` for the exact commands. Short version: `use`, `mod` and call edges were checked with `grep`. NOT WIRED claims were checked by grepping for the module name outside the module's own files. Line numbers were read back from the files. **The Mermaid blocks were not rendered**: no Mermaid CLI is installed offline, and the task bans network use. They were re-read against Mermaid syntax by hand.
