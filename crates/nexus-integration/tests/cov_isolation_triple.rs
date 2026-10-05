#![forbid(unsafe_code)]

//! Coverage hardening: runtime isolation across three simultaneous instances.
//!
//! The two-way case lives in `review_runtime_isolation.rs`. Three live
//! instances are the first shape where a per-instance counter or a shared
//! grant table becomes visible, so this file pins the same contract one level
//! up:
//! - every host-issued identity (`RunId`, `CallId`, `ApprovalId`) carries its
//!   own runtime's incarnation segment, so three simultaneously live
//!   instances never alias even though all three sit at identical ordinals;
//! - a command addressed to another instance's **live** run is
//!   `StaleOrUnknownTarget` for `cancel`, `get_snapshot`, and `approve` —
//!   never `AlreadyFinalized`, never `Accepted` — and the target keeps its
//!   pending grant;
//! - a grant is bound to the exact `(run, call, approval)` triple of the
//!   instance that issued it, so no instance resolves or dispatches another
//!   instance's pending call;
//! - each instance dispatches only its own call, exactly once, and finalizes
//!   exactly once with `Completed`; afterwards its own run reads
//!   `AlreadyFinalized` while another instance's finalized run is still
//!   unknown to it.
//!
//! Determinism: all three instances are parked on a pending approval before
//! any cross-instance command is sent, so no assertion races a driver that is
//! still working, and each instance is granted and drained on its own turn so
//! the other two cannot have dispatched yet. Every wait is bounded by the
//! shared drain backstop, and every assertion reads a recorded invariant
//! (identity values, command replies, snapshot state, tool logs, event
//! counts), never wall-clock timing.

mod common;

use std::collections::HashSet;

use nexus_core::{
    ApprovalId, ApproveCommand, CallId, CancelCommand, CommandReply, EffectState, EventPayload,
    Evidence, ExecutionStatus, GetSnapshotCommand, ProviderEvent, RequestId, RunEvent, RunFinished,
    RunId, RunLifecycle, RunOutcome, Snapshot,
};
use nexus_fakes::{FakeToolCallRecord, stop_turn, tool_turn};
use nexus_runtime::event_is_live;

/// Simultaneously live runtime instances under test.
const INSTANCES: usize = 3;

/// The exact argument text every instance's mutating call is granted and
/// dispatched with.
const GRANTED_ARGS: &str = r#"{"path":"src"}"#;

/// One independent runtime instance parked on its own pending approval.
struct Instance {
    bed: common::Bed,
    run: RunId,
    approval: ApprovalId,
    call: CallId,
    /// Control events already consumed while waiting for `approval`.
    prelude: Vec<RunEvent>,
}

/// One instance's complete published stream plus its terminal record and its
/// tool doubles' independent records of what actually executed.
struct Settled {
    index: usize,
    run: RunId,
    approval: ApprovalId,
    call: CallId,
    data: Vec<RunEvent>,
    control: Vec<RunEvent>,
    finished: RunFinished,
    write_log: Vec<FakeToolCallRecord>,
    read_executions: usize,
    provider_calls: usize,
}

/// One mutation turn behind an approval, then a plain stop turn.
///
/// Every instance serves this byte-identical script with its own provider and
/// tool doubles, so any identity difference between instances is host-issued
/// rather than inherited from turn-local provider values.
fn mutation_script() -> Vec<Vec<ProviderEvent>> {
    vec![
        tool_turn(vec![common::candidate("host_write", GRANTED_ARGS)]),
        stop_turn("done"),
    ]
}

/// Builds a static-safe test request identity.
fn request(raw: &str) -> RequestId {
    RequestId::new(raw).expect("static safe test request identity")
}

/// Asserts `ids` (one per instance) are pairwise distinct.
#[track_caller]
fn assert_pairwise_distinct(label: &str, ids: &[&str]) {
    let unique: HashSet<&str> = ids.iter().copied().collect();
    assert_eq!(
        unique.len(),
        ids.len(),
        "{label} are pairwise distinct across instances: {ids:?}"
    );
}

/// Splits a runtime-issued identity into its incarnation segment and the
/// ordinal tail that follows it.
///
/// The runtime mints `r{incarnation:x}-{run_n}`,
/// `c{incarnation:x}-{run_n}-{call_seq}`, and `a{incarnation:x}-{run_n}-{approval_n}`.
/// The incarnation is the only component that separates two simultaneously
/// live instances, so isolating it is what proves these are instance-scoped
/// identities instead of three views of one shared counter.
fn split_issued(raw: &str, kind: char) -> (&str, &str) {
    let rest = raw
        .strip_prefix(kind)
        .unwrap_or_else(|| panic!("{raw} is not a {kind}-scoped identity"));
    let (incarnation, ordinals) = rest
        .split_once('-')
        .unwrap_or_else(|| panic!("{raw} carries no incarnation segment"));
    assert!(!incarnation.is_empty(), "{raw} has an empty incarnation");
    (incarnation, ordinals)
}

/// Asserts every published event is live for its own run and stale for each
/// foreign run: the consume-side check a three-instance consumer must apply.
#[track_caller]
fn assert_scoped_to_run(label: &str, events: &[&RunEvent], own: &RunId, foreign: &[&RunId]) {
    assert!(!events.is_empty(), "{label} published events");
    for event in events {
        assert!(
            event_is_live(event, Some(own)),
            "{label}: every event is live for its own run"
        );
        for other in foreign {
            assert!(
                !event_is_live(event, Some(other)),
                "{label}: every event is stale for a foreign run"
            );
        }
    }
}

/// Builds `INSTANCES` independent runtimes, submits one run on each, and parks
/// every run on its own pending approval before the caller sends any
/// cross-instance command.
async fn parked_triple(tag: &str) -> Vec<Instance> {
    let mut instances = Vec::with_capacity(INSTANCES);
    for index in 0..INSTANCES {
        let mut bed = common::make_bed(mutation_script(), common::quick_config());
        let submit_tag = format!("{tag}-{index}");
        let response = bed.runtime.submit(common::submit_cmd(&submit_tag)).await;
        assert_eq!(
            response.reply(),
            CommandReply::Accepted,
            "instance {index} accepts its own submission"
        );
        let run = response
            .run()
            .cloned()
            .expect("an accepted submit issues a run");
        let prelude = common::collect_control_until(&mut bed.control, |event| {
            matches!(event.payload(), EventPayload::ApprovalRequired(_))
        })
        .await;
        let (approval, call) =
            common::find_approval(&prelude).expect("the run requested its own approval");
        instances.push(Instance {
            bed,
            run,
            approval,
            call,
            prelude,
        });
    }
    instances
}

/// Drains one instance to its terminal and returns everything its assertions
/// need. The control events consumed while waiting for the approval are folded
/// back in, so the returned stream is the instance's complete published
/// history rather than only its post-approval tail.
async fn finish_instance(instances: &mut [Instance], index: usize) -> Settled {
    let instance = &mut instances[index];
    let (data, mut control, finished) =
        common::drain_until_finished(&mut instance.bed.data, &mut instance.bed.control).await;
    control.append(&mut instance.prelude);
    Settled {
        index,
        run: instance.run.clone(),
        approval: instance.approval.clone(),
        call: instance.call.clone(),
        data,
        control,
        finished,
        write_log: instance.bed.write_tool.log(),
        read_executions: instance.bed.read_tool.execution_count(),
        provider_calls: instance.bed.provider.call_count(),
    }
}

/// Returns an instance's own snapshot, asserting the runtime serves one. A
/// stale target would return no snapshot at all, so the assertion also proves
/// the snapshot came from this instance rather than a neighbour.
async fn own_snapshot(instance: &Instance, tag: &str) -> Snapshot {
    let (reply, snapshot) = instance
        .bed
        .runtime
        .get_snapshot(GetSnapshotCommand {
            request: request(tag),
            run: instance.run.clone(),
        })
        .await;
    assert_eq!(reply.reply(), CommandReply::Accepted, "{tag}");
    let snapshot =
        snapshot.unwrap_or_else(|| panic!("{tag} serves a snapshot for the instance's own run"));
    assert_eq!(snapshot.run(), &instance.run, "{tag} snapshots its own run");
    snapshot
}

/// Asserts an instance is still live, holds exactly its own grant, and has
/// dispatched nothing.
async fn assert_awaiting_own_decision(instances: &[Instance], index: usize, tag: &str) {
    let instance = &instances[index];
    let snapshot = own_snapshot(instance, &format!("{tag}-{index}")).await;
    assert_eq!(
        snapshot.lifecycle(),
        RunLifecycle::Active,
        "instance {index} is still live"
    );
    assert_eq!(
        snapshot.pending_approvals(),
        std::slice::from_ref(&instance.approval),
        "instance {index} holds exactly its own pending grant"
    );
    for foreign in instances.iter().filter(|other| other.run != instance.run) {
        assert!(
            !snapshot.pending_approvals().contains(&foreign.approval),
            "instance {index} never lists a foreign approval: {:?}",
            snapshot.pending_approvals()
        );
    }
    assert_eq!(
        instance.bed.write_tool.execution_count(),
        0,
        "instance {index} dispatched nothing"
    );
}

/// Three simultaneously live instances issue pairwise disjoint identities, each
/// stamped with its own runtime's incarnation even though all three sit at
/// the same ordinals, and each one serves only its own pending grant.
#[test]
fn three_instances_issue_pairwise_disjoint_incarnation_scoped_identities() {
    let rt = common::test_rt();
    rt.block_on(async {
        let instances = parked_triple("iso-triple-ids").await;
        assert_eq!(instances.len(), INSTANCES);

        let runs: Vec<&str> = instances.iter().map(|i| i.run.as_str()).collect();
        let calls: Vec<&str> = instances.iter().map(|i| i.call.as_str()).collect();
        let approvals: Vec<&str> = instances.iter().map(|i| i.approval.as_str()).collect();
        assert_pairwise_distinct("run identities", &runs);
        assert_pairwise_distinct("call identities", &calls);
        assert_pairwise_distinct("approval identities", &approvals);

        let mut incarnations = Vec::with_capacity(INSTANCES);
        for (index, instance) in instances.iter().enumerate() {
            let (run_incarnation, run_ordinals) = split_issued(instance.run.as_str(), 'r');
            let (call_incarnation, call_ordinals) = split_issued(instance.call.as_str(), 'c');
            let (approval_incarnation, approval_ordinals) =
                split_issued(instance.approval.as_str(), 'a');
            assert_eq!(
                call_incarnation, run_incarnation,
                "instance {index} issues call identities in its own incarnation"
            );
            assert_eq!(
                approval_incarnation, run_incarnation,
                "instance {index} issues approval identities in its own incarnation"
            );
            // Ordinals restart per instance, so only the incarnation can keep
            // these three apart: each is at its first run, first call, and
            // first approval.
            assert_eq!(run_ordinals, "1", "instance {index} is on its first run");
            assert_eq!(
                call_ordinals, "1-0",
                "instance {index} holds the first call of its first run"
            );
            assert_eq!(
                approval_ordinals, "1-0",
                "instance {index} holds the first approval of its first run"
            );
            incarnations.push(run_incarnation);
        }
        assert_pairwise_distinct("runtime incarnations", &incarnations);

        for index in 0..INSTANCES {
            assert_awaiting_own_decision(&instances, index, "req-triple-ids-own").await;
            // Each instance drove exactly one model turn of its own script, so
            // no provider work crossed between instances.
            assert_eq!(
                instances[index].bed.provider.call_count(),
                1,
                "instance {index} drove one model turn"
            );
        }

        // Every event published so far belongs to its own run and is stale for
        // the other two.
        for (index, instance) in instances.iter().enumerate() {
            let events: Vec<&RunEvent> = instance.prelude.iter().collect();
            let foreign: Vec<&RunId> = instances
                .iter()
                .filter(|other| other.run != instance.run)
                .map(|other| &other.run)
                .collect();
            assert_scoped_to_run(
                &format!("instance {index} prelude"),
                &events,
                &instance.run,
                &foreign,
            );
        }
    });
}

/// A command addressed to another instance's **live** run is unknown to the
/// receiving instance: not finalized, not accepted, and never disturbing the
/// target's run or its pending grant.
#[test]
fn cross_instance_cancel_and_snapshot_are_stale_and_leave_the_target_live() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut instances = parked_triple("iso-triple-live").await;

        for sender in 0..INSTANCES {
            for target in 0..INSTANCES {
                if sender == target {
                    continue;
                }
                let tag = format!("req-triple-live-{sender}-{target}");
                let cancel = instances[sender]
                    .bed
                    .runtime
                    .cancel(CancelCommand {
                        request: request(&tag),
                        run: instances[target].run.clone(),
                    })
                    .await;
                assert_eq!(
                    cancel.reply(),
                    CommandReply::StaleOrUnknownTarget,
                    "instance {sender} cannot cancel instance {target}'s live run"
                );
                assert!(
                    cancel.run().is_none(),
                    "a stale cancel names no run at all, not the target's"
                );
                let (reply, snapshot) = instances[sender]
                    .bed
                    .runtime
                    .get_snapshot(GetSnapshotCommand {
                        request: request(&tag),
                        run: instances[target].run.clone(),
                    })
                    .await;
                assert_eq!(
                    reply.reply(),
                    CommandReply::StaleOrUnknownTarget,
                    "instance {sender} holds no snapshot of instance {target}'s live run"
                );
                assert!(
                    snapshot.is_none(),
                    "instance {target}'s state never leaks into instance {sender}"
                );
            }
        }

        // None of that traffic cancelled or advanced anything.
        for index in 0..INSTANCES {
            assert_awaiting_own_decision(&instances, index, "req-triple-live-own").await;
        }

        // The still-live runs are untouched by the storm: each one completes on
        // its own approval, dispatching only its own call.
        for index in 0..INSTANCES {
            let approve = instances[index]
                .bed
                .runtime
                .approve(ApproveCommand {
                    request: request(&format!("req-triple-live-decide-{index}")),
                    approval: instances[index].approval.clone(),
                    run: instances[index].run.clone(),
                    call: instances[index].call.clone(),
                })
                .await;
            assert_eq!(
                approve.reply(),
                CommandReply::Accepted,
                "instance {index} still accepts its own decision"
            );
            let settled = finish_instance(&mut instances, index).await;
            assert_eq!(
                settled.finished.outcome(),
                RunOutcome::Completed,
                "instance {index} completes after its own approval"
            );
            assert_eq!(
                settled.write_log.len(),
                1,
                "instance {index} dispatched exactly one call"
            );
            assert_eq!(
                settled.write_log[0].call, settled.call,
                "instance {index} dispatched its own host-issued call identity"
            );
        }
    });
}

/// A grant is bound to the exact `(run, call, approval)` triple of the
/// instance that issued it. Every other combination is unknown, so no instance
/// can resolve or dispatch another instance's pending call.
#[test]
fn cross_instance_approve_never_resolves_or_dispatches_a_foreign_call() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut instances = parked_triple("iso-triple-grant").await;

        for receiver in 0..INSTANCES {
            for issuer in 0..INSTANCES {
                if receiver == issuer {
                    continue;
                }
                let foreign_run = instances[issuer].run.clone();
                let foreign_approval = instances[issuer].approval.clone();
                let foreign_call = instances[issuer].call.clone();

                // The complete foreign grant, addressed to the foreign run.
                let whole = instances[receiver]
                    .bed
                    .runtime
                    .approve(ApproveCommand {
                        request: request(&format!("req-triple-grant-whole-{receiver}-{issuer}")),
                        approval: foreign_approval.clone(),
                        run: foreign_run.clone(),
                        call: foreign_call.clone(),
                    })
                    .await;
                assert_eq!(
                    whole.reply(),
                    CommandReply::StaleOrUnknownTarget,
                    "instance {receiver} does not know instance {issuer}'s run"
                );

                // The foreign grant identity welded onto the receiver's own run.
                let mixed = instances[receiver]
                    .bed
                    .runtime
                    .approve(ApproveCommand {
                        request: request(&format!("req-triple-grant-mixed-{receiver}-{issuer}")),
                        approval: foreign_approval.clone(),
                        run: instances[receiver].run.clone(),
                        call: foreign_call.clone(),
                    })
                    .await;
                assert_eq!(
                    mixed.reply(),
                    CommandReply::StaleOrUnknownTarget,
                    "instance {receiver} holds no grant under instance {issuer}'s approval id"
                );

                // The receiver's own approval id bound to a foreign call.
                let swapped = instances[receiver]
                    .bed
                    .runtime
                    .approve(ApproveCommand {
                        request: request(&format!("req-triple-grant-swap-{receiver}-{issuer}")),
                        approval: instances[receiver].approval.clone(),
                        run: instances[receiver].run.clone(),
                        call: foreign_call,
                    })
                    .await;
                assert_eq!(
                    swapped.reply(),
                    CommandReply::StaleOrUnknownTarget,
                    "instance {receiver} never resolves its own grant against a foreign call"
                );
            }
        }

        // Every instance still holds exactly its own undecided grant, so no
        // foreign decision was consumed anywhere.
        for index in 0..INSTANCES {
            assert_awaiting_own_decision(&instances, index, "req-triple-grant-own").await;
        }

        // Each instance still completes on its own grant alone.
        for index in 0..INSTANCES {
            let approve = instances[index]
                .bed
                .runtime
                .approve(ApproveCommand {
                    request: request(&format!("req-triple-grant-decide-{index}")),
                    approval: instances[index].approval.clone(),
                    run: instances[index].run.clone(),
                    call: instances[index].call.clone(),
                })
                .await;
            assert_eq!(
                approve.reply(),
                CommandReply::Accepted,
                "instance {index} accepts its own decision"
            );
            let settled = finish_instance(&mut instances, index).await;
            assert_eq!(
                settled.finished.outcome(),
                RunOutcome::Completed,
                "instance {index} completes after its own approval"
            );
            assert_eq!(
                settled.write_log.len(),
                1,
                "instance {index} dispatched exactly one call"
            );
            assert_eq!(
                settled.write_log[0].call, settled.call,
                "instance {index} dispatched only the call it granted itself"
            );
        }
    });
}

/// Each instance dispatches only its own call, exactly once, and finalizes
/// exactly once with `Completed`. Afterwards its own run reads
/// `AlreadyFinalized` while another instance's finalized run is still unknown
/// to it.
#[test]
fn each_instance_dispatches_its_own_call_once_and_finalizes_exactly_once_completed() {
    let rt = common::test_rt();
    rt.block_on(async {
        let mut instances = parked_triple("iso-triple-final").await;
        let mut settled_all = Vec::with_capacity(INSTANCES);

        // Grant and drain one instance per turn. Every instance still above
        // this one stays ungranted for the whole drain, so its empty tool log
        // is a real observation rather than a race with a background driver.
        for index in 0..INSTANCES {
            let approve = instances[index]
                .bed
                .runtime
                .approve(ApproveCommand {
                    request: request(&format!("req-triple-final-decide-{index}")),
                    approval: instances[index].approval.clone(),
                    run: instances[index].run.clone(),
                    call: instances[index].call.clone(),
                })
                .await;
            assert_eq!(
                approve.reply(),
                CommandReply::Accepted,
                "instance {index} accepts its own decision"
            );
            assert_eq!(
                approve.run(),
                Some(&instances[index].run),
                "the accepted grant names the receiver's own run"
            );

            let one = finish_instance(&mut instances, index).await;

            // Only the instances still waiting on their first decision are
            // live here. Grants ascend by index, so every instance above this
            // one is still undecided; the ones below it were drained on an
            // earlier turn and are finalized by design. Checking the already
            // settled ones would demand `Active` of a run that is legitimately
            // `Finalized(Completed)`, asserting nothing about this turn.
            for other in (index + 1)..INSTANCES {
                assert_awaiting_own_decision(&instances, other, "req-triple-final-live").await;
            }

            // Exactly one dispatch, carrying this instance's own identities and
            // the exact granted arguments.
            assert_eq!(
                one.write_log.len(),
                1,
                "instance {index} dispatched its call exactly once"
            );
            assert_eq!(
                one.write_log[0].call, one.call,
                "the executed call carries instance {index}'s own host-issued identity"
            );
            assert_eq!(
                one.write_log[0].args, GRANTED_ARGS,
                "the executed call carries the granted arguments unchanged"
            );

            // Exactly one start and one outcome event, both naming that call.
            let started: Vec<CallId> = one
                .control
                .iter()
                .chain(one.data.iter())
                .filter_map(|event| match event.payload() {
                    EventPayload::ToolStarted(info) => Some(info.call.clone()),
                    _ => None,
                })
                .collect();
            let completed: Vec<CallId> = one
                .control
                .iter()
                .chain(one.data.iter())
                .filter_map(|event| match event.payload() {
                    EventPayload::ToolFinished(info) => Some(info.call.clone()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                started,
                vec![one.call.clone()],
                "one start for its own call"
            );
            assert_eq!(
                completed,
                vec![one.call.clone()],
                "one outcome for its own call"
            );
            let outcome = one
                .control
                .iter()
                .find_map(|event| match event.payload() {
                    EventPayload::ToolFinished(info) => Some(&info.outcome),
                    _ => None,
                })
                .expect("the granted call reports its outcome");
            assert_eq!(outcome.status(), ExecutionStatus::Succeeded);
            assert_eq!(outcome.effect(), EffectState::KnownApplied);
            assert_eq!(outcome.evidence(), Evidence::HostObserved);

            // Exactly one `Completed` terminal on contiguous per-run sequences,
            // with no data event lost, and no foreign work performed.
            assert_eq!(
                one.finished.outcome(),
                RunOutcome::Completed,
                "instance {index} finalizes as completed"
            );
            common::assert_single_terminal(&one.data, &one.control);
            common::assert_contiguous(&one.data, &one.control);
            assert_eq!(
                one.provider_calls, 2,
                "instance {index} ran its tool turn and its stop turn only"
            );
            assert_eq!(
                one.read_executions, 0,
                "instance {index} never dispatched the read double"
            );
            let requested: Vec<ApprovalId> = one
                .control
                .iter()
                .chain(one.data.iter())
                .filter_map(|event| match event.payload() {
                    EventPayload::ApprovalRequired(notice) => Some(notice.approval.clone()),
                    _ => None,
                })
                .collect();
            assert_eq!(
                requested,
                vec![one.approval.clone()],
                "the single approval request of instance {index} carries its own grant identity"
            );

            // The finalized instance keeps its own honest terminal snapshot.
            let snapshot =
                own_snapshot(&instances[index], &format!("req-triple-final-snap-{index}")).await;
            assert_eq!(
                snapshot.lifecycle(),
                RunLifecycle::Finalized(RunOutcome::Completed),
                "instance {index} reports its finalized outcome"
            );
            assert!(
                snapshot.pending_approvals().is_empty(),
                "instance {index} finalized with no pending grant"
            );

            settled_all.push(one);
        }

        // Every published event of every instance is live for its own run and
        // stale for the other two.
        for one in &settled_all {
            let events: Vec<&RunEvent> = one.data.iter().chain(one.control.iter()).collect();
            let foreign: Vec<&RunId> = settled_all
                .iter()
                .filter(|other| other.run != one.run)
                .map(|other| &other.run)
                .collect();
            assert_scoped_to_run(
                &format!("instance {} stream", one.index),
                &events,
                &one.run,
                &foreign,
            );
        }

        // Late commands: each instance's own finalized run reads finalized,
        // while another instance's finalized run is still unknown to it. A
        // finalized run is per-incarnation knowledge, not a global state.
        for index in 0..INSTANCES {
            let own_cancel = instances[index]
                .bed
                .runtime
                .cancel(CancelCommand {
                    request: request(&format!("req-triple-final-own-cancel-{index}")),
                    run: instances[index].run.clone(),
                })
                .await;
            assert_eq!(
                own_cancel.reply(),
                CommandReply::AlreadyFinalized,
                "instance {index} reports its own run as finalized"
            );
            let own_approve = instances[index]
                .bed
                .runtime
                .approve(ApproveCommand {
                    request: request(&format!("req-triple-final-own-approve-{index}")),
                    approval: instances[index].approval.clone(),
                    run: instances[index].run.clone(),
                    call: instances[index].call.clone(),
                })
                .await;
            assert_eq!(
                own_approve.reply(),
                CommandReply::AlreadyFinalized,
                "instance {index} dispatches no further work for its own finalized run"
            );

            for other in 0..INSTANCES {
                if other == index {
                    continue;
                }
                let foreign_cancel = instances[index]
                    .bed
                    .runtime
                    .cancel(CancelCommand {
                        request: request(&format!("req-triple-final-cross-cancel-{index}-{other}")),
                        run: instances[other].run.clone(),
                    })
                    .await;
                assert_eq!(
                    foreign_cancel.reply(),
                    CommandReply::StaleOrUnknownTarget,
                    "instance {index} never calls instance {other}'s finalized run finalized"
                );
                let (reply, snapshot) = instances[index]
                    .bed
                    .runtime
                    .get_snapshot(GetSnapshotCommand {
                        request: request(&format!("req-triple-final-cross-snap-{index}-{other}")),
                        run: instances[other].run.clone(),
                    })
                    .await;
                assert_eq!(
                    reply.reply(),
                    CommandReply::StaleOrUnknownTarget,
                    "instance {index} holds no snapshot of instance {other}'s finalized run"
                );
                assert!(
                    snapshot.is_none(),
                    "instance {other}'s finalized state never reaches instance {index}"
                );
                let foreign_approve = instances[index]
                    .bed
                    .runtime
                    .approve(ApproveCommand {
                        request: request(&format!(
                            "req-triple-final-cross-approve-{index}-{other}"
                        )),
                        approval: instances[other].approval.clone(),
                        run: instances[other].run.clone(),
                        call: instances[other].call.clone(),
                    })
                    .await;
                assert_eq!(
                    foreign_approve.reply(),
                    CommandReply::StaleOrUnknownTarget,
                    "instance {index} never resolves instance {other}'s spent grant"
                );
            }

            // Nothing in the late traffic moved the recorded dispatch counts.
            assert_eq!(
                instances[index].bed.write_tool.execution_count(),
                1,
                "instance {index} still shows exactly one dispatch"
            );
        }
    });
}
