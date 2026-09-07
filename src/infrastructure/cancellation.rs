use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use tokio::sync::Notify;

/// Cooperative cancellation shared by the CLI and the process-owning adapters.
///
/// Cancellation is sticky: subscribers created after `cancel` still observe it.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    state: Arc<CancellationState>,
}

#[derive(Debug, Default)]
struct CancellationState {
    cancelled: AtomicBool,
    notify: Notify,
}

impl CancellationToken {
    pub fn cancel(&self) {
        if !self.state.cancelled.swap(true, Ordering::AcqRel) {
            self.state.notify.notify_waiters();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.state.cancelled.load(Ordering::Acquire)
    }

    pub async fn cancelled(&self) {
        loop {
            let notified = self.state.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancellation_wakes_existing_subscribers() {
        let token = CancellationToken::default();
        let subscriber = token.clone();
        let waiting = tokio::spawn(async move { subscriber.cancelled().await });

        token.cancel();

        waiting.await.unwrap();
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn cancellation_is_observed_by_late_subscribers() {
        let token = CancellationToken::default();
        token.cancel();

        tokio::time::timeout(std::time::Duration::from_millis(50), token.cancelled())
            .await
            .expect("sticky cancellation must resolve immediately");
    }
}
