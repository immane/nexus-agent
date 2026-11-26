//! Run-owned, in-memory supervision of background read-only exploration.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use nexus_core::{CancelCommand, RequestId, RunOutcome, SessionId, SubmitCommand};
use tokio::sync::watch;

use crate::{EventStreams, Runtime};

pub(crate) const MAX_BRIEF_BYTES: usize = 4096;
pub(crate) const MAX_RESULT_BYTES: usize = 8192;
pub(crate) const MAX_DURATION: Duration = Duration::from_secs(45);

/// Terminal output of a read-only exploration. Non-completed jobs return no text.
#[derive(Debug, PartialEq, Eq)]
pub struct ExplorationResult {
    /// Runtime outcome; no tool effects are implied.
    pub outcome: RunOutcome,
    /// Whether the supervisor's wall-clock deadline triggered cancellation.
    pub timed_out: bool,
    /// Accepted final answer, not streamed scratch text.
    pub text: String,
    /// The result was cut to its finite byte budget.
    pub truncated: bool,
}

/// Result of waiting for a job owned by the supplied parent run.
#[derive(Debug, PartialEq, Eq)]
pub enum ExplorationWait {
    /// This caller consumed the result.
    Finished(ExplorationResult),
    /// The result was already claimed.
    Collected,
    /// The parent stopped waiting; the child may still be draining.
    ParentStopped,
    /// The job isn't owned by the current parent.
    Unknown,
}

#[derive(Default)]
pub(crate) struct Explorations(HashMap<RequestId, Arc<Job>>);

impl Explorations {
    pub(crate) fn get(&self, id: &RequestId) -> Option<Arc<Job>> {
        self.0.get(id).cloned()
    }

    pub(crate) fn insert(&mut self, id: RequestId, job: Arc<Job>) {
        self.0.insert(id, job);
    }

    pub(crate) fn cancel_all(&self) {
        for job in self.0.values() {
            job.cancel.send_replace(true);
        }
    }
}

enum Completion {
    Pending,
    Finished(ExplorationResult),
    Collected,
}

pub(crate) struct Job {
    cancel: watch::Sender<bool>,
    completion: watch::Sender<Completion>,
}

impl Job {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            cancel: watch::channel(false).0,
            completion: watch::channel(Completion::Pending).0,
        })
    }

    pub(crate) async fn take(&self) -> ExplorationWait {
        let mut receiver = self.completion.subscribe();
        let mut cancellation = self.cancel.subscribe();
        loop {
            if *cancellation.borrow() {
                return ExplorationWait::ParentStopped;
            }
            let mut answer = None;
            self.completion.send_if_modified(|state| match state {
                Completion::Pending => false,
                Completion::Collected => {
                    answer = Some(ExplorationWait::Collected);
                    false
                }
                Completion::Finished(_) => {
                    if let Completion::Finished(result) =
                        std::mem::replace(state, Completion::Collected)
                    {
                        answer = Some(ExplorationWait::Finished(result));
                    }
                    true
                }
            });
            if let Some(answer) = answer {
                return answer;
            }
            tokio::select! {
                biased;
                _ = cancellation.wait_for(|value| *value) => return ExplorationWait::ParentStopped,
                _ = receiver.changed() => {}
            }
        }
    }

    pub(crate) async fn run(
        self: Arc<Self>,
        runtime: Runtime,
        streams: EventStreams,
        command: SubmitCommand,
        deadline: Instant,
    ) {
        let mut cancellation = self.cancel.subscribe();
        let session = command.session.clone();
        let timed_out = Instant::now() >= deadline;
        let result = if *cancellation.borrow() || timed_out {
            stopped(RunOutcome::Cancelled, timed_out)
        } else if let Some(run) = runtime.submit(command).await.run().cloned() {
            let EventStreams {
                mut control,
                mut data,
            } = streams;
            let terminal = runtime.wait_for_terminal(&run);
            tokio::pin!(terminal);
            let mut stop = None;
            let mut control_open = true;
            let mut data_open = true;
            let outcome = loop {
                tokio::select! {
                    result = &mut terminal => break result.unwrap_or(RunOutcome::Failed),
                    _ = async { let _ = cancellation.wait_for(|value| *value).await; }, if stop.is_none() => {
                        stop = Some(false);
                        let _ = runtime.cancel(CancelCommand {
                            request: RequestId::new("exploration-cancel").expect("valid request"),
                            run: run.clone(),
                        }).await;
                    }
                    _ = tokio::time::sleep_until(deadline.into()), if stop.is_none() => {
                        stop = Some(true);
                        let _ = runtime.cancel(CancelCommand {
                            request: RequestId::new("exploration-timeout").expect("valid request"),
                            run: run.clone(),
                        }).await;
                    }
                    // The child must be able to publish even if nobody awaits it.
                    event = control.recv(), if control_open => control_open = event.is_some(),
                    event = data.recv(), if data_open => data_open = event.is_some(),
                }
            };
            if let Some(timed_out) = stop {
                stopped(RunOutcome::Cancelled, timed_out)
            } else if outcome == RunOutcome::Completed {
                completed(&runtime, &session).await
            } else {
                stopped(outcome, false)
            }
        } else {
            stopped(RunOutcome::Failed, false)
        };
        self.completion.send_replace(Completion::Finished(result));
    }
}

fn stopped(outcome: RunOutcome, timed_out: bool) -> ExplorationResult {
    ExplorationResult {
        outcome,
        timed_out,
        text: String::new(),
        truncated: false,
    }
}

async fn completed(runtime: &Runtime, session: &SessionId) -> ExplorationResult {
    let Some((text, truncated)) = runtime.exploration_answer(session, MAX_RESULT_BYTES).await
    else {
        return stopped(RunOutcome::Failed, false);
    };
    ExplorationResult {
        outcome: RunOutcome::Completed,
        timed_out: false,
        text,
        truncated,
    }
}
