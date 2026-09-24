use dotenv::dotenv;
use std::time::Duration;
use teloxide::dptree::endpoint;
use teloxide::prelude::*;
use teloxide::stop::StopToken;
use teloxide::types::{ChatId, InputFile, MessageId};
use teloxide::update_listeners::{self, UpdateListener, webhooks};
use tokio::sync::watch;
use x_media::site;

mod config;
mod ctx;
mod db;
mod handlers;
mod link_cache;
mod media_sender;
mod photo;
mod queue;
mod rate_limit;
mod send;
mod state;

use ctx::CONTEXT;
use handlers::{CONFIG, TASK_QUEUE};

/// Docker `stop` / `compose down` delivers SIGTERM, which teloxide's ctrlc
/// handler (SIGINT only) never sees — without this the process would die
/// before the graceful shutdown below (admin notice, queue drain). Stopping
/// the token unwinds the dispatcher exactly like Ctrl+C does.
#[cfg(unix)]
fn spawn_sigterm_handler(stop_token: StopToken) {
    tokio::spawn(async move {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler");
        sigterm.recv().await;
        log::info!("SIGTERM received, stopping the dispatcher");
        stop_token.stop();
    });
}

#[cfg(not(unix))]
fn spawn_sigterm_handler(_stop_token: StopToken) {}

/// A leftover temp file must be at least this old before the startup sweep
/// touches it. Orphans come from a *previous* run; anything younger could
/// belong to a second instance sharing the temp directory (a misconfiguration,
/// but one that must not cost it its in-flight download).
const ORPHAN_TEMP_AGE: Duration = Duration::from_secs(3600);

/// Removes this project's own leftover temp entries (`x_media::TEMP_FILE_PREFIX`)
/// from `dir` once they are older than `older_than`. Returns how many were
/// removed. Entries that are not ours, or are too young, or cannot be dated,
/// are left alone: the OS temp directory is shared, and the marker prefix plus
/// the age gate are the only two things that make deleting here safe.
fn sweep_temp_dir(dir: &std::path::Path, older_than: Duration) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let cutoff = std::time::SystemTime::now() - older_than;
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name
            .to_string_lossy()
            .starts_with(x_media::TEMP_FILE_PREFIX)
        {
            continue;
        }
        let old_enough = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .is_ok_and(|modified| modified < cutoff);
        if !old_enough {
            continue;
        }
        let path = entry.path();
        let result = if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        match result {
            Ok(()) => removed += 1,
            // Not worth a warning per entry: a file another process removed
            // first (or one we may not delete) is not a problem here.
            Err(e) => log::debug!("could not remove orphaned temp entry {path:?}: {e}"),
        }
    }
    removed
}

#[tokio::main]
async fn main() {
    dotenv().ok();
    // Without RUST_LOG nothing at all was logged (env_logger falls back to
    // `error`), so a deployment that forgot the variable looked like a bot
    // with no logs; and at `debug` the HTTP client's own lines (hyper_util,
    // reqwest) outnumbered the bot's by two to one. The timed builder adds
    // the timestamp the plain `init` omitted, so a line can be compared with
    // a user's report. An explicit RUST_LOG still wins outright — but a blank
    // one (`RUST_LOG=` in `.env`, which is not "unset") must not silence the
    // log the way its absence used to.
    let filter = std::env::var("RUST_LOG")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "info,hyper_util=warn,reqwest=warn".to_string());
    pretty_env_logger::formatted_timed_builder()
        .parse_filters(&filter)
        .init();
    log::info!("Starting bot");

    // Temp media (downloaded files, ugoira/remux dirs) is cleaned up by
    // `TempDir`/`NamedTempFile` on drop — which a killed process never runs.
    // Without this sweep every hard restart left its downloads behind (up to
    // hundreds of MB each) and nothing could tell them apart from a live
    // process's files or from anything else in the OS temp dir. See
    // [`sweep_temp_dir`] for why the age gate makes that safe.
    let orphans = sweep_temp_dir(&std::env::temp_dir(), ORPHAN_TEMP_AGE);
    if orphans > 0 {
        log::info!("swept {orphans} orphaned temp file(s) from a previous run");
    }

    let bot = Bot::from_env();
    // Force the queue workers' shared Bot to initialize now so a missing
    // token fails at startup, not on the first queued task.
    let _ = &*send::BOT;

    // Register the command list with Telegram (client `/` menu).
    if let Err(e) = handlers::register_commands(&bot).await {
        log::warn!("failed to register commands: {e}");
    }

    // The effective tunables, so an operator can see what the process actually
    // resolved (a mistyped DATA_DIR or a forgotten TTL override is otherwise
    // invisible until it bites). The proxy URL is never printed — it may embed
    // credentials — and admin ids are chat identifiers, so they stay at debug.
    let quote_chars = match CONFIG.caption_quote_text_chars {
        0 => "off".to_string(),
        n => format!("{n} chars"),
    };
    log::info!(
        "config: {} admin(s), state {}, edit-message TTL {}s, link cache TTL {}s, caption quote {quote_chars}, proxy={}",
        CONFIG.admin_ids.len(),
        crate::handlers::db_path().display(),
        CONFIG.edit_message_ttl.as_secs(),
        CONFIG.link_cache_ttl.as_secs(),
        if std::env::var("TELOXIDE_PROXY").is_ok() {
            "yes"
        } else {
            "no"
        }
    );
    log::debug!("config: admin ids {:?}", CONFIG.admin_ids);

    // Startup repair, before any worker runs: a queued retry whose media was a
    // local file (ugoira MP4, bsky remux, a downloaded temp file) can never
    // succeed after a restart — the registry that kept those files alive is in
    // memory — so those rows are re-fetched from their post instead of
    // dead-lettering the user's link.
    let repaired = match handlers::repair_lost_local_media(&CONTEXT).await {
        Ok(repaired) => repaired,
        Err(e) => {
            log::error!("startup repair failed: {e}; refusing to start queue workers");
            return;
        }
    };
    if repaired > 0 {
        log::info!("startup repair: re-fetched {repaired} queued task(s)");
    }

    // Queue worker: handles typed tasks, dead-letters failed sends to the
    // task's chat. Both closures use the shared context (the queue requires
    // 'static handlers, and the statics are process-wide anyway).
    TASK_QUEUE
        .start(
            |payload| send::handle_task(&CONTEXT, payload),
            |payload, message| send::dead_letter_notify(&CONTEXT, payload, message),
        )
        .await;
    log::info!("task queue worker started");

    // URL job workers: bounded channel + fixed pool for per-URL work.
    handlers::start_url_workers().await;
    log::info!("url workers started");

    // Site login validation (user request): a failed login notifies the
    // admin and the site disables itself for this process (pixiv).
    let failures = site::validate_all().await;
    if failures.is_empty() {
        log::info!("site logins validated");
    } else {
        for (site_id, message) in &failures {
            log::error!("{site_id} login failed: {message}");
            if let Some(admin) = CONFIG.admin_ids.first() {
                let _ = bot
                    .send_message(ChatId(*admin), format!("{site_id} login failed: {message}"))
                    .await;
            }
        }
    }

    // Background sweep: expires the edit prompts and prunes what has aged out.
    log::info!(
        "edit-expiry sweep: every {}s, ttl {}",
        SWEEP_INTERVAL.as_secs(),
        CONFIG.edit_message_ttl.as_secs()
    );
    let (stop_tx, stop_rx) = watch::channel(false);
    {
        let bot = bot.clone();
        tokio::spawn(async move {
            periodic_sweep(crate::ctx::AppContext::from_statics(&bot), stop_rx).await;
        });
    }

    let handler = dptree::entry()
        .branch(Update::filter_message().branch(endpoint(handlers::message_handler)))
        .branch(Update::filter_inline_query().branch(endpoint(handlers::inline_query_handler)))
        .branch(Update::filter_callback_query().branch(endpoint(handlers::callback_query_handler)));

    let mut dispatcher = Dispatcher::builder(bot.clone(), handler)
        .enable_ctrlc_handler()
        .build();

    if CONFIG.webhook_enabled {
        log::info!("running in webhook mode");
        let url = CONFIG.webhook_url.clone().expect("WEBHOOK_URL is not set");
        // `webhooks::axum` calls set_webhook itself (with the full options,
        // secret token included) — no explicit registration here.
        let listen = CONFIG.webhook_listen.expect("WEBHOOK_LISTEN is not set");
        let port = CONFIG.webhook_port.expect("WEBHOOK_PORT is not set");
        // No secret, no webhook: without one the axum listener accepts any
        // POST, and a forged update can impersonate anyone — admins included.
        let secret = CONFIG
            .webhook_secret_token
            .clone()
            .expect("WEBHOOK_SECRET_TOKEN is not set (required in webhook mode)");
        let mut options = webhooks::Options::new((listen, port).into(), url).secret_token(secret);
        if let Some(cert) = &CONFIG.webhook_cert {
            options = options.certificate(InputFile::file(cert));
        }

        let mut listener = webhooks::axum(bot.clone(), options)
            .await
            .expect("Failed to create webhook listener");
        let stop_token = listener.stop_token();
        spawn_sigterm_handler(stop_token);

        dispatcher
            .dispatch_with_listener(
                listener,
                LoggingErrorHandler::with_custom_text("Error from update listener"),
            )
            .await;
    } else {
        log::info!("running in polling mode");
        // Same listener `dispatch()` builds internally — using
        // `dispatch_with_listener` just exposes its stop token so SIGTERM can
        // unwind the dispatcher before the graceful shutdown below.
        let mut listener = update_listeners::polling_default(bot.clone()).await;
        let stop_token = listener.stop_token();
        spawn_sigterm_handler(stop_token);

        dispatcher
            .dispatch_with_listener(
                listener,
                LoggingErrorHandler::with_custom_text("Error from update listener"),
            )
            .await;
    }

    // Graceful stop (Ctrl+C / SIGTERM): stop the queue first so no new
    // persistent task is leased while the URL workers drain. The two drains
    // share the bounded shutdown budget; URL work may legitimately outlive it,
    // but the queue must not be left running until process exit.
    log::info!("Stopping bot");
    const SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
    let shutdown = async {
        let _ = stop_tx.send(true);
        TASK_QUEUE.stop().await;
        handlers::stop_url_workers().await;
        if let Some(admin) = CONFIG.admin_ids.first() {
            let _ = bot.send_message(ChatId(*admin), "Shutting down...").await;
        }
    };
    if tokio::time::timeout(SHUTDOWN_TIMEOUT, shutdown)
        .await
        .is_err()
    {
        log::warn!("graceful shutdown timed out after {SHUTDOWN_TIMEOUT:?}; exiting");
    } else {
        log::info!("Bot stopped");
    }
}

/// How often [`periodic_sweep`] runs.
const SWEEP_INTERVAL: Duration = Duration::from_secs(300);

/// The background sweep: rewrites the expired edit prompts in place, prunes the
/// link cache, the idle rate-limit buckets and the idle inline-query entries,
/// and reports the queue only when it is not empty.
///
/// Takes the shared [`crate::ctx::AppContext`] — the collaborators as one
/// bundle, production assembling it from the statics and tests from tempdir
/// stores — so a test can drive a tick with a paused clock: a sleeping task
/// nothing drives is how the queue's own sweep kept a missing worker wake-up.
async fn periodic_sweep(ctx: crate::ctx::AppContext<'_>, mut stop: watch::Receiver<bool>) {
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            _ = tokio::time::sleep(SWEEP_INTERVAL) => {}
        }
        let removed = ctx
            .chat_store
            .prune_expired(ctx.config.edit_message_ttl)
            .await;
        let pruned = ctx.link_cache.prune(ctx.config.link_cache_ttl).await;
        if pruned > 0 {
            log::info!("link cache: pruned {pruned} expired entr(ies)");
        }
        let idle_limiters = crate::rate_limit::prune_idle();
        if idle_limiters > 0 {
            log::debug!("rate limiter: dropped {idle_limiters} idle bucket(s)");
        }
        // Entries past Telegram's own inline cache window: a repeat is sent to
        // the bot again anyway, so keeping them would suppress a fetch the user
        // is waiting for (and the map grew one entry per user, forever).
        let idle_inline = handlers::prune_idle_states();
        if idle_inline > 0 {
            log::debug!("inline queries: dropped {idle_inline} idle entry(ies)");
        }
        // Only speaks up when the queue is not empty: a healthy bot has nothing
        // to report, and a periodic "0 pending" line is noise that hides the
        // lines that matter.
        if let Some((pending, oldest_run_after)) = ctx.task_queue.pending_backlog().await {
            let overdue = crate::db::now_f64() - oldest_run_after;
            if overdue >= 0.0 {
                log::info!("queue: {pending} pending task(s), oldest {overdue:.0}s overdue");
            } else {
                log::info!(
                    "queue: {pending} pending task(s), oldest retry in {:.0}s",
                    -overdue
                );
            }
        }
        for (chat_id, prompt_message_id) in removed {
            // Rewritten in place, not announced: the sweep is a background
            // timer, and a fresh message would wake the chat up to a full TTL
            // later about a prompt the user already walked away from. The edit
            // drops the buttons too. If the prompt was already deleted this
            // fails with a 400 "message to edit not found" — log and ignore.
            if let Err(e) = ctx
                .sender
                .edit_message_text(
                    ChatId(chat_id),
                    MessageId(prompt_message_id as i32),
                    send::EDIT_PROMPT_EXPIRED_TEXT.to_string(),
                )
                .await
            {
                log::info!("edit-expiry sweep: prompt message gone: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweep_removes_only_our_old_temp_entries() {
        let dir = tempfile::tempdir().unwrap();
        let old = std::time::SystemTime::now() - Duration::from_secs(7200);
        let make = |name: &str, aged: bool| {
            let path = dir.path().join(name);
            std::fs::write(&path, b"x").unwrap();
            if aged {
                let file = std::fs::File::options().write(true).open(&path).unwrap();
                file.set_modified(old).unwrap();
            }
            path
        };
        let ours_old = make(&format!("{}photo-old.jpg", x_media::TEMP_FILE_PREFIX), true);
        let ours_fresh = make(
            &format!("{}photo-new.jpg", x_media::TEMP_FILE_PREFIX),
            false,
        );
        let theirs = make("someone-elses-file", true);

        assert_eq!(sweep_temp_dir(dir.path(), Duration::from_secs(3600)), 1);
        assert!(!ours_old.exists(), "an old leftover of ours is removed");
        assert!(ours_fresh.exists(), "a fresh file may belong to a live run");
        assert!(
            theirs.exists(),
            "files without our prefix are never touched"
        );

        // A caller with no age gate also reaches the directory branch (aging a
        // *directory* is not portable, so the gate is what the first half
        // above proves): the fresh dir and file go, the unrelated file stays.
        let leftover_dir = dir
            .path()
            .join(format!("{}ugoira", x_media::TEMP_FILE_PREFIX));
        std::fs::create_dir(&leftover_dir).unwrap();
        std::fs::write(leftover_dir.join("frame.png"), b"x").unwrap();
        assert_eq!(sweep_temp_dir(dir.path(), Duration::ZERO), 2);
        assert!(
            !leftover_dir.exists(),
            "leftover dirs go with their contents"
        );
        assert!(!ours_fresh.exists(), "no age gate: ours, however fresh");
        assert!(theirs.exists());
    }

    /// The sweep's tick: an expired prompt is rewritten in place (buttons
    /// dropped) while a live one is left alone. Driven through the loop's own
    /// timer on a paused clock — the loop is what a hand-called helper would
    /// leave untested, which is how the queue's sweep kept a missing wake-up.
    #[tokio::test(start_paused = true)]
    async fn the_sweep_expires_only_the_prompts_past_their_ttl() {
        use crate::ctx::test_support::{
            FORWARDED_ID, PROMPT_ID, TestStores, api_error, seed_prompt,
        };
        use crate::media_sender::test_support::MockSender;
        use crate::state::EditMessage;

        // The interval is pinned here because no assertion on the edits can see
        // it: a shorter interval produces the same single edit (the record is
        // gone after the first tick), and the paused clock can jump past the
        // boundary while a tick's DB work is in flight.
        assert_eq!(SWEEP_INTERVAL, Duration::from_secs(300));

        let config = crate::config::Config::load();
        let stores = TestStores::new();
        let sender = MockSender::scripted(vec![], || {
            api_error("Bad Request: message to edit not found")
        });
        let ctx = stores.ctx(&sender);
        // Chat 1 holds a prompt past its ttl; chat 2 a live one.
        let stale = crate::db::unix_now() - config.edit_message_ttl.as_secs() as i64 - 1;
        seed_prompt(&ctx, "", stale).await;
        stores
            .chat_store()
            .update(2, |data| {
                data.edit_message.insert(
                    PROMPT_ID,
                    EditMessage {
                        url: "https://x.com/u/status/1".into(),
                        chat_id: 2,
                        forward_message_ids: vec![FORWARDED_ID],
                        template: String::new(),
                        created_at: crate::db::unix_now(),
                    },
                );
            })
            .await
            .unwrap();

        let (stop_tx, stop_rx) = watch::channel(false);
        let sweep = periodic_sweep(stores.ctx(&sender), stop_rx);
        tokio::pin!(sweep);

        // One second short of the interval: nothing has been touched. The
        // select is what polls the loop (a pinned future nobody awaits never
        // runs), and the paused clock makes this the loop's own timer.
        tokio::select! {
            _ = &mut sweep => unreachable!("the sweep only returns on stop"),
            _ = tokio::time::sleep(SWEEP_INTERVAL - Duration::from_secs(1)) => {}
        }
        assert!(
            sender.edited_texts().is_empty(),
            "the sweep ran before its interval"
        );

        // The second that crosses the interval: the tick fires.
        tokio::select! {
            _ = &mut sweep => unreachable!("the sweep only returns on stop"),
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
        }

        assert_eq!(
            sender.edited_texts(),
            vec![(1, PROMPT_ID, send::EDIT_PROMPT_EXPIRED_TEXT.to_string())],
            "exactly the expired prompt, rewritten in place"
        );
        assert!(
            !ctx.chat_store
                .get(1)
                .await
                .edit_message
                .contains_key(&PROMPT_ID),
            "the expired record is gone"
        );
        assert!(
            ctx.chat_store
                .get(2)
                .await
                .edit_message
                .contains_key(&PROMPT_ID),
            "a live prompt keeps its record and its buttons"
        );

        stop_tx.send(true).unwrap();
        sweep.await;
    }
}
