//! Graceful shutdown: one [`Shutdown`] signal per server.
//!
//! On SIGTERM / Ctrl-C (or the future given to
//! [`serve_with_shutdown`](crate::PreparedServer::serve_with_shutdown)) the server stops accepting
//! connections, `/readyz` answers 503, in-flight requests get `server.shutdown_grace_secs` to
//! finish (then their connections are closed and the handlers dropped), then modules' `shutdown`
//! (all at the same time, within `server.module_shutdown_timeout_secs` together) and the shutdown
//! hooks run and the database pool is closed.

use std::sync::Arc;

use tokio::sync::watch;

/// A shutdown signal that can be triggered once and awaited by many.
#[derive(Clone, Debug)]
pub struct Shutdown(Arc<watch::Sender<bool>>);

impl Default for Shutdown {
    fn default() -> Self {
        Self::new()
    }
}

impl Shutdown {
    /// A signal that has not fired.
    pub fn new() -> Self {
        Self(Arc::new(watch::channel(false).0))
    }

    /// Fire the signal (idempotent).
    pub fn trigger(&self) {
        self.0.send_replace(true);
    }

    /// Whether the signal fired.
    pub fn is_triggered(&self) -> bool {
        *self.0.borrow()
    }

    /// Wait until the signal fires (at once if it already did).
    pub async fn wait(&self) {
        let mut rx = self.0.subscribe();
        // `wait_for` returns an error only if the sender is gone, which `self` prevents.
        let _ = rx.wait_for(|fired| *fired).await;
    }

    /// [`wait`](Shutdown::wait) as an owned future (for `'static` contexts).
    pub async fn wait_owned(self) {
        self.wait().await
    }
}

/// Resolves on Ctrl-C, or SIGTERM on Unix. If a handler cannot be installed, that source is
/// ignored (logged) rather than shutting down at once.
pub async fn os_signal() {
    let ctrl_c = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::warn!(%error, "cannot listen for Ctrl-C");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                tracing::warn!(%error, "cannot listen for SIGTERM");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn trigger_wakes_every_waiter() {
        let shutdown = Shutdown::new();
        assert!(!shutdown.is_triggered());
        let waiter = tokio::spawn(shutdown.clone().wait_owned());
        shutdown.trigger();
        shutdown.trigger();
        assert!(tokio::time::timeout(std::time::Duration::from_secs(2), waiter).await.is_ok());
        assert!(shutdown.is_triggered());
        shutdown.wait().await;
    }
}
