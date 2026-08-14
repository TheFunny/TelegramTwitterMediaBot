//! Callback query handling: the edit-before-forward prompt's "forward" and
//! "template|<name>" buttons.

use super::urls::enqueue_retry;
use super::{CHAT_STORE, CONFIG, TASK_QUEUE};
use crate::send::{self, Task};
use crate::state::unix_now;
use teloxide::RequestError;
use teloxide::prelude::*;
use teloxide::types::{CallbackQuery, ChatId, MessageId, ParseMode};

pub async fn callback_query_handler(bot: Bot, query: CallbackQuery) -> Result<(), RequestError> {
    let callback_query_id = query.id;
    let data = query.data.clone();
    let Some(message) = &query.message else {
        return respond(());
    };
    let chat_id = message.chat().id.0;
    let prompt_message_id = message.id().0 as i64;
    let ttl_secs = CONFIG.edit_message_ttl.as_secs() as i64;
    let chat_data = CHAT_STORE.get(chat_id).await;
    let edit = chat_data.edit_message.get(&prompt_message_id).cloned();
    let Some(edit) = edit else {
        log::debug!(
            "callback from {}: no edit record for prompt {prompt_message_id}",
            chat_id
        );
        bot.answer_callback_query(callback_query_id)
            .text("Expired")
            .await?;
        return respond(());
    };
    // Lazy expiry: a stale record (past the TTL, not yet swept) is dropped.
    if edit.created_at + ttl_secs <= unix_now() {
        CHAT_STORE
            .update(chat_id, |data| {
                data.edit_message.remove(&prompt_message_id);
            })
            .await;
        bot.answer_callback_query(callback_query_id)
            .text("Expired")
            .await?;
        return respond(());
    }

    let Some(data) = data else {
        return respond(());
    };
    log::info!(
        "callback from {} on prompt {prompt_message_id}: {data}",
        chat_id
    );
    if data == "forward" {
        match chat_data.forward_channel_id {
            Some(channel_id) => {
                let forward_task = Task::ForwardMessages {
                    from_chat_id: edit.chat_id,
                    to_chat_id: channel_id,
                    message_ids: edit.forward_message_ids.clone(),
                    notify_chat_id: Some(chat_id),
                    notify_message_id: Some(prompt_message_id),
                };
                match send::forward_messages(&bot, &forward_task).await {
                    Ok(()) => {
                        log::info!(
                            "forwarded {} message(s) to channel {channel_id}",
                            edit.forward_message_ids.len()
                        );
                        bot.answer_callback_query(callback_query_id)
                            .text("✅ Forwarded")
                            .await?;
                        let _ = bot
                            .delete_message(ChatId(chat_id), MessageId(prompt_message_id as i32))
                            .await;
                        CHAT_STORE
                            .update(chat_id, |data| {
                                data.edit_message.remove(&prompt_message_id);
                            })
                            .await;
                    }
                    Err(send::SendError::Retryable {
                        delay_seconds,
                        task,
                    }) => {
                        log::info!("forward queued for retry in {delay_seconds:.1}s");
                        enqueue_retry(&TASK_QUEUE, task, delay_seconds).await;
                        bot.answer_callback_query(callback_query_id)
                            .text("Forward queued for retry.")
                            .await?;
                    }
                    Err(send::SendError::Permanent { message, .. }) => {
                        log::error!("forward failed permanently: {message}");
                        bot.answer_callback_query(callback_query_id)
                            .text(format!("Forward failed: {message}"))
                            .await?;
                    }
                }
            }
            None => {
                log::debug!("forward callback without a forward channel set");
                bot.answer_callback_query(callback_query_id)
                    .text("No forward channel set.")
                    .await?;
            }
        }
        return respond(());
    }
    if let Some(name) = data.strip_prefix("template|") {
        if let Some(template_html) = chat_data.template.get(name).cloned()
            && let Some(first_forward_id) = edit.forward_message_ids.first().copied()
        {
            // Raw template including the [] placeholder (Python parity).
            let _ = bot
                .edit_message_caption(ChatId(chat_id), MessageId(first_forward_id as i32))
                .caption(template_html)
                .parse_mode(ParseMode::Html)
                .await;
            CHAT_STORE
                .update(chat_id, |data| {
                    if let Some(entry) = data.edit_message.get_mut(&prompt_message_id) {
                        entry.template = name.to_string();
                    }
                })
                .await;
            log::info!("template '{name}' applied to prompt {prompt_message_id}");
        }
        bot.answer_callback_query(callback_query_id).await?;
    }
    respond(())
}
