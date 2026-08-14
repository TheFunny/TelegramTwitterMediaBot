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

    /// Waits until `n` tokens are available, consuming them. The wait is
    /// bounded: the deficit is committed as debt and repaid over time, so a
    /// large acquire returns once its share of the refill budget has passed.
    pub async fn acquire(&self, n: f64) {
        // The parking_lot guard is confined to this block: only the plain
        // `wait` duration crosses the await (a guard across an await point
        // would make the future !Send).
        let wait = {
            let mut state = self.state.lock();
            let now = tokio::time::Instant::now();
            let elapsed = now
                .saturating_duration_since(state.last_refill)
                .as_secs_f64();
            // Refill up to the capacity; a debt (negative balance) is repaid
            // before any surplus accumulates.
            state.tokens = (state.tokens + elapsed * self.refill_per_sec).min(self.capacity);
            state.last_refill = now;
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
}
