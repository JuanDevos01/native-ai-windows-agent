//! Answers the agent is waiting for, delivered past the message queue.
//!
//! When the agent asks the person something mid-turn (an approval), it is
//! blocked inside that turn. The gateway handles one message at a time, so a
//! reply that went through the normal queue would wait behind the very turn
//! that is waiting for it - a deadlock. Channels therefore check here first:
//! if an answer is expected from this chat, the message is handed straight to
//! the waiting turn instead of being queued.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use tokio::sync::oneshot;

fn pending() -> &'static Mutex<HashMap<String, oneshot::Sender<String>>> {
    static PENDING: OnceLock<Mutex<HashMap<String, oneshot::Sender<String>>>> = OnceLock::new();
    PENDING.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Start waiting for the next message from `key` (e.g. `telegram:123`).
/// A newer wait for the same key replaces an older one.
pub fn expect_answer(key: &str) -> oneshot::Receiver<String> {
    let (tx, rx) = oneshot::channel();
    pending()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(key.to_string(), tx);
    rx
}

/// Stop waiting (timeout or the turn ended).
pub fn cancel(key: &str) {
    pending()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(key);
}

/// Called by a channel for every incoming message. Returns true if the
/// message was an expected answer and has been delivered - the channel must
/// then NOT also queue it as a new request.
pub fn try_answer(key: &str, text: &str) -> bool {
    let tx = pending()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(key);
    match tx {
        Some(tx) => tx.send(text.to_string()).is_ok(),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_expected_answer_is_delivered_and_not_queued() {
        let rx = expect_answer("telegram:test-1");
        assert!(try_answer("telegram:test-1", "2"));
        assert_eq!(rx.await.unwrap(), "2");
        // Only once: the next message is ordinary again.
        assert!(!try_answer("telegram:test-1", "hello"));
    }

    #[test]
    fn an_unexpected_message_is_left_for_the_queue() {
        assert!(!try_answer("telegram:nobody-waiting", "hello"));
    }

    #[test]
    fn a_cancelled_wait_no_longer_captures_messages() {
        let _rx = expect_answer("telegram:test-2");
        cancel("telegram:test-2");
        assert!(!try_answer("telegram:test-2", "1"));
    }
}
