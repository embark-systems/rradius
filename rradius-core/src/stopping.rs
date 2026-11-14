//! The signal that the server stops taking requests: cancelled once, waited for by every loop
//! that takes them in.
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::Notify;

/// Cancelled once, when the server stops taking requests; clones share it.
#[derive(Clone, Default)]
pub struct Stopping(Arc<Inner>);

#[derive(Default)]
struct Inner {
    cancelled: AtomicBool,
    notify: Notify,
}

impl Stopping {
    pub fn new() -> Self {
        Self::default()
    }

    /// Cancels it for every clone; what waits in `cancelled` returns.
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::SeqCst);
        self.0.notify.notify_waiters();
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::SeqCst)
    }

    /// Returns once it is cancelled, at once if it already is.
    pub async fn cancelled(&self) {
        loop {
            let notified = self.0.notify.notified();
            tokio::pin!(notified);
            // Registered before the check, so a cancel between the two is not missed.
            notified.as_mut().enable();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

impl std::fmt::Debug for Stopping {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Stopping").field(&self.is_cancelled()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn test_cancel_wakes_every_waiter() -> Result<(), String> {
        let stopping = Stopping::new();
        let waiters: Vec<_> = (0..3).map(|_| tokio::spawn({
            let stopping = stopping.clone();
            async move { stopping.cancelled().await }
        })).collect();
        tokio::task::yield_now().await;
        assert!(!stopping.is_cancelled());
        stopping.cancel();
        for waiter in waiters {
            tokio::time::timeout(Duration::from_secs(1), waiter).await.map_err(|_| "a waiter was not woken".to_string())?.map_err(|e| e.to_string())?;
        }
        // Already cancelled: returns at once.
        tokio::time::timeout(Duration::from_secs(1), stopping.cancelled()).await.map_err(|_| "not at once".to_string())?;
        Ok(())
    }
}
