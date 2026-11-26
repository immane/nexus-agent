//! Run-owned, in-memory supervision of background read-only exploration.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nexus_core::provider::{ChildCompletion, ChildStatus};
use nexus_core::{CancelCommand, RequestId, RunId, RunOutcome, SessionId, SubmitCommand, Usage};
use tokio::sync::watch;

use crate::{EventStreams, Runtime};

pub(crate) const MAX_BRIEF_BYTES: usize = 4096;
pub(crate) const MAX_RESULT_BYTES: usize = ChildCompletion::MAX_TEXT_BYTES;
pub(crate) const MAX_DURATION: Duration = Duration::from_secs(45);

type WorkerId = (RunId, RequestId);
type WorkerMap = HashMap<WorkerId, Arc<Job>>;

#[derive(Default)]
pub(crate) struct ExplorationWorkers(Arc<Mutex<WorkerMap>>);

impl ExplorationWorkers {
    pub(crate) fn track(&self, parent: RunId, id: RequestId, job: Arc<Job>) -> ExplorationWorker {
        let key = (parent, id);
        self.0
            .lock()
            .expect("child workers lock")
            .insert(key.clone(), job);
        ExplorationWorker {
            workers: self.0.clone(),
            key,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.0.lock().expect("child workers lock").len()
    }

    pub(crate) fn stop(&self) {
        for job in self.0.lock().expect("child workers lock").values() {
            job.stop();
        }
    }
}

pub(crate) struct ExplorationWorker {
    workers: Arc<Mutex<WorkerMap>>,
    key: WorkerId,
}

impl Drop for ExplorationWorker {
    fn drop(&mut self) {
        self.workers
            .lock()
            .expect("child workers lock")
            .remove(&self.key);
    }
}

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
    /// Whether the caller must validate a structured proposal before display.
    pub role: ExplorationRole,
    /// Child inference usage, kept separate from the parent's counters.
    pub usage: Option<Usage>,
}

/// Which read-only instructions to give the child.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ExplorationRole {
    /// Return a short investigation result.
    #[default]
    Explore,
    /// Return one suggested replacement, never a workspace mutation.
    Propose,
}

impl ExplorationResult {
    /// The terminal classification shared by explicit and automatic delivery.
    pub fn status(&self) -> ChildStatus {
        if self.timed_out {
            return ChildStatus::TimedOut;
        }
        match self.outcome {
            RunOutcome::Completed => ChildStatus::Completed,
            RunOutcome::Refused => ChildStatus::Refused,
            RunOutcome::Failed => ChildStatus::Failed,
            RunOutcome::Cancelled => ChildStatus::Cancelled,
            RunOutcome::LimitReached => ChildStatus::LimitReached,
        }
    }

    /// Parses one bounded exact replacement from a successful Propose job.
    /// The host still has to validate the referenced project file.
    pub fn proposal(&self) -> Option<ExplorationProposal> {
        if self.role != ExplorationRole::Propose
            || self.status() != ChildStatus::Completed
            || self.truncated
        {
            return None;
        }
        ExplorationProposal::parse(&self.text)
    }
}

/// Structured suggestion, not permission to modify a file.
#[derive(Debug)]
pub struct ExplorationProposal {
    /// Project-relative file.
    pub path: String,
    /// Exact existing text.
    pub old_text: String,
    /// Suggested replacement.
    pub new_text: String,
}

impl ExplorationProposal {
    fn parse(text: &str) -> Option<Self> {
        use std::path::{Component, Path};

        let value = nexus_validation::parse_object(text, MAX_RESULT_BYTES).ok()?;
        let fields = value.as_object()?;
        if fields.len() != 3 {
            return None;
        }
        let path = fields.get("path")?.as_str()?.to_owned();
        let old_text = fields.get("old_text")?.as_str()?.to_owned();
        let new_text = fields.get("new_text")?.as_str()?.to_owned();
        if path.is_empty()
            || path.len() > 1024
            || path.contains(['\0', '\\'])
            || !Path::new(&path)
                .components()
                .all(|component| matches!(component, Component::Normal(_)))
            || old_text.is_empty()
            || old_text.len() > 4096
            || new_text.len() > 4096
            || old_text == new_text
        {
            return None;
        }
        Some(Self {
            path,
            old_text,
            new_text,
        })
    }
}
/// How a child result may be collected.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ExplorationDelivery {
    /// Only an explicit wait may collect this result.
    #[default]
    Await,
    /// Offer at the next safe request boundary of the originating parent.
    Steer,
    /// Offer only after the originating parent completes successfully.
    Queue,
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
pub(crate) struct Explorations {
    jobs: HashMap<RequestId, Arc<Job>>,
    steering: Vec<RequestId>,
}

impl Explorations {
    pub(crate) fn get(&self, id: &RequestId) -> Option<Arc<Job>> {
        self.jobs.get(id).cloned()
    }

    pub(crate) fn insert(&mut self, id: RequestId, job: Arc<Job>, delivery: ExplorationDelivery) {
        if delivery == ExplorationDelivery::Steer {
            self.steering.push(id.clone());
        }
        self.jobs.insert(id, job);
    }

    pub(crate) fn cancel_all(&self) {
        for job in self.jobs.values() {
            job.stop();
        }
    }

    pub(crate) fn parent_completed(&self) {
        for job in self.jobs.values() {
            job.parent_completed.send_replace(true);
        }
    }

    pub(crate) fn has_steering(&self) -> bool {
        !self.steering.is_empty()
    }

    /// Offers completed jobs in registration order without waiting for pending jobs.
    pub(crate) fn steer(
        &mut self,
        parent: &RunId,
        mut accept: impl FnMut(ChildCompletion) -> bool,
    ) {
        self.steering.retain(|id| {
            let job = &self.jobs[id];
            matches!(
                job.try_deliver(|result| {
                    ChildCompletion::new(
                        parent.clone(),
                        id.clone(),
                        result.status(),
                        result.text.clone(),
                        result.truncated,
                    )
                    .map(|item| {
                        result
                            .usage
                            .map_or(item.clone(), |usage| item.with_usage(usage))
                    })
                    .is_ok_and(&mut accept)
                }),
                ExplorationClaim::Pending | ExplorationClaim::Deferred
            )
        });
    }
}

enum Completion {
    Pending,
    Finished(ExplorationResult),
    Collected,
    ParentStopped,
}

/// Outcome of a synchronous attempt to hand a child result to the inbox.
#[derive(Debug, PartialEq, Eq)]
pub enum ExplorationClaim {
    /// The job has not finished.
    Pending,
    /// The inbox declined the handoff; the result remains available.
    Deferred,
    /// The inbox accepted and consumed the result.
    Accepted,
    /// Another delivery path has already consumed the result.
    Collected,
    /// The parent stopped and no result can be delivered.
    ParentStopped,
    /// The runtime no longer knows this parent or job.
    Unknown,
}

/// Parent state at the moment of an inbox offer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExplorationParent {
    /// The originating run is still active.
    Active,
    /// The originating run finished successfully and no newer run is active.
    IdleAfterSuccess,
    /// A newer run is active; delivery must not steer that run.
    NewerRunActive,
}

pub(crate) struct Job {
    cancel: watch::Sender<bool>,
    completion: watch::Sender<Completion>,
    parent_completed: watch::Sender<bool>,
}

/// A non-consuming notification and atomic claim for automatic queue delivery.
#[derive(Clone)]
pub struct ExplorationHandoff(Arc<Job>);

impl ExplorationHandoff {
    pub(crate) fn new(job: Arc<Job>) -> Self {
        Self(job)
    }

    /// Waits for successful parent completion and an unclaimed child result.
    pub async fn ready(&self) -> bool {
        let mut parent = self.0.parent_completed.subscribe();
        let mut completion = self.0.completion.subscribe();
        loop {
            match &*completion.borrow_and_update() {
                Completion::Finished(_) if *parent.borrow_and_update() => return true,
                Completion::Collected | Completion::ParentStopped => return false,
                _ => {}
            }
            tokio::select! {
                _ = parent.changed() => {}
                _ = completion.changed() => {}
            }
        }
    }

    /// Claims only if the parent succeeded and the inbox accepts the result.
    pub fn offer(&self, accept: impl FnOnce(&ExplorationResult) -> bool) -> ExplorationClaim {
        if !*self.0.parent_completed.borrow() {
            return ExplorationClaim::Deferred;
        }
        self.0.try_deliver(accept)
    }

    /// Drops an unclaimed result at the end of its handoff window.
    pub fn expire(&self) -> bool {
        self.offer(|_| true) == ExplorationClaim::Accepted
    }
}

impl Job {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            cancel: watch::channel(false).0,
            completion: watch::channel(Completion::Pending).0,
            parent_completed: watch::channel(false).0,
        })
    }

    fn stop(&self) {
        self.completion.send_replace(Completion::ParentStopped);
        self.cancel.send_replace(true);
    }

    pub(crate) fn finish(&self, result: ExplorationResult) {
        self.completion.send_if_modified(|state| {
            if matches!(state, Completion::Pending) {
                *state = Completion::Finished(result);
                true
            } else {
                false
            }
        });
    }

    /// Claims under the same lock as cancellation and explicit waits. Admission
    /// must be synchronous, local, and must not re-enter the job. A declined
    /// handoff retains the payload for another attempt or explicit wait.
    pub(crate) fn try_deliver(
        &self,
        accept: impl FnOnce(&ExplorationResult) -> bool,
    ) -> ExplorationClaim {
        self.claim(|result| if accept(&result) { Ok(()) } else { Err(result) })
    }

    fn claim(
        &self,
        accept: impl FnOnce(ExplorationResult) -> Result<(), ExplorationResult>,
    ) -> ExplorationClaim {
        use ExplorationClaim as Claim;

        let mut claim = Claim::Pending;
        self.completion.send_if_modified(|state| {
            claim = match state {
                Completion::Pending => Claim::Pending,
                Completion::Collected => Claim::Collected,
                Completion::ParentStopped => Claim::ParentStopped,
                Completion::Finished(_) => {
                    let Completion::Finished(result) =
                        std::mem::replace(state, Completion::Collected)
                    else {
                        unreachable!("matched finished state");
                    };
                    match accept(result) {
                        Ok(()) => Claim::Accepted,
                        Err(result) => {
                            *state = Completion::Finished(result);
                            Claim::Deferred
                        }
                    }
                }
            };
            claim == Claim::Accepted
        });
        claim
    }

    pub(crate) async fn take(&self) -> ExplorationWait {
        use ExplorationClaim as Claim;

        let mut receiver = self.completion.subscribe();
        loop {
            let mut result = None;
            match self.claim(|value| {
                result = Some(value);
                Ok(())
            }) {
                Claim::Accepted => {
                    return ExplorationWait::Finished(result.expect("claim accepted result"));
                }
                Claim::Collected => return ExplorationWait::Collected,
                Claim::ParentStopped => return ExplorationWait::ParentStopped,
                Claim::Deferred | Claim::Unknown => {
                    unreachable!("explicit wait owns the job and accepts its result")
                }
                Claim::Pending => {
                    let _ = receiver.changed().await;
                }
            }
        }
    }

    pub(crate) async fn run(
        self: Arc<Self>,
        runtime: Runtime,
        streams: EventStreams,
        command: SubmitCommand,
        deadline: Instant,
        _worker: ExplorationWorker,
        role: ExplorationRole,
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
            let mut usage = None;
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
                    event = control.recv(), if control_open => {
                        if let Some(event) = &event
                            && let nexus_core::EventPayload::UsageUpdated(observed) = event.payload()
                        {
                            usage = Some(*observed);
                        }
                        control_open = event.is_some();
                    }
                    event = data.recv(), if data_open => data_open = event.is_some(),
                }
            };
            let mut result = if let Some(timed_out) = stop {
                stopped(RunOutcome::Cancelled, timed_out)
            } else if outcome == RunOutcome::Completed {
                completed(&runtime, &session).await
            } else {
                stopped(outcome, false)
            };
            result.usage = usage;
            result
        } else {
            stopped(RunOutcome::Failed, false)
        };
        self.finish(ExplorationResult { role, ..result });
        runtime.wait_for_worker_exit().await;
    }
}

fn stopped(outcome: RunOutcome, timed_out: bool) -> ExplorationResult {
    ExplorationResult {
        outcome,
        timed_out,
        text: String::new(),
        truncated: false,
        role: ExplorationRole::Explore,
        usage: None,
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
        role: ExplorationRole::Explore,
        usage: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn exploration_claim_retains_declined_handoff_for_explicit_wait() {
        let job = Job::new();
        assert_eq!(
            job.try_deliver(|_| panic!("pending result offered")),
            ExplorationClaim::Pending
        );
        job.finish(stopped(RunOutcome::Refused, false));
        assert_eq!(job.try_deliver(|_| false), ExplorationClaim::Deferred);
        assert_eq!(
            job.take().await,
            ExplorationWait::Finished(stopped(RunOutcome::Refused, false))
        );
        assert_eq!(
            job.try_deliver(|_| panic!("collected result offered twice")),
            ExplorationClaim::Collected
        );
    }

    #[tokio::test]
    async fn exploration_claim_races_explicit_wait_without_delivering_twice() {
        let job = Job::new();
        let waiter = tokio::spawn({
            let job = job.clone();
            async move { job.take().await }
        });
        job.finish(stopped(RunOutcome::Refused, false));
        let offer = job.try_deliver(|_| true);
        match offer {
            ExplorationClaim::Accepted => {
                assert_eq!(waiter.await.unwrap(), ExplorationWait::Collected)
            }
            ExplorationClaim::Collected => assert!(matches!(
                waiter.await.unwrap(),
                ExplorationWait::Finished(_)
            )),
            other => panic!("unexpected claim: {other:?}"),
        }
    }

    #[tokio::test]
    async fn exploration_claim_suppresses_results_on_both_sides_of_parent_stop() {
        for finish_first in [false, true] {
            let job = Job::new();
            if finish_first {
                job.finish(stopped(RunOutcome::Refused, false));
            }
            job.stop();
            if !finish_first {
                job.finish(stopped(RunOutcome::Failed, false));
            }
            assert_eq!(
                job.try_deliver(|_| panic!("stopped parent must not get a result")),
                ExplorationClaim::ParentStopped
            );
            assert_eq!(job.take().await, ExplorationWait::ParentStopped);
        }
    }
}
