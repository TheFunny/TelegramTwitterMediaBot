use dotenv::dotenv;
use teloxide::dptree::endpoint;
use teloxide::types::{ChatId, InputFile, MessageId};
use teloxide::update_listeners::webhooks;
use teloxide::prelude::*;
use tokio::sync::watch;
use x_media::site;

mod config;
mod handlers;
mod queue;
mod send;
mod state;

use handlers::{CHAT_STORE, CONFIG, TASK_QUEUE};

#[tokio::main]
async fn main() {
    dotenv().ok();
    pretty_env_logger::init();
    log::info!("Starting bot");

    let bot = Bot::from_env();

    log::info!(
        "config: {} admin(s), edit-message TTL {}s",
        CONFIG.admin_ids.len(),
        CONFIG.edit_message_ttl.as_secs()
    );

    // Queue worker: handles typed tasks, dead-letters failed sends to the
    // task's chat.
    TASK_QUEUE
        .start(send::handle_task, send::dead_letter_notify)
        .await;
    log::info!("task queue worker started");

    // Pixiv login validation (user request): a failed login notifies the
    // admin and disables pixiv for this process.
    if site::pixiv::enabled() {
        match site::pixiv::validate().await {
            Ok(()) => log::info!("pixiv login validated"),
            Err(e) => {
                log::error!("pixiv login failed: {e}");
                if let Some(admin) = CONFIG.admin_ids.first() {
                    let _ = bot
                        .send_message(ChatId(*admin), format!("Pixiv login failed: {e}"))
                        .await;
                }
                site::pixiv::disable();
            }
        }
    }

    // Edit-expiry sweep: clears the prompt's buttons once the record expires.
    log::info!("edit-expiry sweep: every 300s, ttl {}", CONFIG.edit_message_ttl.as_secs());
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
                for (chat_id, prompt_message_id) in removed {
                    // If the prompt was already deleted, this fails with a
                    // 400 "message to edit not found" — log and ignore.
                    if let Err(e) = bot
                        .edit_message_reply_markup(ChatId(chat_id), MessageId(prompt_message_id as i32))
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
        let url = CONFIG
            .webhook_url
            .clone()
            .expect("WEBHOOK_URL is not set");
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

        dispatcher
            .dispatch_with_listener(
                webhooks::axum(bot.clone(), options)
                    .await
                    .expect("Failed to create webhook listener"),
                LoggingErrorHandler::with_custom_text("Error from update listener"),
            )
            .await;
    } else {
        log::info!("running in polling mode");
        dispatcher.dispatch().await;
    }

    // Graceful stop (Ctrl+C): stop the sweep, notify the admin, drain the queue.
    log::info!("Stopping bot");
    let _ = stop_tx.send(true);
    if let Some(admin) = CONFIG.admin_ids.first() {
        let _ = bot.send_message(ChatId(*admin), "Shutting down...").await;
    }
    TASK_QUEUE.stop().await;
    log::info!("Bot stopped");
}
