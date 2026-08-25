use std::io::IsTerminal;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use tokio::task;
use tracing::{error, info, info_span, warn, Instrument};

use crate::client;
use crate::config::Config;
use crate::git_worktree::PrewarmTracker;
use crate::instance::InstanceMap;
use crate::socketwrapper::Listener;
use crate::tui;

pub async fn run(config: &Config) -> Result<()> {
    let instance_map = InstanceMap::new(config);
    let prewarm_tracker = config.git_worktree_prewarm.then(|| {
        let tracker = PrewarmTracker::new(config.git_worktree_poll_interval);
        tracker.start(instance_map.clone());
        tracker
    });
    if config.status_ui && std::io::stdout().is_terminal() {
        tui::start(instance_map.clone(), prewarm_tracker.clone());
    }
    let next_client_id = AtomicUsize::new(0);
    let next_client_id = || next_client_id.fetch_add(1, Ordering::Relaxed);

    let listener = Listener::bind(&config.listen).await.context("listen")?;
    info!(socket = ?config.listen, "listening");
    loop {
        match listener.accept().await {
            Ok((socket, _addr)) => {
                let client_id = next_client_id();
                let instance_map = instance_map.clone();
                let prewarm_tracker = prewarm_tracker.clone();

                task::spawn(
                    async move {
                        info!("client connected");
                        match client::process(socket, client_id, instance_map, prewarm_tracker)
                            .await
                        {
                            Ok(_) => {}
                            Err(err) => error!("client error: {err:?}"),
                        }
                    }
                    .instrument(info_span!("client", %client_id)),
                );
            }
            Err(err) => match err.kind() {
                // ignore benign errors
                std::io::ErrorKind::NotConnected => {
                    warn!("listener error {err}");
                }
                _ => {
                    Err(err).context("accept connection")?;
                }
            },
        }
    }
}
