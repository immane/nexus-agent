#![forbid(unsafe_code)]

//! Coverage hardening: assistant turn and content conversion boundaries.
//!
//! Exercises the public content model only: completeness gating, text and
//! tool-result block bounds, candidate identity validation, block-count
//! limits, continuation bounds, and correlation preservation across
//! admission and conversion. Deterministic: fixed inputs, no clocks, no
//! randomness, standard library only.

use nexus_core::content::MAX_TOOL_NAME_LEN;
use nexus_core::{
    AssistantTurn, CallCandidate, CallId, CompletedTurn, ContentBlock, ContinuationData,
    ErrorCategory, Limits, M0_REVISION, MAX_CONTINUATION_BYTES, MAX_ITEM_KEY_LEN,
    MAX_PROVIDER_REF_LEN, NormalizedArgs, RetryGuidance, RunId, TextContent, ToolCall, ToolId,
    ToolResult, TurnCompleteness, TurnId,
};

fn run_id() -> RunId {
    RunId::new("run-cov").expect("valid run id")
}

fn turn_id() -> TurnId {
    TurnId::new("turn-cov").expect("valid turn id")
}

fn call_id() -> CallId {
    CallId::new("call-cov").expect("valid call id")
}

fn text_block(raw: &str) -> ContentBlock {
    ContentBlock::Text(TextContent::new(raw).expect("valid text block"))
}

#[test]
fn content_bounds_match_documented_values() {
    assert_eq!(MAX_CONTINUATION_BYTES, 65_536);
    assert_eq!(MAX_ITEM_KEY_LEN, 128);
    assert_eq!(MAX_PROVIDER_REF_LEN, 256);
    assert_eq!(MAX_TOOL_NAME_LEN, 64);
}

#[test]
fn partial_turn_is_never_completed_and_reports_invalid_input() {
    let partial = AssistantTurn::new(
        run_id(),
        turn_id(),
        vec![text_block("half")],
        TurnCompleteness::Partial,
    )
    .expect("partial turn builds");

    assert!(!partial.is_complete());
    let error = partial
        .into_completed()
        .expect_err("partial output must not convert");
    assert_eq!(error.category(), ErrorCategory::InvalidInput);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert!(!error.message().is_empty());
}

#[test]
fn complete_turn_converts_and_preserves_identity_and_block_order() {
    let run = run_id();
    let turn = turn_id();
    let complete = AssistantTurn::new(
        run.clone(),
        turn.clone(),
        vec![text_block("first"), text_block("second")],
        TurnCompleteness::Complete,
    )
    .expect("complete turn builds");
    assert!(complete.is_complete());

    let completed: CompletedTurn = complete
        .clone()
        .into_completed()
        .expect("complete converts");
    assert_eq!(completed.inner(), &complete);
    assert_eq!(completed.inner().run(), &run);
    assert_eq!(completed.inner().turn(), &turn);
    assert!(completed.inner().is_complete());
    assert_eq!(completed.inner().blocks().len(), 2);
    match (
        completed.inner().blocks().first(),
        completed.inner().blocks().get(1),
    ) {
        (Some(ContentBlock::Text(first)), Some(ContentBlock::Text(second))) => {
            assert_eq!(first.as_str(), "first");
            assert_eq!(second.as_str(), "second");
        }
        other => panic!("expected two text blocks in order, got {other:?}"),
    }
}

#[test]
fn complete_turn_with_no_blocks_converts() {
    let complete = AssistantTurn::new(run_id(), turn_id(), Vec::new(), TurnCompleteness::Complete)
        .expect("empty turn builds");
    assert!(complete.is_complete());
    let completed = complete
        .into_completed()
        .expect("empty complete turn converts");
    assert!(completed.inner().blocks().is_empty());
}

#[test]
fn text_block_accepts_the_exact_output_budget_and_rejects_oversize_and_empty() {
    let budget = Limits::M0_TEST_TOOL_OUTPUT_BYTES;
    let exact = "x".repeat(budget);
    let text = TextContent::new(exact.clone()).expect("exact budget is accepted");
    assert_eq!(text.as_str().len(), budget);
    assert_eq!(text.as_str(), exact);

    let error = TextContent::new("x".repeat(budget + 1)).expect_err("oversize text is rejected");
    assert_eq!(error.category(), ErrorCategory::InvalidInput);
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry);
    assert!(
        TextContent::new("").is_err(),
        "empty text carries no content"
    );
}

#[test]
fn result_block_preserves_call_correlation_and_truncation_flag() {
    let call = call_id();
    let result = ToolResult::new(call.clone(), "output", true).expect("valid result builds");
    assert_eq!(result.call(), &call);
    assert_eq!(result.content(), "output");
    assert!(result.is_truncated());

    let untruncated =
        ToolResult::new(call_id(), String::new(), false).expect("empty result content is allowed");
    assert_eq!(untruncated.content(), "");
    assert!(!untruncated.is_truncated());

    let budget = Limits::M0_TEST_TOOL_OUTPUT_BYTES;
    let exact = ToolResult::new(call_id(), "x".repeat(budget), false)
        .expect("exact output budget is accepted");
    assert_eq!(exact.content().len(), budget);

    let error = ToolResult::new(call_id(), "x".repeat(budget + 1), false)
        .expect_err("oversize result is rejected");
    assert_eq!(error.category(), ErrorCategory::InvalidInput);
}

#[test]
fn candidate_preserves_item_key_and_provider_ref_exactly() {
    let candidate = CallCandidate::new("item-7", "prov-ref-7", "host_read", r#"{"path":"/tmp/x"}"#)
        .expect("valid candidate builds");
    let clone = candidate.clone();
    assert_eq!(candidate, clone);
    assert_eq!(clone.item_key(), "item-7");
    assert_eq!(clone.provider_ref(), "prov-ref-7");
    assert_eq!(clone.tool_name(), "host_read");
    assert_eq!(clone.arguments_json(), r#"{"path":"/tmp/x"}"#);
}

#[test]
fn candidate_accepts_identity_boundaries_and_rejects_invalid_or_oversize() {
    let max_item = "i".repeat(MAX_ITEM_KEY_LEN);
    let max_ref = "r".repeat(MAX_PROVIDER_REF_LEN);
    let max_tool = "t".repeat(MAX_TOOL_NAME_LEN);
    CallCandidate::new(&max_item, &max_ref, &max_tool, "{}")
        .expect("exact identity boundaries are accepted");

    let cases: Vec<(&str, &str, &str)> = vec![
        ("", &max_ref, &max_tool),
        (&max_item, "", &max_tool),
        (&max_item, &max_ref, ""),
        (&max_item, &max_ref, "host.read"),
        (&max_item, &max_ref, "host read"),
        (&max_item, &max_ref, "host:read"),
    ];
    for (item, reference, tool) in cases {
        let error = CallCandidate::new(item, reference, tool, "{}")
            .expect_err("invalid identity must be rejected");
        assert_eq!(
            error.category(),
            ErrorCategory::InvalidInput,
            "identity case {item:?}/{reference:?}/{tool:?}"
        );
    }

    let over_item = "i".repeat(MAX_ITEM_KEY_LEN + 1);
    let over_ref = "r".repeat(MAX_PROVIDER_REF_LEN + 1);
    let over_tool = "t".repeat(MAX_TOOL_NAME_LEN + 1);
    assert!(CallCandidate::new(&over_item, &max_ref, &max_tool, "{}").is_err());
    assert!(CallCandidate::new(&max_item, &over_ref, &max_tool, "{}").is_err());
    assert!(CallCandidate::new(&max_item, &max_ref, &over_tool, "{}").is_err());

    let budget = Limits::M0_TEST_ARG_ASSEMBLY_BYTES;
    let exact_args = "x".repeat(budget);
    let candidate = CallCandidate::new("item-0", "prov-ref-0", "host_read", exact_args.clone())
        .expect("exact assembly budget is accepted");
    assert_eq!(candidate.arguments_json().len(), budget);
    assert_eq!(candidate.arguments_json(), exact_args);
    let error = CallCandidate::new("item-0", "prov-ref-0", "host_read", "x".repeat(budget + 1))
        .expect_err("oversize arguments are rejected");
    assert_eq!(error.category(), ErrorCategory::InvalidInput);
}

#[test]
fn turn_block_count_boundary_matches_retained_context_budget() {
    let budget = Limits::M0_TEST_RETAINED_CONTEXT_ITEMS;
    let blocks: Vec<ContentBlock> = (0..budget).map(|_| text_block("x")).collect();
    let turn = AssistantTurn::new(run_id(), turn_id(), blocks, TurnCompleteness::Partial)
        .expect("exact block budget is accepted");
    assert_eq!(turn.blocks().len(), budget);

    let over: Vec<ContentBlock> = (0..=budget).map(|_| text_block("x")).collect();
    let error = AssistantTurn::new(run_id(), turn_id(), over, TurnCompleteness::Complete)
        .expect_err("over-budget block count is rejected");
    assert_eq!(error.category(), ErrorCategory::InvalidInput);
}

#[test]
fn continuation_preserves_opaque_bytes_and_exact_compatibility() {
    let adapter = "adapter-a".to_owned();
    let scope = "scope-a".to_owned();
    let bytes = vec![0x00, 0x01, 0x7F, 0x80, 0xFE, 0xFF];
    let state = ContinuationData::new(adapter.clone(), scope.clone(), bytes.clone())
        .expect("valid continuation builds");
    assert_eq!(state.adapter(), adapter.as_str());
    assert_eq!(state.scope(), scope.as_str());
    assert_eq!(state.bytes(), bytes.as_slice());
    assert!(state.is_compatible_with(&adapter, &scope));
    assert!(!state.is_compatible_with(&adapter, "scope-b"));
    assert!(!state.is_compatible_with("adapter-b", &scope));

    let exact = ContinuationData::new(
        "a".repeat(MAX_ITEM_KEY_LEN),
        "s".repeat(MAX_ITEM_KEY_LEN),
        vec![0u8; MAX_CONTINUATION_BYTES],
    )
    .expect("exact continuation bounds are accepted");
    assert_eq!(exact.bytes().len(), MAX_CONTINUATION_BYTES);

    let over = ContinuationData::new("a", "s", vec![0u8; MAX_CONTINUATION_BYTES + 1])
        .expect_err("over-bound continuation is rejected");
    assert_eq!(over.category(), ErrorCategory::InvalidInput);
    assert!(ContinuationData::new("", "s", Vec::new()).is_err());
    assert!(ContinuationData::new("a", "", Vec::new()).is_err());
    assert!(
        ContinuationData::new("a".repeat(MAX_ITEM_KEY_LEN + 1), "s", Vec::new()).is_err(),
        "overlong adapter label is rejected"
    );
    assert!(
        ContinuationData::new("a", "s".repeat(MAX_ITEM_KEY_LEN + 1), Vec::new()).is_err(),
        "overlong scope label is rejected"
    );
}

#[test]
fn mixed_turn_conversion_preserves_every_block_and_correlation() {
    let run = run_id();
    let turn = turn_id();
    let call = call_id();
    let tool = ToolId::new("host_read", M0_REVISION).expect("valid tool id");
    let args = NormalizedArgs::new(r#"{"path":"/tmp/x"}"#).expect("valid normalized args");
    let admitted = ToolCall::new(
        run.clone(),
        turn.clone(),
        call.clone(),
        tool.clone(),
        args.clone(),
    );
    let candidate = CallCandidate::new("item-9", "prov-ref-9", "host_read", r#"{"path":"/tmp/x"}"#)
        .expect("valid candidate builds");
    let result =
        ToolResult::new(call.clone(), "file contents", false).expect("valid result builds");
    let continuation = ContinuationData::new("adapter-a", "scope-a", vec![9, 8, 7])
        .expect("valid continuation builds");

    let blocks = vec![
        text_block("assistant text"),
        ContentBlock::CallProposal(candidate.clone()),
        ContentBlock::Call(admitted.clone()),
        ContentBlock::Result(result.clone()),
        ContentBlock::Continuation(continuation.clone()),
    ];
    let completed = AssistantTurn::new(
        run.clone(),
        turn.clone(),
        blocks,
        TurnCompleteness::Complete,
    )
    .expect("mixed complete turn builds")
    .into_completed()
    .expect("mixed complete turn converts");
    let inner = completed.inner();
    assert_eq!(inner.run(), &run);
    assert_eq!(inner.turn(), &turn);
    assert_eq!(inner.blocks().len(), 5);
    assert_eq!(completed, completed.clone());

    match &inner.blocks()[0] {
        ContentBlock::Text(text) => assert_eq!(text.as_str(), "assistant text"),
        other => panic!("expected text block, got {other:?}"),
    }
    match &inner.blocks()[1] {
        ContentBlock::CallProposal(carried) => {
            assert_eq!(carried, &candidate);
            assert_eq!(carried.item_key(), "item-9");
            assert_eq!(carried.provider_ref(), "prov-ref-9");
        }
        other => panic!("expected call proposal, got {other:?}"),
    }
    match &inner.blocks()[2] {
        ContentBlock::Call(carried) => {
            assert_eq!(carried, &admitted);
            assert_eq!(carried.run(), &run);
            assert_eq!(carried.turn(), &turn);
            assert_eq!(carried.call(), &call);
            assert_eq!(carried.tool(), &tool);
            assert_eq!(carried.args(), &args);
        }
        other => panic!("expected admitted call, got {other:?}"),
    }
    match &inner.blocks()[3] {
        ContentBlock::Result(carried) => {
            assert_eq!(carried, &result);
            assert_eq!(
                carried.call(),
                &call,
                "result must correlate to the admitted call"
            );
            assert_eq!(carried.content(), "file contents");
            assert!(!carried.is_truncated());
        }
        other => panic!("expected result block, got {other:?}"),
    }
    match &inner.blocks()[4] {
        ContentBlock::Continuation(carried) => {
            assert_eq!(carried, &continuation);
            assert!(carried.is_compatible_with("adapter-a", "scope-a"));
            assert_eq!(carried.bytes(), &[9, 8, 7]);
        }
        other => panic!("expected continuation block, got {other:?}"),
    }
}
