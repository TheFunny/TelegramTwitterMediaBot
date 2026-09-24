//! Callback query handling: the edit-before-forward prompt's `"forward"` and
//! `"template|<name>"` buttons.
//!
//! [`callback_query_handler`] is the dptree entry; it only pulls the plain
//! values out of the teloxide update and hands them to [`handle_callback`],
//! which holds the button logic and is driven directly by tests.

use crate::ctx::AppContext;
use crate::db::unix_now;
use crate::send::{self, Task};
use teloxide::RequestError;
use teloxide::prelude::*;
use teloxide::types::{CallbackQuery, CallbackQueryId, MessageId};

/// The `"forward"` button's data.
const FORWARD: &str = "forward";
/// The `"skip"` button's data: drop the prompt without forwarding.
const SKIP: &str = "skip";
/// Prefix of a template button's data: `"template|<name>"`.
const TEMPLATE_PREFIX: &str = "template|";

pub async fn callback_query_handler(bot: Bot, query: CallbackQuery) -> Result<(), RequestError> {
    let Some(message) = &query.message else {
        return respond(());
    };
    let Some(data) = query.data.clone() else {
        return respond(());
    };
    let ctx = AppContext::from_statics(&bot);
    handle_callback(
        &ctx,
        query.id.clone(),
        message.chat().id.0,
        message.id().0 as i64,
        &data,
    )
    .await;
    respond(())
}

/// Handles one button press on the edit-before-forward prompt.
async fn handle_callback(
    ctx: &AppContext<'_>,
    callback_query_id: CallbackQueryId,
    chat_id: i64,
    prompt_message_id: i64,
    data: &str,
) {
    let ttl_secs = ctx.config.edit_message_ttl.as_secs() as i64;
    let chat_data = ctx.chat_store.get(chat_id).await;
    let edit = chat_data.edit_message.get(&prompt_message_id).cloned();
    let Some(edit) = edit else {
        log::debug!("callback from {chat_id}: no edit record for prompt {prompt_message_id}");
        let _ = ctx
            .sender
            .answer_callback_query(callback_query_id, Some("Expired".to_string()))
            .await;
        return;
    };
    // Lazy expiry: a stale record (past the TTL, not yet swept) is dropped.
    if edit.created_at + ttl_secs <= unix_now() {
        ctx.chat_store
            .update(chat_id, |data| {
                data.edit_message.remove(&prompt_message_id);
            })
            .await;
        let _ = ctx
            .sender
            .answer_callback_query(callback_query_id, Some("Expired".to_string()))
            .await;
        return;
    }

    log::info!(
        "callback from {chat_id} on prompt {prompt_message_id}: {}",
        super::log_escape(data)
    );
    if data == SKIP {
        // Skip works with or without a forward channel: it is the explicit
        // "do not forward this" answer, and it drops the record so the forward
        // can never happen later.
        log::info!("edit-before-forward prompt {prompt_message_id} skipped");
        ctx.chat_store
            .update(chat_id, |data| {
                data.edit_message.remove(&prompt_message_id);
            })
            .await;
        let _ = ctx
            .sender
            .delete_message(ChatId(chat_id), MessageId(prompt_message_id as i32))
            .await;
        let _ = ctx
            .sender
            .answer_callback_query(
                callback_query_id,
                Some("Skipped — nothing was forwarded.".to_string()),
            )
            .await;
        return;
    }
    if data == FORWARD {
        match chat_data.forward_channel_id {
            Some(channel_id) => {
                let forward_task = Task::ForwardMessages {
                    from_chat_id: edit.chat_id,
                    to_chat_id: channel_id,
                    message_ids: edit.forward_message_ids.clone(),
                    notify_chat_id: Some(chat_id),
                    notify_message_id: Some(prompt_message_id),
                };
                let (answer, settled) = match send::forward_messages(ctx, &forward_task).await {
                    Ok(()) => {
                        log::info!(
                            "forwarded {} message(s) to channel {channel_id}",
                            edit.forward_message_ids.len()
                        );
                        ("✅ Forwarded".to_string(), true)
                    }
                    Err(send::SendError::Retryable {
                        delay_seconds,
                        task,
                    }) => {
                        // The queued row owns the forward from here (it carries
                        // the message ids itself), so the prompt is settled
                        // either way: leaving it live let a second Confirm copy
                        // the same messages to the channel twice, and let Skip
                        // answer "nothing was forwarded" while the row still
                        // delivered it.
                        let queued =
                            send::enqueue_retry(ctx.task_queue, &task, delay_seconds).await;
                        if queued {
                            log::info!("forward queued for retry in {delay_seconds:.1}s");
                            ("Forward queued for retry.".to_string(), true)
                        } else {
                            log::error!("forward retry could not be queued");
                            (
                                "Forward failed and the retry could not be queued.".to_string(),
                                true,
                            )
                        }
                    }
                    Err(send::SendError::Permanent { message, .. }) => {
                        log::error!("forward failed permanently: {message}");
                        (format!("Forward failed: {message}"), false)
                    }
                };
                if settled {
                    // The prompt is done: drop it and its record.
                    let _ = ctx
                        .sender
                        .delete_message(ChatId(chat_id), MessageId(prompt_message_id as i32))
                        .await;
                    ctx.chat_store
                        .update(chat_id, |data| {
                            data.edit_message.remove(&prompt_message_id);
                        })
                        .await;
                }
                let _ = ctx
                    .sender
                    .answer_callback_query(callback_query_id, Some(answer))
                    .await;
            }
            None => {
                log::debug!("forward callback without a forward channel set");
                let _ = ctx
                    .sender
                    .answer_callback_query(
                        callback_query_id,
                        Some("No forward channel set.".to_string()),
                    )
                    .await;
            }
        }
        return;
    }

    if let Some(name) = data.strip_prefix(TEMPLATE_PREFIX) {
        let mut answer = None;
        if let Some(template_html) = chat_data.template.get(name).cloned()
            && let Some(first_forward_id) = edit.forward_message_ids.first().copied()
        {
            // Raw template including the [] placeholder (Python parity).
            match super::apply_caption_edit(
                ctx.sender,
                ChatId(chat_id),
                MessageId(first_forward_id as i32),
                template_html,
            )
            .await
            {
                super::EditOutcome::Applied => {
                    ctx.chat_store
                        .update(chat_id, |data| {
                            if let Some(entry) = data.edit_message.get_mut(&prompt_message_id) {
                                entry.template = name.to_string();
                            }
                        })
                        .await;
                    log::info!("template '{name}' applied to prompt {prompt_message_id}");
                }
                // Nothing was applied, so nothing is recorded either: the
                // prompt keeps rendering through whatever it used before, and
                // the toast says why (a silently "successful" press left the
                // caption unchanged).
                super::EditOutcome::Failed(reason) => {
                    log::error!("template '{name}' could not be applied: {reason}");
                    answer = Some(format!("Could not apply the template: {reason}"));
                }
            }
        }
        let _ = ctx
            .sender
            .answer_callback_query(callback_query_id, answer)
            .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::test_support::{FORWARDED_ID, PROMPT_ID, TestStores, api_error, seed_prompt};
    use crate::media_sender::test_support::{MockSender, Outcome};

    /// The Telegram wording the mocks answer with: a chat the bot cannot reach.
    const API_ERROR: &str = "Bad Request: chat not found";

    fn callback_id() -> CallbackQueryId {
        CallbackQueryId("cb-1".to_string())
    }

    #[tokio::test]
    async fn template_button_swaps_the_caption_and_records_the_choice() {
        let sender = MockSender::scripted(vec![Outcome::EditOk], || api_error(API_ERROR));
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        seed_prompt(&ctx, "", crate::db::unix_now()).await;

        handle_callback(&ctx, callback_id(), 1, PROMPT_ID, "template|tpl").await;

        assert_eq!(
            sender.calls(),
            vec!["edit_message_caption", "answer_callback_query"]
        );
        // The raw template, including the [] the user edits into.
        assert_eq!(sender.captions(), vec!["<b>[]</b>"]);
        assert_eq!(sender.answers(), vec![None]);
        let data = ctx.chat_store.get(1).await;
        assert_eq!(data.edit_message[&PROMPT_ID].template, "tpl");
    }

    #[tokio::test]
    async fn a_failed_template_swap_is_reported_in_the_toast() {
        let sender = MockSender::scripted(vec![Outcome::EditErr], || api_error(API_ERROR));
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        seed_prompt(&ctx, "", crate::db::unix_now()).await;

        handle_callback(&ctx, callback_id(), 1, PROMPT_ID, "template|tpl").await;

        // The caption never changed, so the toast says so and the record does
        // not claim the template was applied.
        let toast = sender.answers().last().cloned().flatten();
        assert!(
            toast
                .as_deref()
                .is_some_and(|t| t.contains("Could not apply the template")),
            "{toast:?}"
        );
        assert_eq!(
            ctx.chat_store.get(1).await.edit_message[&PROMPT_ID].template,
            ""
        );
    }

    #[tokio::test]
    async fn forward_button_copies_then_clears_the_prompt() {
        let sender = MockSender::scripted(vec![Outcome::CopyOk], || api_error(API_ERROR));
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        seed_prompt(&ctx, "", crate::db::unix_now()).await;

        handle_callback(&ctx, callback_id(), 1, PROMPT_ID, "forward").await;

        assert_eq!(
            sender.calls(),
            vec!["copy_messages", "delete_message", "answer_callback_query"]
        );
        assert_eq!(sender.answers(), vec![Some("✅ Forwarded".to_string())]);
        assert!(
            ctx.chat_store.get(1).await.edit_message.is_empty(),
            "a settled prompt must drop its record"
        );
    }

    #[tokio::test]
    async fn skip_drops_the_prompt_without_forwarding() {
        // "skip" needs no forward channel and no scripted outcomes: it deletes
        // the prompt and drops the record, so no forward can ever happen.
        let sender = MockSender::scripted(vec![], || api_error(API_ERROR));
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        seed_prompt(&ctx, "", crate::db::unix_now()).await;

        handle_callback(&ctx, callback_id(), 1, PROMPT_ID, "skip").await;

        assert_eq!(
            sender.calls(),
            vec!["delete_message", "answer_callback_query"]
        );
        assert_eq!(
            sender.answers(),
            vec![Some("Skipped — nothing was forwarded.".to_string())]
        );
        assert!(
            ctx.chat_store.get(1).await.edit_message.is_empty(),
            "a skipped prompt must drop its record"
        );
    }

    /// The whole callback path against a stand-in API through a real `Bot`:
    /// copy, delete, toast, carrying the ids the prompt held. The scripted
    /// mock records that a call happened; this records what the API received.
    #[tokio::test]
    async fn the_forward_button_talks_to_the_api_through_a_real_bot() {
        use crate::media_sender::test_support::fake_api::FakeApi;
        use teloxide::Bot;

        let api = FakeApi::start().await;
        let bot = Bot::new("42:TEST").set_api_url(api.url());
        let stores = TestStores::new();
        let ctx = stores.ctx(&bot);
        seed_prompt(&ctx, "", crate::db::unix_now()).await;

        handle_callback(&ctx, callback_id(), 1, PROMPT_ID, "forward").await;

        assert_eq!(
            api.methods(),
            vec!["CopyMessages", "DeleteMessage", "AnswerCallbackQuery"]
        );
        let copy = api.body("CopyMessages");
        assert_eq!(copy["chat_id"], 2, "the prompt's channel");
        assert_eq!(copy["from_chat_id"], 1);
        assert_eq!(copy["message_ids"], serde_json::json!([FORWARDED_ID]));
        assert_eq!(api.body("AnswerCallbackQuery")["text"], "✅ Forwarded");
    }

    #[tokio::test]
    async fn forward_without_a_channel_is_reported() {
        let sender = MockSender::scripted(vec![], || api_error(API_ERROR));
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        seed_prompt(&ctx, "", crate::db::unix_now()).await;
        ctx.chat_store
            .update(1, |data| data.forward_channel_id = None)
            .await;

        handle_callback(&ctx, callback_id(), 1, PROMPT_ID, "forward").await;

        assert_eq!(sender.calls(), vec!["answer_callback_query"]);
        assert_eq!(
            sender.answers(),
            vec![Some("No forward channel set.".to_string())]
        );
    }

    #[tokio::test]
    async fn retryable_forward_is_queued_and_settles_the_prompt() {
        use teloxide::types::Seconds;
        let sender = MockSender::scripted(vec![Outcome::CopyErr], || {
            RequestError::RetryAfter(Seconds::from_seconds(7))
        });
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);
        seed_prompt(&ctx, "", crate::db::unix_now()).await;

        handle_callback(&ctx, callback_id(), 1, PROMPT_ID, "forward").await;

        // The queued row carries the message ids itself, so it owns the
        // forward from here and the prompt is closed with it. Keeping it live
        // (the old behaviour) let a second Confirm copy the same messages to
        // the channel twice, and let Skip answer "nothing was forwarded" while
        // the row still delivered it.
        assert_eq!(
            sender.calls(),
            vec!["copy_messages", "delete_message", "answer_callback_query"]
        );
        assert_eq!(
            sender.answers(),
            vec![Some("Forward queued for retry.".to_string())]
        );
        assert_eq!(stores.queued_tasks().await, 1);
        assert!(
            !ctx.chat_store
                .get(1)
                .await
                .edit_message
                .contains_key(&PROMPT_ID),
            "the record must be dropped so the prompt cannot be used again"
        );

        // A second tap finds no record: it cannot enqueue a duplicate copy.
        handle_callback(&ctx, callback_id(), 1, PROMPT_ID, "forward").await;
        assert_eq!(
            sender.calls(),
            vec![
                "copy_messages",
                "delete_message",
                "answer_callback_query",
                "answer_callback_query"
            ]
        );
        assert_eq!(
            sender.answers().last().map(|a| a.as_deref()),
            Some(Some("Expired"))
        );
        assert_eq!(stores.queued_tasks().await, 1, "no second forward row");
    }

    #[tokio::test]
    async fn unknown_and_expired_prompts_answer_expired() {
        let sender = MockSender::scripted(vec![], || api_error(API_ERROR));
        let stores = TestStores::new();
        let ctx = stores.ctx(&sender);

        // No record at all.
        handle_callback(&ctx, callback_id(), 1, PROMPT_ID, "forward").await;
        assert_eq!(sender.answers(), vec![Some("Expired".to_string())]);

        // A record past its TTL (nothing swept it yet) is dropped on use.
        let stale = crate::db::unix_now() - ctx.config.edit_message_ttl.as_secs() as i64 - 1;
        seed_prompt(&ctx, "", stale).await;
        handle_callback(&ctx, callback_id(), 1, PROMPT_ID, "forward").await;
        assert_eq!(
            sender.answers(),
            vec![Some("Expired".to_string()), Some("Expired".to_string())]
        );
        assert!(
            ctx.chat_store.get(1).await.edit_message.is_empty(),
            "the expired record must be dropped"
        );
        assert_eq!(sender.calls(), vec!["answer_callback_query"; 2]);
    }
}
