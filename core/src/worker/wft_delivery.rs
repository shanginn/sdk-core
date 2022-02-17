use crate::worker::OneshotNotification;
use crate::{pollers, pollers::BoxedWFPoller};
use std::future;
use std::sync::atomic::{AtomicBool, Ordering};
use temporal_sdk_core_protos::temporal::api::workflowservice::v1::PollWorkflowTaskQueueResponse;
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::sync::{Mutex, Notify};

/// Workflow tasks typically come from polling, but may also come as a response to task completion.
/// This struct allows fetching WFTs to be centralized while prioritizing tasks from completes.
pub(crate) struct WFTSource {
    from_completions_tx: UnboundedSender<PollWorkflowTaskQueueResponse>,
    from_completions_rx: Mutex<UnboundedReceiver<PollWorkflowTaskQueueResponse>>,
    poll_buffer: BoxedWFPoller,
    task_taken_notifier: Notify,
    poll_buffer_complete: AtomicBool,
    shutdown_notifier: Mutex<Box<dyn OneshotNotification + Send>>,
}

impl WFTSource {
    pub fn new(
        poller: BoxedWFPoller,
        shutdown_notifier: Box<dyn OneshotNotification + Send>,
    ) -> Self {
        let (from_completions_tx, from_completions_rx) = unbounded_channel();
        Self {
            poll_buffer: poller,
            task_taken_notifier: Notify::new(),
            poll_buffer_complete: AtomicBool::new(false),
            from_completions_rx: Mutex::new(from_completions_rx),
            from_completions_tx,
            shutdown_notifier: Mutex::new(shutdown_notifier),
        }
    }

    /// Returns the next available WFT if one is already stored from a completion, otherwise
    /// forwards to the poller.
    pub async fn next_wft(&self) -> Option<pollers::Result<PollWorkflowTaskQueueResponse>> {
        let poll_completions = async {
            self.from_completions_rx
                .lock()
                .await
                .recv()
                .await
                .map(|wft| Ok(wft))
        };
        let poll_buffer = async {
            if self.poll_buffer_complete.load(Ordering::SeqCst) {
                future::pending().await
            } else {
                match self.poll_buffer.poll().await {
                    None => {
                        self.poll_buffer_complete.store(true, Ordering::SeqCst);
                        None
                    }
                    other => other,
                }
            }
        };
        tokio::select! {
            biased;

            _ = async { let notifier = self.shutdown_notifier.lock().await; notifier.ready().await } => None,
            wft = poll_completions => wft,
            wft = poll_buffer => wft,
        }
    }

    /// Add a WFT received from the completion of another WFT
    pub fn add_wft_from_completion(&self, wft: PollWorkflowTaskQueueResponse) {
        let _ = self.from_completions_tx.send(wft);
    }

    /// Notifies the pollers to stop polling
    pub fn stop_pollers(&self) {
        self.poll_buffer.notify_shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_help::mock_poller;
    use crate::worker::TokenOneshotNotification;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn drains_from_completes_on_shutdown() {
        let mp = mock_poller();
        let wftsrc = WFTSource::new(Box::new(mp), Box::new(TokenOneshotNotification::default()));
        let fake_wft = PollWorkflowTaskQueueResponse {
            started_event_id: 1,
            ..Default::default()
        };
        wftsrc.add_wft_from_completion(fake_wft);
        let fake_wft = PollWorkflowTaskQueueResponse {
            started_event_id: 2,
            ..Default::default()
        };
        wftsrc.add_wft_from_completion(fake_wft);
    }
}
