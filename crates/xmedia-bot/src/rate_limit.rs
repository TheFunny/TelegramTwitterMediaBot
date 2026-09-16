//! Per-chat token-bucket rate limiting.
//!
//! Telegram throttles bots that burst past a chat's message budget
//! (roughly 20 messages/min for channels/groups); today the bot absorbs
//! those 429s with queue retries. This limiter smooths the burst *before*
//! it reaches the API: media sends to a chat consume one token per
//! message, refilled at [`REFILL_PER_SEC`], so a batch forward paces itself
//! instead of tripping flood control. The queue retry stays as the safety
//! net for limits this bucket does not model (global per-bot limits etc.).

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::Duration;

/// Burst capacity: how many messages may be sent at once without waiting.
const CAPACITY: f64 = 20.0;
/// Sustained refill: ~20 messages per minute.
const REFILL_PER_SEC: f64 = 20.0 / 60.0;

struct State {
    /// Current token balance; may go negative (debt from an acquire larger
    /// than the capacity, repaid by subsequent refills).
    tokens: f64,
    last_refill: tokio::time::Instant,
}

/// A token bucket: at most `CAPACITY` tokens accumulate, refilled at
/// `REFILL_PER_SEC`. [`TokenBucket::acquire`] consumes `n` tokens, waiting
/// for the deficit (a single acquire may exceed the capacity and goes into
/// debt, which the refill repays).
pub struct TokenBucket {
    capacity: f64,
    refill_per_sec: f64,
    state: Mutex<State>,
}

impl TokenBucket {
    fn new(capacity: f64, refill_per_sec: f64) -> Self {
        TokenBucket {
            capacity,
            refill_per_sec,
            state: Mutex::new(State {
                tokens: capacity,
                last_refill: tokio::time::Instant::now(),
            }),
        }
    }

    /// Applies the elapsed refill to `state`. Shared by [`Self::acquire`] and
    /// the idle check so the two cannot drift apart.
    fn refill(&self, state: &mut State) {
        let now = tokio::time::Instant::now();
        let elapsed = now
            .saturating_duration_since(state.last_refill)
            .as_secs_f64();
        // Refill up to the capacity; a debt (negative balance) is repaid
        // before any surplus accumulates.
        state.tokens = (state.tokens + elapsed * self.refill_per_sec).min(self.capacity);
        state.last_refill = now;
    }

    /// Waits until `n` tokens are available, consuming them. The wait is
    /// bounded: the deficit is committed as debt and repaid over time, so a
    /// large acquire returns once its share of the refill budget has passed.
    pub async fn acquire(&self, n: f64) {
        // The parking_lot guard is confined to this block: only the plain
        // `wait` duration crosses the await (a guard across an await point
        // would make the future !Send).
        let wait = {
            let mut state = self.state.lock();
            self.refill(&mut state);
            if state.tokens >= n {
                state.tokens -= n;
                return;
            }
            // Commit the whole consumption now; the caller proceeds once the
            // deficit's worth of refill time has passed.
            let debt = n - state.tokens;
            state.tokens = -debt;
            debt / self.refill_per_sec
        };
        tokio::time::sleep(Duration::from_secs_f64(wait)).await;
    }

    /// True when the bucket has refilled to capacity: no debt outstanding, so
    /// the chat has not sent anything recently.
    fn is_idle(&self) -> bool {
        let mut state = self.state.lock();
        self.refill(&mut state);
        state.tokens >= self.capacity
    }
}

/// One limiter per chat, created on first use. Per-chat so one chat's burst
/// never throttles another.
static LIMITERS: LazyLock<Mutex<HashMap<i64, Arc<TokenBucket>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Returns the shared limiter for a chat, creating it on first use.
pub fn limiter_for(chat_id: i64) -> Arc<TokenBucket> {
    LIMITERS
        .lock()
        .entry(chat_id)
        .or_insert_with(|| Arc::new(TokenBucket::new(CAPACITY, REFILL_PER_SEC)))
        .clone()
}

/// Drops limiters that are idle (refilled to capacity, so the chat has not
/// sent recently) and are not still held by an in-flight sender. The map
/// would otherwise keep one bucket per chat that ever sent media, forever.
/// Called from the periodic sweep; returns how many were dropped.
pub fn prune_idle() -> usize {
    let mut limiters = LIMITERS.lock();
    let before = limiters.len();
    // Lock order map → bucket, the only order taken anywhere.
    limiters.retain(|_, bucket| Arc::strong_count(bucket) > 1 || !bucket.is_idle());
    before - limiters.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn limiter_for_reuses_the_per_chat_bucket() {
        let a = limiter_for(1);
        let b = limiter_for(1);
        let c = limiter_for(2);
        assert!(Arc::ptr_eq(&a, &b), "same chat → same bucket");
        assert!(!Arc::ptr_eq(&a, &c), "different chat → different bucket");
    }

    #[tokio::test(start_paused = true)]
    async fn burst_is_consumed_instantly_then_refill_waits() {
        let bucket = TokenBucket::new(3.0, 1.0);
        // A burst within capacity passes without waiting.
        bucket.acquire(3.0).await;
        // The bucket is empty now; one token needs 1s of refill.
        let start = tokio::time::Instant::now();
        bucket.acquire(1.0).await;
        assert!(
            start.elapsed() >= Duration::from_secs(1),
            "elapsed {:?}",
            start.elapsed()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn acquire_larger_than_capacity_waits_for_the_deficit() {
        let bucket = TokenBucket::new(2.0, 1.0);
        // 5 tokens with a capacity of 2: the 3-token deficit takes 3s.
        let start = tokio::time::Instant::now();
        bucket.acquire(5.0).await;
        assert!(
            start.elapsed() >= Duration::from_secs(3),
            "elapsed {:?}",
            start.elapsed()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn prune_idle_drops_full_unheld_buckets_only() {
        // Held by this task: kept even at full capacity, a sender has it.
        let held = limiter_for(9_001);
        assert!(held.is_idle(), "a fresh bucket is full");
        // Only the map holds this one and it is full → dropped.
        limiter_for(9_002);
        // Mid-debt (an acquire larger than the capacity): kept.
        {
            let bucket = Arc::new(TokenBucket::new(CAPACITY, REFILL_PER_SEC));
            bucket.state.lock().tokens = -1.0;
            LIMITERS.lock().insert(9_003, bucket);
        }

        assert!(prune_idle() >= 1);

        let limiters = LIMITERS.lock();
        assert!(limiters.contains_key(&9_001), "held bucket pruned");
        assert!(!limiters.contains_key(&9_002), "idle unheld bucket kept");
        assert!(limiters.contains_key(&9_003), "indebted bucket pruned");
    }
}
