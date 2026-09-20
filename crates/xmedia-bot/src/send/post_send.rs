//! Everything around a send: the link-cache write that follows one, the
//! keep-alive registry for locally produced media, task settlement, the
//! post-send actions (edit prompt / channel forward) and the queue entry
//! points.

use super::{SendError, Task, forward_messages, send_animation, send_media_sequence};
use crate::ctx::AppContext;
use crate::db::{now_f64, unix_now};
use crate::handlers::log_key;
use crate::link_cache::{CachedMedia, CachedMediaKind, LinkCache};
use crate::media_sender::MediaSender;
use crate::queue::{PersistentTaskQueue, QueueError};
use crate::state::EditMessage;
use std::collections::HashMap;
use std::sync::LazyLock;
use teloxide::types::{ChatId, InlineKeyboardButton, InlineKeyboardMarkup, Message, MessageId};

/// Persists a successful send under the post's cache key. Only runs for a
/// fresh (non-resumed) task that carried raw cache data with no file ids yet.
pub(super) async fn cache_sent_task(ctx: &AppContext<'_>, task: &Task, media: Vec<CachedMedia>) {
    let Some(cache_data) = task.cache_data() else {
        return;
    };
    if !cache_data.media.is_empty() || media.is_empty() {
        return;
    }
    let mut post = cache_data.clone();
    post.media = media;
    if let Some(key) = x_media::site::cache_key(&post.url) {
        ctx.link_cache.put(&key, &post).await;
        log::debug!("cached send for [key={}]", log_key(&post.url));
    }
}

/// Persists a lone animation send under the post's cache key.
pub(super) async fn cache_animation_send(ctx: &AppContext<'_>, task: &Task, message: &Message) {
    if let Some(file_id) = message.animation().map(|a| a.file.id.to_string()) {
        cache_sent_task(
            ctx,
            task,
            vec![CachedMedia {
                kind: CachedMediaKind::Animation,
                file_id,
            }],
        )
        .await;
    }
}

/// How a task ended. The two states differ only in whether a link-cache entry
/// may still be holding the (now unusable) media.
pub(crate) enum Settled {
    Sent,
    Failed,
}

/// Every path that ends a task's life — sent, permanently failed, or
/// dead-lettered after the last retry — funnels through here, so the cleanup a
/// settled task owes cannot be forgotten by a new path: release the keep-alive
/// temp media (retryable tasks keep it, they will be resent) and drop the
/// link-cache entry that a failed send's stale file ids would keep poisoning.
pub(crate) async fn settle_task(ctx: &AppContext<'_>, task: &Task, outcome: Settled) {
    if matches!(outcome, Settled::Failed) {
        invalidate_cache(ctx.link_cache, task).await;
    }
    release_keep_alive(task);
}

/// A cached Telegram file id failed permanently (stale/expired); drop the
/// cache entry so the next request re-fetches instead of repeating it.
async fn invalidate_cache(cache: &LinkCache, task: &Task) {
    if task.is_cached_send()
        && let Some(url) = task.source_url()
        && let Some(key) = x_media::site::cache_key(url)
    {
        log::debug!("removing stale link cache entry for [key={}]", log_key(url));
        cache.remove(&key).await;
    }
}

/// Locally produced media files (ugoira MP4, bsky remux MP4) whose temp dirs
/// must stay alive while their task may be retried by the queue. The fetch
/// pipeline hands ownership here via
/// [`x_media::site::Fetched::take_keep_alive`] before that
/// [`x_media::site::Fetched`] is dropped; a queued retry runs after that drop,
/// so without this the local file would be gone by the time the retry sends
/// it. Entries are removed when the task settles (see [`release_keep_alive`]).
pub(crate) static KEEP_ALIVE: LazyLock<parking_lot::Mutex<Vec<tempfile::TempDir>>> =
    LazyLock::new(|| parking_lot::Mutex::new(Vec::new()));

/// Drops the keep-alive temp dirs holding media referenced by `task` (matched
/// by path prefix). Called once a task settles — sent or permanently failed —
/// so retry-only temp files do not leak; retryable tasks keep them alive.
pub(crate) fn release_keep_alive(task: &Task) {
    let paths = task.local_media_paths();
    if paths.is_empty() {
        return;
    }
    let mut alive = KEEP_ALIVE.lock();
    alive.retain(|dir| {
        let dir_path = dir.path();
        !paths.iter().any(|p| p.starts_with(dir_path))
    });
}

/// The edit-before-forward prompt's text. It names both controls and the TTL,
/// because the buttons alone left users waiting for a forward that never came
/// (nothing is forwarded until Confirm).
pub(super) fn edit_prompt_text(ttl: std::time::Duration) -> String {
    format!(
        "Reply to edit the caption, or tap a template, then ↩️ Confirm to forward. \
         Expires in {}. Nothing is forwarded until you confirm.",
        coarsest_unit(ttl)
    )
}

/// Text the prompt is rewritten to once its record expires. The sweep edits
/// the prompt in place (see `main`): announcing the expiry with a new message
/// would wake the chat up to a full TTL later about a prompt nobody is
/// waiting on.
pub(crate) const EDIT_PROMPT_EXPIRED_TEXT: &str = "⌛ Expired — nothing was forwarded.";

/// `24h` / `90m` / `45s`: the coarsest whole unit, so the prompt stays short.
fn coarsest_unit(ttl: std::time::Duration) -> String {
    let secs = ttl.as_secs();
    if secs >= 3600 {
        format!("{}h", secs / 3600)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{secs}s")
    }
}

/// Templates per keyboard row. Telegram rejects a keyboard with more than 100
/// buttons *outright*, which would silently drop the whole prompt, so the
/// names are folded and capped rather than listed one per row.
pub(super) const TEMPLATE_BUTTONS_PER_ROW: usize = 3;
/// Hard cap on template buttons; the prompt text names the ones not shown.
pub(super) const MAX_TEMPLATE_BUTTONS: usize = 60;

/// Template buttons ([`TEMPLATE_BUTTONS_PER_ROW`] per row, at most
/// [`MAX_TEMPLATE_BUTTONS`]), then the confirm/skip pair. Sorted by name: the
/// templates live in a `HashMap`, so an unsorted walk would reshuffle the
/// buttons between prompts.
pub(super) fn build_edit_markup(templates: &HashMap<String, String>) -> InlineKeyboardMarkup {
    let mut names: Vec<&String> = templates.keys().collect();
    names.sort();
    let shown = names.len().min(MAX_TEMPLATE_BUTTONS);
    let mut rows = Vec::with_capacity(shown / TEMPLATE_BUTTONS_PER_ROW + 2);
    for chunk in names[..shown].chunks(TEMPLATE_BUTTONS_PER_ROW) {
        rows.push(
            chunk
                .iter()
                .map(|name| {
                    InlineKeyboardButton::callback(name.as_str(), format!("template|{name}"))
                })
                .collect(),
        );
    }
    // Skip exists because the prompt holds the forward hostage until Confirm:
    // without it the only escape was deleting the message and waiting out the
    // TTL for a forward that then never happens.
    rows.push(vec![
        InlineKeyboardButton::callback("↩️ Confirm", "forward"),
        InlineKeyboardButton::callback("🛑 Skip", "skip"),
    ]);
    InlineKeyboardMarkup::new(rows)
}

/// How many templates the markup could not fit, for the prompt text.
pub(super) fn hidden_template_count(templates: &HashMap<String, String>) -> usize {
    templates.len().saturating_sub(MAX_TEMPLATE_BUTTONS)
}

/// Notifies a chat about a dead-lettered task (skips when `notify_chat_id` is
/// absent).
pub(super) async fn notify_failure(
    sender: &dyn MediaSender,
    chat_id: Option<i64>,
    message_id: Option<i64>,
    message: &str,
) {
    let Some(chat_id) = chat_id else { return };
    let reply_to = message_id.map(|id| MessageId(id as i32));
    if let Err(e) = sender
        .send_message(ChatId(chat_id), message.to_string(), reply_to, None)
        .await
    {
        log::error!("failed to notify about failed task: {e}");
    }
}

/// After a successful send: either open the edit-before-forward prompt or
/// forward to the configured channel (with retry/queue handling).
pub(crate) async fn post_send_actions(ctx: &AppContext<'_>, task: &Task, message_ids: Vec<i64>) {
    let (
        chat_id,
        reply_to,
        source_url,
        edit_before_forward,
        forward_channel_id,
        notify_chat_id,
        notify_message_id,
    ) = match task {
        Task::SendMediaSequence {
            chat_id,
            reply_to_message_id,
            source_url,
            edit_before_forward,
            forward_channel_id,
            notify_chat_id,
            notify_message_id,
            ..
        }
        | Task::SendAnimation {
            chat_id,
            reply_to_message_id,
            source_url,
            edit_before_forward,
            forward_channel_id,
            notify_chat_id,
            notify_message_id,
            ..
        } => (
            *chat_id,
            *reply_to_message_id,
            source_url.clone(),
            *edit_before_forward,
            *forward_channel_id,
            *notify_chat_id,
            *notify_message_id,
        ),
        Task::ForwardMessages { .. } => return,
    };

    if edit_before_forward {
        let templates = ctx.chat_store.get(chat_id).await.template;
        let keyboard = build_edit_markup(&templates);
        let mut text = edit_prompt_text(ctx.config.edit_message_ttl);
        let hidden = hidden_template_count(&templates);
        if hidden > 0 {
            // The keyboard is capped; say so instead of silently hiding them.
            text.push_str(&format!(
                "\n({hidden} more templates not shown — /remove_template to prune.)"
            ));
        }
        let prompt = ctx
            .sender
            .send_message(
                ChatId(chat_id),
                text,
                Some(MessageId(reply_to as i32)),
                Some(keyboard),
            )
            .await;
        match prompt {
            Ok(prompt_id) => {
                log::info!(
                    "edit-before-forward prompt {prompt_id} opened for {} message(s)",
                    message_ids.len()
                );
                let source_url = source_url.clone();
                ctx.chat_store
                    .update(chat_id, move |data| {
                        data.edit_message.insert(
                            prompt_id,
                            EditMessage {
                                url: source_url,
                                chat_id,
                                forward_message_ids: message_ids,
                                template: String::new(),
                                created_at: unix_now(),
                            },
                        );
                    })
                    .await;
            }
            Err(e) => log::error!("failed to send edit prompt: {e}"),
        }
        return;
    }

    if let Some(channel_id) = forward_channel_id {
        log::info!(
            "forwarding {} message(s) to channel {channel_id}",
            message_ids.len()
        );
        let forward_task = Task::ForwardMessages {
            from_chat_id: chat_id,
            to_chat_id: channel_id,
            message_ids,
            notify_chat_id,
            notify_message_id,
        };
        match forward_messages(ctx, &forward_task).await {
            Ok(()) => {}
            Err(SendError::Retryable {
                delay_seconds,
                task,
            }) => {
                enqueue_retry(ctx.task_queue, *task, delay_seconds).await;
            }
            Err(SendError::Permanent { message, .. }) => {
                notify_failure(
                    ctx.sender,
                    notify_chat_id,
                    notify_message_id,
                    &failure_text(None, &message),
                )
                .await;
            }
        }
    }
}

/// Enqueues a task for a later attempt (retry / forward resume). When the
/// enqueue itself fails the task can never be sent again, so its keep-alive
/// temp media is released instead of leaking until process exit.
pub(crate) async fn enqueue_retry(queue: &PersistentTaskQueue, task: Task, delay_seconds: f64) {
    let payload = serde_json::to_value(&task).expect("task serializes");
    let run_after = now_f64() + delay_seconds;
    if let Err(e) = queue.enqueue(payload, run_after).await {
        log::error!("failed to enqueue retry: {e}");
        release_keep_alive(&task);
    }
}

/// Queue entry point: parses the stored task and dispatches.
pub(crate) async fn handle_task(
    ctx: &AppContext<'_>,
    payload: serde_json::Value,
) -> Result<(), QueueError> {
    let task: Task = match serde_json::from_value(payload.clone()) {
        Ok(task) => task,
        Err(e) => {
            return Err(QueueError::Permanent {
                message: format!("invalid task payload: {e}"),
                payload,
            });
        }
    };
    match task {
        Task::SendMediaSequence { .. } | Task::SendAnimation { .. } => {
            let message_ids = match send_media_or_animation(ctx, &task).await {
                Ok(ids) => ids,
                Err(SendError::Retryable {
                    delay_seconds,
                    task,
                }) => {
                    return Err(QueueError::Retryable {
                        delay_seconds,
                        payload: serde_json::to_value(task).expect("task serializes"),
                    });
                }
                Err(SendError::Permanent { message, task }) => {
                    settle_task(ctx, &task, Settled::Failed).await;
                    return Err(QueueError::Permanent {
                        message,
                        payload: serde_json::to_value(task).expect("task serializes"),
                    });
                }
            };
            // A task only reaches the queue after a failed send, so this
            // successful run is the first time post_send_actions can fire —
            // the fresh attempt failed before it ever got here. Run it
            // unconditionally: `post_send_actions` executes once, after the
            // whole sequence (every batch) completed, so the channel forward
            // and the edit-before-forward prompt must not be lost just
            // because the send needed a retry.
            post_send_actions(ctx, &task, message_ids).await;
            settle_task(ctx, &task, Settled::Sent).await;
            Ok(())
        }
        Task::ForwardMessages { .. } => match forward_messages(ctx, &task).await {
            Ok(()) => Ok(()),
            Err(SendError::Retryable {
                delay_seconds,
                task,
            }) => Err(QueueError::Retryable {
                delay_seconds,
                payload: serde_json::to_value(task).expect("task serializes"),
            }),
            Err(SendError::Permanent { message, task }) => {
                settle_task(ctx, &task, Settled::Failed).await;
                Err(QueueError::Permanent {
                    message,
                    payload: serde_json::to_value(task).expect("task serializes"),
                })
            }
        },
    }
}

async fn send_media_or_animation(ctx: &AppContext<'_>, task: &Task) -> Result<Vec<i64>, SendError> {
    match task {
        Task::SendMediaSequence { .. } => send_media_sequence(ctx, task).await,
        Task::SendAnimation { .. } => send_animation(ctx, task).await,
        Task::ForwardMessages { .. } => unreachable!(),
    }
}

/// User-facing text for a task that will never run again: which link died and
/// why. The raw error alone left the user guessing which post it was about.
pub(super) fn failure_text(task: Option<&Task>, message: &str) -> String {
    match task.and_then(|task| task.source_url()).map(log_key) {
        Some(key) => format!("Send failed permanently for {key}: {message}"),
        // `ForwardMessages` carries no source URL: that failure is about the
        // channel copy, not about a post.
        None => format!("Forward failed permanently: {message}"),
    }
}

/// Dead-letter callback wired to the queue in main: settles the task and
/// notifies its chat.
pub(crate) async fn dead_letter_notify(
    ctx: &AppContext<'_>,
    payload: serde_json::Value,
    message: String,
) {
    // A dead-lettered task never runs again, and the queue dead-letters retry
    // exhaustion itself (the handler is not called again), so this is the only
    // place that sees the final payload.
    let task = serde_json::from_value::<Task>(payload.clone()).ok();
    if let Some(task) = &task {
        settle_task(ctx, task, Settled::Failed).await;
    }
    let notify_chat_id = payload.get("notify_chat_id").and_then(|v| v.as_i64());
    let notify_message_id = payload.get("notify_message_id").and_then(|v| v.as_i64());
    notify_failure(
        ctx.sender,
        notify_chat_id,
        notify_message_id,
        &failure_text(task.as_ref(), &message),
    )
    .await;
}
