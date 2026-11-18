//! Discord adapter for the staff archive command. A20 owns authorization and reply rendering.
use super::{
    archive::OwnedRoot,
    archive_index::MAX_PAGE,
    archive_manage::{self as manage, Manage, ManageError},
    closer::Actor,
    policy::Settings,
    repository::Repository,
    retention::RetentionPolicy,
    runtime_close::{actor, archive_root, now, reply},
};
use crate::{Context, Error};
use std::path::Path;

const UNAVAILABLE: &str = "Ticket archives are unavailable (configuration or storage).";
const USAGE: &str = "Unknown action. Use list, show, health, orphans, retention or restore.";
const MISSING_ID: &str = "Supply an archive ID using the id option.";

enum Action<'a> {
    List,
    Show(&'a str),
    Health,
    Orphans,
    Retention,
    Restore(&'a str),
}

impl<'a> Action<'a> {
    fn parse(action: &str, id: Option<&'a str>) -> Result<Self, &'static str> {
        Ok(match action {
            "list" => Self::List,
            "show" => Self::Show(id.ok_or(MISSING_ID)?),
            "health" => Self::Health,
            "orphans" => Self::Orphans,
            "retention" => Self::Retention,
            "restore" => Self::Restore(id.ok_or(MISSING_ID)?),
            _ => return Err(USAGE),
        })
    }
}

fn open_roots(settings: &Settings) -> Option<(OwnedRoot, OwnedRoot)> {
    let parent = Path::new(&settings.db_path).parent()?;
    let base = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let base = base.canonicalize().ok()?;
    let root = archive_root(settings).ok()?;
    if root.path() != base.join("ticket-archives") {
        return None;
    }
    let staging_path = base.join("ticket-restore-staging");
    let staging = OwnedRoot::open(&staging_path).ok()?;
    // Canonical roots must be the intended siblings, not a redirected directory.
    (staging.path() == staging_path).then_some((root, staging))
}

pub(super) async fn dispatch(
    settings: &Settings,
    repo: &Repository,
    actor: &Actor,
    retention: RetentionPolicy,
    action: &str,
    id: Option<&str>,
    offset: usize,
) -> String {
    // Use the service's own authorization before even opening (or creating) owned roots.
    if let Err(error) = manage::authorize(settings, actor) {
        return manage::render_list(&Err(error));
    }
    let action = match Action::parse(action, id) {
        Ok(action) => action,
        Err(hint) => return hint.into(),
    };
    let storage = settings.clone();
    let Ok(Some((root, staging))) = tokio::task::spawn_blocking(move || open_roots(&storage)).await
    else {
        return UNAVAILABLE.into();
    };
    let m = Manage {
        settings,
        repo,
        root: &root,
        staging: &staging,
        now: &now,
    };
    match action {
        Action::List => manage::render_list(&manage::list(&m, actor, MAX_PAGE, offset).await),
        Action::Show(id) => manage::render_show(&manage::show(&m, actor, id).await),
        Action::Health => manage::render_health(&manage::health(&m, actor, offset).await),
        Action::Orphans => manage::render_orphans(&manage::orphans(&m, actor).await),
        Action::Retention => {
            manage::render_retention(&manage::retention_plan(&m, actor, retention).await)
        }
        Action::Restore(id) => manage::render_restore(&manage::restore(&m, actor, id).await),
    }
}

pub async fn command(
    ctx: Context<'_>,
    action: &str,
    id: Option<&str>,
    offset: usize,
) -> Result<(), Error> {
    ctx.defer_ephemeral().await?;
    let data = ctx.data();
    let (Some(settings), Some(repo)) = (data.config.tickets.as_ref(), data.tickets.as_ref()) else {
        return reply(ctx, UNAVAILABLE).await;
    };
    let Some(actor) = actor(ctx).await else {
        return reply(ctx, &manage::render_list(&Err(ManageError::Denied))).await;
    };
    let text = dispatch(
        settings,
        repo,
        &actor,
        data.config.ticket_archive_retention,
        action,
        id,
        offset,
    )
    .await;
    reply(ctx, &text).await
}
