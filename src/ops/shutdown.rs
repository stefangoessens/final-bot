use tokio::sync::watch;

#[derive(Clone)]
pub struct Shutdown {
    rx: watch::Receiver<bool>,
}

#[derive(Clone)]
pub struct ShutdownTrigger {
    tx: watch::Sender<bool>,
}

pub fn channel() -> (ShutdownTrigger, Shutdown) {
    let (tx, rx) = watch::channel(false);
    (ShutdownTrigger { tx }, Shutdown { rx })
}

impl Shutdown {
    pub async fn wait(mut self) {
        while !*self.rx.borrow() {
            if self.rx.changed().await.is_err() {
                break;
            }
        }
    }
}

impl ShutdownTrigger {
    pub fn trigger(&self) {
        let _ = self.tx.send(true);
    }
}

pub async fn listen_for_shutdown(trigger: ShutdownTrigger) {
    #[cfg(unix)]
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
    {
        Ok(signal) => signal,
        Err(err) => {
            tracing::warn!(target: "shutdown", error = %err, "failed to register SIGTERM handler");
            trigger.trigger();
            return;
        }
    };

    let ctrl_c = tokio::signal::ctrl_c();

    #[cfg(unix)]
    tokio::select! {
        _ = ctrl_c => {},
        _ = term.recv() => {},
    }

    #[cfg(not(unix))]
    let _ = ctrl_c.await;

    tracing::info!(target: "shutdown", "shutdown signal received");
    trigger.trigger();
}
