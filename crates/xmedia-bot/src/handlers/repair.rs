//! Startup repair, run before any queue worker exists: a queued retry whose
//! local media (a ugoira MP4, a bsky remux, a downloaded temp file) did not
//! survive the restart can never succeed, because the registry that kept those
//! files alive (`send::KEEP_ALIVE`) is in memory. Those rows are re-fetched
//! from their post instead of dead-lettering the user's link.

use super::log_key;
use super::urls::{cached_snapshot, media_to_payload};
use crate::ctx::AppContext;
use crate::link_cache::CachedPost;
use crate::send::{self, Delivery, MediaItemPayload, Task};

// ── Startup repair: queued retries whose local media did not survive ───────

/// A post's fresh media plus the caption and cache snapshot that go with them:
/// what [`refetch`] hands [`apply_refresh`]. Plain data, so the rewrite below
/// can be tested without a network fetch (which cannot be faked here:
/// [`x_media::site::Fetched`] keeps a private field and is not constructible
/// outside its crate).
struct Refetched {
    caption: String,
    items: Vec<MediaItemPayload>,
    cache_data: Option<CachedPost>,
}

/// Whether a queued task should have its post re-fetched, because it still
/// wants a local file (ugoira MP4, a bsky remux, a downloaded temp file) that is
/// gone. Those files live in the system temp dir and the registry that keeps
/// them alive for the retry (`send::KEEP_ALIVE`) is in memory, so a restart
/// takes all of them — a retry that needs one can only dead-letter.
///
/// A partially delivered album is left alone: its remaining batches cannot be
/// reconciled with a fresh media list without risking a second copy of what the
/// user already received.
fn needs_refetch(task: &Task) -> bool {
    if let Task::SendMediaSequence {
        batch_index,
        sent_message_ids,
        ..
    } = task
        && (*batch_index > 0 || !sent_message_ids.is_empty())
    {
        return false;
    }
    task.local_media_paths().iter().any(|path| !path.exists())
}

/// Rebuilds the task from the fresh media, keeping its delivery envelope (chat,
/// reply, forward/edit settings, notify targets): the retry that was queued must
/// still deliver the same way, whoever asked for it.
fn apply_refresh(task: &Task, fresh: &Refetched) -> Option<Task> {
    let chat_id = task.chat_id()?;
    let (edit_before_forward, forward_channel_id) = match task {
        Task::SendMediaSequence {
            edit_before_forward,
            forward_channel_id,
            ..
        }
        | Task::SendAnimation {
            edit_before_forward,
            forward_channel_id,
            ..
        } => (*edit_before_forward, *forward_channel_id),
        Task::ForwardMessages { .. } => return None,
    };
    let reply_to_message_id = match task {
        Task::SendMediaSequence {
            reply_to_message_id,
            ..
        }
        | Task::SendAnimation {
            reply_to_message_id,
            ..
        } => *reply_to_message_id,
        Task::ForwardMessages { .. } => return None,
    };
    let (notify_chat_id, notify_message_id) = task.notify_target();
    Some(Task::from_items(
        Delivery {
            chat_id,
            reply_to_message_id,
            edit_before_forward,
            forward_channel_id,
            notify_chat_id,
            notify_message_id,
        },
        task.source_url()?.to_string(),
        fresh.caption.clone(),
        fresh.items.clone(),
        fresh.cache_data.clone(),
    ))
}

/// Fetches the post again and maps it into [`Refetched`]: the same mapping the
/// fresh-fetch path uses (per-site caption format from the chat, render fields
/// for the link-cache snapshot), so a repaired task looks like a first send.
async fn refetch(
    ctx: &AppContext<'_>,
    chat_id: i64,
    url: &str,
) -> Result<Option<Refetched>, x_media::site::FetchError> {
    let Some(fetched) = x_media::site::fetch(url).await? else {
        return Ok(None);
    };
    if fetched.media.is_empty() {
        return Ok(None);
    }
    let chat_data = ctx.chat_store.get(chat_id).await;
    let format = chat_data.format_for(fetched.site_id);
    let caption = fetched.caption_with(&format);
    let cache_data = cached_snapshot(&fetched);
    let items: Vec<MediaItemPayload> = fetched
        .media
        .iter()
        .map(|media| media_to_payload(media, fetched.sensitive))
        .collect();
    // The re-fetch may produce a fresh local file (ugoira / bsky remux): hand it
    // to the same keep-alive registry the first fetch uses.
    if let Some(dir) = fetched.keep_alive() {
        send::KEEP_ALIVE.lock().push(dir);
    }
    Ok(Some(Refetched {
        caption,
        items,
        cache_data,
    }))
}

/// Re-fetches every queued task whose local media did not survive the restart,
/// so the user's link is still delivered instead of dead-lettering on a file
/// that cannot come back. Returns how many rows were rewritten.
///
/// Startup only, before the queue workers start: no worker can lease a row while
/// this writes, which is what lets it replace payloads without the lease-token
/// guard every worker write-back carries.
pub(crate) async fn repair_lost_local_media(ctx: &AppContext<'_>) -> usize {
    let mut repaired = 0;
    for (id, payload) in ctx.task_queue.runnable_rows().await {
        let Ok(task) = serde_json::from_str::<Task>(&payload) else {
            continue;
        };
        if !needs_refetch(&task) {
            continue;
        }
        let (Some(url), Some(chat_id)) = (task.source_url().map(str::to_string), task.chat_id())
        else {
            continue;
        };
        match refetch(ctx, chat_id, &url).await {
            Ok(Some(fresh)) => {
                let Some(updated) = apply_refresh(&task, &fresh) else {
                    continue;
                };
                let updated = serde_json::to_value(&updated).expect("task serializes");
                if ctx.task_queue.replace_payload(&id, &updated).await {
                    repaired += 1;
                    log::info!(
                        "startup repair: re-fetched [key={}] for chat={chat_id} (its local media did not survive the restart)",
                        log_key(&url)
                    );
                }
            }
            // The post is gone or withheld now: the retry could not have
            // delivered anything either, so say why instead of letting it
            // dead-letter on a missing file.
            Ok(None) | Err(_) => {
                let (notify_chat_id, notify_message_id) = task.notify_target();
                log::warn!(
                    "startup repair: [key={}] for chat={chat_id} needed a re-fetch and none was possible",
                    log_key(&url)
                );
                send::notify_failure(
                    ctx.sender,
                    notify_chat_id,
                    notify_message_id,
                    &format!(
                        "{} — the media held for retry was lost when the bot restarted and the post could not be fetched again. Please send the link again.",
                        log_key(&url)
                    ),
                )
                .await;
            }
        }
    }
    repaired
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::test_support::{TestStores, permanent_error, photo_item};
    use crate::media_sender::test_support::MockSender;
    use crate::send::MediaRef;

    fn queued_task(media: &str, batch_index: usize, sent: Vec<i64>) -> Task {
        Task::SendMediaSequence {
            chat_id: 1,
            reply_to_message_id: 2,
            caption: "cap".into(),
            media_batches: vec![vec![photo_item(media, false, false)]],
            batch_index,
            sent_message_ids: sent,
            source_url: "https://x.com/u/status/1".into(),
            edit_before_forward: true,
            forward_channel_id: Some(2),
            notify_chat_id: Some(1),
            notify_message_id: Some(2),
            cache_data: None,
        }
    }

    #[test]
    fn only_tasks_missing_a_local_file_need_a_refetch() {
        // A URL send needs nothing.
        assert!(!needs_refetch(&queued_task("https://cdn/1.jpg", 0, vec![])));
        // A local path that is still there (a survived temp file) needs nothing.
        let dir = tempfile::tempdir().unwrap();
        let alive = dir.path().join("ugoira.mp4");
        std::fs::write(&alive, b"x").unwrap();
        assert!(!needs_refetch(&queued_task(
            alive.to_str().unwrap(),
            0,
            vec![]
        )));
        // A local path the restart took away does.
        assert!(needs_refetch(&queued_task(
            "/nonexistent-ugoira.mp4",
            0,
            vec![]
        )));
        // A partially delivered album is left to its own retry path.
        assert!(!needs_refetch(&queued_task(
            "/nonexistent-ugoira.mp4",
            1,
            vec![7]
        )));
        assert!(!needs_refetch(&queued_task(
            "/nonexistent-ugoira.mp4",
            0,
            vec![7]
        )));
        // A channel copy holds no media.
        assert!(!needs_refetch(&Task::ForwardMessages {
            from_chat_id: 1,
            to_chat_id: 2,
            message_ids: vec![3],
            notify_chat_id: None,
            notify_message_id: None,
        }));
    }

    #[test]
    fn apply_refresh_keeps_the_delivery_envelope() {
        let task = queued_task("/nonexistent-ugoira.mp4", 0, vec![]);
        let fresh = Refetched {
            caption: "fresh caption".into(),
            items: vec![photo_item("https://cdn/fresh.jpg", true, false)],
            cache_data: None,
        };
        match apply_refresh(&task, &fresh).expect("a repairable task") {
            Task::SendMediaSequence {
                chat_id,
                reply_to_message_id,
                caption,
                media_batches,
                batch_index,
                sent_message_ids,
                source_url,
                edit_before_forward,
                forward_channel_id,
                notify_chat_id,
                notify_message_id,
                ..
            } => {
                // Same delivery: chat, reply, forward/edit settings, notify.
                assert_eq!((chat_id, reply_to_message_id), (1, 2));
                assert!(edit_before_forward);
                assert_eq!(forward_channel_id, Some(2));
                assert_eq!((notify_chat_id, notify_message_id), (Some(1), Some(2)));
                assert_eq!(source_url, "https://x.com/u/status/1");
                // Fresh media, and nothing of it counted as sent yet.
                assert_eq!(caption, "fresh caption");
                assert!(
                    matches!(
                        media_batches[0][0].media_ref(),
                        MediaRef::Source(media) if media == "https://cdn/fresh.jpg"
                    ),
                    "fresh media must replace the lost local file"
                );
                assert!(matches!(
                    media_batches[0][0],
                    MediaItemPayload::Photo {
                        has_spoiler: true,
                        ..
                    }
                ));
                assert_eq!((batch_index, sent_message_ids.len()), (0, 0));
            }
            other => panic!("expected a media sequence, got {other:?}"),
        }
    }

    /// The whole repair against a real post: a queued row whose media is a local
    /// file the restart took away is re-fetched from its `source_url` and
    /// rewritten in place, so the retry can still deliver it.
    #[tokio::test]
    #[ignore = "live network: requires outbound HTTPS to public.api.bsky.app"]
    async fn live_repair_refetches_a_lost_local_media_row() {
        let stores = TestStores::new();
        // An empty script: the repair must not need to tell the user anything.
        let sender = MockSender::scripted(vec![], permanent_error);
        let ctx = stores.ctx(&sender);
        let mut task = queued_task("/nonexistent-ugoira.mp4", 0, vec![]);
        if let Task::SendMediaSequence { source_url, .. } = &mut task {
            *source_url = "https://bsky.app/profile/fu-futa.bsky.social/post/3laoveufjv224".into();
        }
        stores
            .task_queue()
            .enqueue(serde_json::to_value(&task).unwrap(), crate::db::now_f64())
            .await
            .unwrap();

        assert_eq!(repair_lost_local_media(&ctx).await, 1);

        let updated: Task = serde_json::from_value(stores.queued_payload().await).unwrap();
        match updated {
            Task::SendMediaSequence {
                media_batches,
                batch_index,
                sent_message_ids,
                caption,
                ..
            } => {
                let media: Vec<&str> = media_batches
                    .iter()
                    .flatten()
                    .map(|item| match item.media_ref() {
                        MediaRef::Source(media) | MediaRef::FileId(media) => media.as_str(),
                    })
                    .collect();
                assert!(!media.is_empty(), "the fresh fetch yielded no media");
                assert!(
                    media.iter().all(|m| m.starts_with("http")),
                    "the retry must be uploadable from URLs again: {media:?}"
                );
                assert_eq!((batch_index, sent_message_ids.len()), (0, 0));
                assert!(!caption.is_empty());
            }
            other => panic!("expected a repaired media sequence, got {other:?}"),
        }
        // The post was re-read, not re-delivered: nothing was sent.
        assert!(sender.calls().is_empty(), "{:?}", sender.calls());
    }
}
