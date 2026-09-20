use dotenv::dotenv;
use teloxide::dptree::endpoint;
use teloxide::prelude::*;
use teloxide::stop::StopToken;
use teloxide::types::{ChatId, InlineKeyboardMarkup, InputFile, MessageId};
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
use handlers::{CHAT_STORE, CONFIG, LINK_CACHE, TASK_QUEUE};

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

#[tokio::main]
async fn main() {
    dotenv().ok();
    pretty_env_logger::init();
    log::info!("Starting bot");

    let bot = Bot::from_env();
    // Force the queue workers' shared Bot to initialize now so a missing
    // token fails at startup, not on the first queued task.
    let _ = &*send::BOT;

    // Register the command list with Telegram (client `/` menu).
    if let Err(e) = handlers::register_commands(&bot).await {
        log::warn!("failed to register commands: {e}");
    }

    log::info!(
        "config: {} admin(s), edit-message TTL {}s",
        CONFIG.admin_ids.len(),
        CONFIG.edit_message_ttl.as_secs()
    );

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

    // Edit-expiry sweep: clears the prompt's buttons once the record expires.
    log::info!(
        "edit-expiry sweep: every 300s, ttl {}",
        CONFIG.edit_message_ttl.as_secs()
    );
    let (stop_tx, stop_rx) = watch::channel(false);
    {
        let bot = bot.clone();
        let mut stop_rx = stop_rx;
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stop_rx.changed() => break,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(300)) => {}
                }
                let ttl = CONFIG.edit_message_ttl;
                let removed = CHAT_STORE.prune_expired(ttl).await;
                let pruned = LINK_CACHE.prune(CONFIG.link_cache_ttl).await;
                if pruned > 0 {
                    log::info!("link cache: pruned {pruned} expired entr(ies)");
                }
                let idle_limiters = crate::rate_limit::prune_idle();
                if idle_limiters > 0 {
                    log::debug!("rate limiter: dropped {idle_limiters} idle bucket(s)");
                }
                for (chat_id, prompt_message_id) in removed {
                    // Rewritten in place, not announced: the sweep is a
                    // background timer, and a fresh message would wake the chat
                    // up to a full TTL later about a prompt the user already
                    // walked away from. The edit drops the buttons too. If the
                    // prompt was already deleted this fails with a 400
                    // "message to edit not found" — log and ignore.
                    if let Err(e) = bot
                        .edit_message_text(
                            ChatId(chat_id),
                            MessageId(prompt_message_id as i32),
                            send::EDIT_PROMPT_EXPIRED_TEXT,
                        )
                        .reply_markup(InlineKeyboardMarkup::default())
                        .await
                    {
                        log::info!("edit-expiry sweep: prompt message gone: {e}");
                    }
                }
            }
        });
    }

    let handler = dptree::entry()
        .branch(Update::filter_message().branch(endpoint(handlers::message_handler)))
        .branch(Update::filter_inline_query().branch(endpoint(handlers::inline_query_handler)))
        .branch(Update::filter_callback_query().branch(endpoint(handlers::callback_query_handler)));

    let mut dispatcher = Dispatcher::builder(bot.clone(), handler)
        .dependencies(dptree::deps![""])
        .enable_ctrlc_handler()
        .build();

    if CONFIG.webhook_enabled {
        log::info!("running in webhook mode");
        let url = CONFIG.webhook_url.clone().expect("WEBHOOK_URL is not set");
        // `webhooks::axum` calls set_webhook itself (with the full options,
        // secret token included) — no explicit registration here.
        let listen = CONFIG.webhook_listen.expect("WEBHOOK_LISTEN is not set");
        let port = CONFIG.webhook_port.expect("WEBHOOK_PORT is not set");
        let mut options = webhooks::Options::new((listen, port).into(), url);
        if let Some(cert) = &CONFIG.webhook_cert {
            options = options.certificate(InputFile::file(cert));
        }
        if let Some(secret) = &CONFIG.webhook_secret_token {
            options = options.secret_token(secret.clone());
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

    // Graceful stop (Ctrl+C / SIGTERM): stop the sweep, notify the admin,
    // drain the queue. Bounded: a worker mid-download (30 s timeout) or a
    // long ugoira encode must not hold the shutdown hostage forever.
    log::info!("Stopping bot");
    const SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
    let shutdown = async {
        let _ = stop_tx.send(true);
        handlers::stop_url_workers().await;
        if let Some(admin) = CONFIG.admin_ids.first() {
            let _ = bot.send_message(ChatId(*admin), "Shutting down...").await;
        }
        TASK_QUEUE.stop().await;
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
