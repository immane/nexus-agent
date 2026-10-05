#![forbid(unsafe_code)]

//! Public-boundary coverage for `ModelContextItem` and conversation bounds.
//!
//! Everything here goes through `nexus_core`'s public API: item builders
//! reject empty and oversize text, item keys, and provider references;
//! `ModelContextItem::payload_bytes` counts every owned string; and
//! `ModelRequest::with_conversation` enforces the retained-item count and
//! aggregate payload bounds, including per-item host metadata (run, turn,
//! call, tool name, and arguments) that is easy to omit. No adapter,
//! runtime, clock, or randomness is involved, so every assertion is
//! deterministic.

use nexus_core::{
    AgentError, CallId, EffectState, ErrorCategory, Evidence, ExecutionStatus, ItemKey, Limits,
    M0_REVISION, MAX_CONVERSATION_BYTES, MAX_CONVERSATION_ITEMS, MAX_ITEM_KEY_LEN,
    MAX_PROVIDER_REF_LEN, ModelContextItem, ModelRequest, NormalizedArgs, ProviderRef,
    RetryGuidance, RunId, ToolCall, ToolId, ToolOutcome, TurnId,
};

const OUTPUT_BUDGET: usize = 1024;
const TEXT_BUDGET: usize = Limits::M0_TEST_TOOL_OUTPUT_BYTES;
/// 128 items of 8192 bytes fill both maxima exactly.
const CHUNK: usize = MAX_CONVERSATION_BYTES / MAX_CONVERSATION_ITEMS;
/// Owned identity bytes of `result_item` beyond content: host call, item key,
/// provider reference, and tool name.
const RESULT_METADATA_BYTES: usize =
    "call-1".len() + "item-1".len() + "prov-ref-1".len() + "host_read".len();

fn run() -> RunId {
    RunId::new("run-1").expect("valid run id")
}

fn turn() -> TurnId {
    TurnId::new("turn-1").expect("valid turn id")
}

fn call_id() -> CallId {
    CallId::new("call-1").expect("valid call id")
}

fn tool_id() -> ToolId {
    ToolId::new("host_read", M0_REVISION).expect("valid tool id")
}

fn request() -> ModelRequest {
    ModelRequest::new(
        run(),
        turn(),
        "test-profile",
        vec![tool_id()],
        None,
        OUTPUT_BUDGET,
    )
    .expect("valid request builds")
}

fn tool_call() -> ToolCall {
    ToolCall::new(
        run(),
        turn(),
        call_id(),
        tool_id(),
        NormalizedArgs::new(r#"{"path":"src"}"#).expect("valid arguments"),
    )
}

fn outcome(content: &str) -> ToolOutcome {
    ToolOutcome::new(
        ExecutionStatus::Succeeded,
        EffectState::KnownApplied,
        Evidence::HostObserved,
        content,
        false,
    )
    .expect("bounded outcome builds")
}

fn result_item(content: &str) -> ModelContextItem {
    ModelContextItem::tool_result(
        call_id(),
        "item-1",
        "prov-ref-1",
        tool_id(),
        outcome(content),
    )
    .expect("bounded result item builds")
}

/// One small item per kind, cycling by index, so count-bound tests exercise
/// every variant rather than only user text.
fn mixed_item(index: usize) -> ModelContextItem {
    match index % 4 {
        0 => ModelContextItem::user_text("x").expect("user text builds"),
        1 => ModelContextItem::assistant_text("k", "x").expect("assistant text builds"),
        2 => {
            ModelContextItem::assistant_call("k", "r", tool_call()).expect("assistant call builds")
        }
        _ => result_item(""),
    }
}

fn assert_invalid(error: &AgentError) {
    assert_eq!(error.category(), ErrorCategory::InvalidInput, "{error}");
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry, "{error}");
    assert!(!error.message().is_empty(), "diagnostics stay non-empty");
}

fn assert_limit(error: &AgentError, needle: &str) {
    assert_eq!(error.category(), ErrorCategory::ResourceLimit, "{error}");
    assert_eq!(error.retry(), RetryGuidance::DoNotRetry, "{error}");
    assert!(
        error.message().contains(needle),
        "message {:?} should contain {needle:?}",
        error.message()
    );
}

#[test]
fn raw_ref_constructors_reject_empty_and_oversize() {
    assert_invalid(&ItemKey::new("").expect_err("empty item key is rejected"));
    assert_invalid(
        &ItemKey::new("k".repeat(MAX_ITEM_KEY_LEN + 1)).expect_err("oversize item key is rejected"),
    );
    assert_eq!(
        ItemKey::new("k".repeat(MAX_ITEM_KEY_LEN))
            .expect("max item key builds")
            .as_str()
            .len(),
        MAX_ITEM_KEY_LEN
    );

    assert_invalid(&ProviderRef::new("").expect_err("empty provider ref is rejected"));
    assert_invalid(
        &ProviderRef::new("r".repeat(MAX_PROVIDER_REF_LEN + 1))
            .expect_err("oversize provider ref is rejected"),
    );
    assert_eq!(
        ProviderRef::new("r".repeat(MAX_PROVIDER_REF_LEN))
            .expect("max provider ref builds")
            .as_str()
            .len(),
        MAX_PROVIDER_REF_LEN
    );
}

#[test]
fn user_text_rejects_empty_and_oversize_and_counts_its_bytes() {
    assert_invalid(&ModelContextItem::user_text("").expect_err("empty user text is rejected"));

    let exact = ModelContextItem::user_text("x".repeat(TEXT_BUDGET)).expect("text at the budget");
    assert_eq!(exact.payload_bytes(), TEXT_BUDGET);

    assert_invalid(
        &ModelContextItem::user_text("x".repeat(TEXT_BUDGET + 1))
            .expect_err("text over the budget is rejected"),
    );
}

#[test]
fn assistant_text_bounds_key_and_text_and_counts_both() {
    assert_invalid(
        &ModelContextItem::assistant_text("", "text").expect_err("empty key is rejected"),
    );
    assert_invalid(
        &ModelContextItem::assistant_text("k".repeat(MAX_ITEM_KEY_LEN + 1), "text")
            .expect_err("oversize key is rejected"),
    );
    assert_invalid(
        &ModelContextItem::assistant_text("key", "").expect_err("empty text is rejected"),
    );

    let key = "k".repeat(MAX_ITEM_KEY_LEN);
    let text = "x".repeat(TEXT_BUDGET);
    let item =
        ModelContextItem::assistant_text(key.clone(), text.clone()).expect("boundary values build");
    match &item {
        ModelContextItem::AssistantText {
            item_key,
            text: carried,
            reasoning,
        } => {
            assert_eq!(item_key.as_str(), key);
            assert_eq!(carried.as_str(), text);
            assert_eq!(reasoning, &None);
        }
        other => panic!("unexpected item {other:?}"),
    }
    assert_eq!(item.payload_bytes(), key.len() + text.len());
}

#[test]
fn assistant_call_bounds_refs_and_counts_owned_metadata() {
    let call = tool_call();
    assert_invalid(
        &ModelContextItem::assistant_call("", "prov-ref-1", call.clone())
            .expect_err("empty key is rejected"),
    );
    assert_invalid(
        &ModelContextItem::assistant_call("item-1", "", call.clone())
            .expect_err("empty ref is rejected"),
    );
    assert_invalid(
        &ModelContextItem::assistant_call(
            "k".repeat(MAX_ITEM_KEY_LEN + 1),
            "prov-ref-1",
            call.clone(),
        )
        .expect_err("oversize key is rejected"),
    );
    assert_invalid(
        &ModelContextItem::assistant_call(
            "item-1",
            "r".repeat(MAX_PROVIDER_REF_LEN + 1),
            call.clone(),
        )
        .expect_err("oversize ref is rejected"),
    );

    let key = "k".repeat(MAX_ITEM_KEY_LEN);
    let reference = "r".repeat(MAX_PROVIDER_REF_LEN);
    let item = ModelContextItem::assistant_call(key.clone(), reference.clone(), call.clone())
        .expect("boundary key and ref build");
    match &item {
        ModelContextItem::AssistantCall {
            item_key,
            provider_ref,
            call: carried,
            reasoning,
        } => {
            assert_eq!(item_key.as_str(), key);
            assert_eq!(provider_ref.as_str(), reference);
            assert_eq!(carried, &call);
            assert_eq!(reasoning, &None);
        }
        other => panic!("unexpected item {other:?}"),
    }
    assert_eq!(
        item.payload_bytes(),
        key.len()
            + reference.len()
            + call.run().as_str().len()
            + call.turn().as_str().len()
            + call.call().as_str().len()
            + call.tool().name().len()
            + call.args().as_str().len()
    );
}

#[test]
fn tool_result_bounds_refs_and_counts_host_metadata() {
    let empty_content = result_item("");
    assert_eq!(
        empty_content.payload_bytes(),
        RESULT_METADATA_BYTES,
        "empty content is allowed and costs metadata only"
    );

    assert_invalid(
        &ModelContextItem::tool_result(call_id(), "", "prov-ref-1", tool_id(), outcome("ok"))
            .expect_err("empty key is rejected"),
    );
    assert_invalid(
        &ModelContextItem::tool_result(call_id(), "item-1", "", tool_id(), outcome("ok"))
            .expect_err("empty ref is rejected"),
    );
    assert_invalid(
        &ModelContextItem::tool_result(
            call_id(),
            "k".repeat(MAX_ITEM_KEY_LEN + 1),
            "prov-ref-1",
            tool_id(),
            outcome("ok"),
        )
        .expect_err("oversize key is rejected"),
    );
    assert_invalid(
        &ModelContextItem::tool_result(
            call_id(),
            "item-1",
            "r".repeat(MAX_PROVIDER_REF_LEN + 1),
            tool_id(),
            outcome("ok"),
        )
        .expect_err("oversize ref is rejected"),
    );

    let content = "x".repeat(TEXT_BUDGET);
    let item = ModelContextItem::tool_result(
        call_id(),
        "item-1",
        "prov-ref-1",
        tool_id(),
        outcome(&content),
    )
    .expect("result at the content budget builds");
    match &item {
        ModelContextItem::ToolResult {
            call,
            item_key,
            provider_ref,
            tool,
            outcome: carried,
        } => {
            assert_eq!(call, &call_id());
            assert_eq!(item_key.as_str(), "item-1");
            assert_eq!(provider_ref.as_str(), "prov-ref-1");
            assert_eq!(tool, &tool_id());
            assert_eq!(carried.status(), ExecutionStatus::Succeeded);
            assert_eq!(carried.effect(), EffectState::KnownApplied);
            assert_eq!(carried.evidence(), Evidence::HostObserved);
            assert_eq!(carried.content(), content.as_str());
            assert!(!carried.is_truncated());
        }
        other => panic!("unexpected item {other:?}"),
    }
    assert_eq!(item.payload_bytes(), RESULT_METADATA_BYTES + TEXT_BUDGET);
}

#[test]
fn refs_use_byte_bounds_and_preserve_text_exactly() {
    // The key bound is bytes, not characters: 64 two-byte characters are
    // exactly 128 bytes and accepted; 65 characters are 130 bytes.
    let max_key = "é".repeat(MAX_ITEM_KEY_LEN / 2);
    assert_eq!(max_key.len(), MAX_ITEM_KEY_LEN);
    assert!(ModelContextItem::assistant_text(max_key, "x").is_ok());
    assert_invalid(
        &ModelContextItem::assistant_text("é".repeat(MAX_ITEM_KEY_LEN / 2 + 1), "x")
            .expect_err("130 multibyte key bytes exceed the 128-byte bound"),
    );

    // The reference bound is bytes too: 128 two-byte characters are 256
    // bytes; 129 characters are 258 bytes.
    let max_ref = "é".repeat(MAX_PROVIDER_REF_LEN / 2);
    assert_eq!(max_ref.len(), MAX_PROVIDER_REF_LEN);
    assert!(ModelContextItem::assistant_call("item-1", max_ref, tool_call()).is_ok());
    assert_invalid(
        &ModelContextItem::assistant_call(
            "item-1",
            "é".repeat(MAX_PROVIDER_REF_LEN / 2 + 1),
            tool_call(),
        )
        .expect_err("258 multibyte ref bytes exceed the 256-byte bound"),
    );

    // Keys and refs are preserved exactly; no trimming or normalization
    // happens at this boundary.
    let item = ModelContextItem::assistant_text(" item-1 ", " hällo ").expect("spaced text builds");
    match &item {
        ModelContextItem::AssistantText {
            item_key,
            text,
            reasoning,
        } => {
            assert_eq!(item_key.as_str(), " item-1 ");
            assert_eq!(text.as_str(), " hällo ");
            assert_eq!(reasoning, &None);
        }
        other => panic!("unexpected item {other:?}"),
    }
}

#[test]
fn payload_bytes_matches_the_documented_owned_strings() {
    assert_eq!(
        ModelContextItem::user_text("hello")
            .expect("builds")
            .payload_bytes(),
        "hello".len()
    );

    let text = ModelContextItem::assistant_text("item-1", "hello").expect("builds");
    assert_eq!(text.payload_bytes(), "item-1".len() + "hello".len());

    let call = tool_call();
    let assistant =
        ModelContextItem::assistant_call("item-1", "prov-ref-1", call.clone()).expect("builds");
    assert_eq!(
        assistant.payload_bytes(),
        "item-1".len()
            + "prov-ref-1".len()
            + "run-1".len()
            + "turn-1".len()
            + "call-1".len()
            + "host_read".len()
            + r#"{"path":"src"}"#.len()
    );

    let result = result_item("listed src");
    assert_eq!(
        result.payload_bytes(),
        "call-1".len()
            + "item-1".len()
            + "prov-ref-1".len()
            + "host_read".len()
            + "listed src".len()
    );
    assert_eq!(
        result.payload_bytes(),
        RESULT_METADATA_BYTES + "listed src".len()
    );

    let longer_args = NormalizedArgs::new(r#"{"path":"src","depth":1}"#).expect("valid arguments");
    let longer = ModelContextItem::assistant_call(
        "item-1",
        "prov-ref-1",
        ToolCall::new(run(), turn(), call_id(), tool_id(), longer_args),
    )
    .expect("builds");
    assert_eq!(
        longer.payload_bytes() - assistant.payload_bytes(),
        r#"{"path":"src","depth":1}"#.len() - r#"{"path":"src"}"#.len(),
        "argument bytes are owned metadata and move the payload"
    );
}

#[test]
fn conversation_accepts_count_and_bytes_exactly_at_their_maxima() {
    let items: Vec<ModelContextItem> = (0..MAX_CONVERSATION_ITEMS)
        .map(|_| ModelContextItem::user_text("x".repeat(CHUNK)).expect("chunk builds"))
        .collect();
    assert_eq!(
        items
            .iter()
            .map(ModelContextItem::payload_bytes)
            .sum::<usize>(),
        MAX_CONVERSATION_BYTES
    );

    let request = request()
        .with_conversation(items)
        .expect("both maxima are inclusive");
    assert_eq!(request.conversation().len(), MAX_CONVERSATION_ITEMS);
    assert_eq!(request.conversation()[0].payload_bytes(), CHUNK);
    assert_eq!(
        request.conversation()[MAX_CONVERSATION_ITEMS - 1].payload_bytes(),
        CHUNK
    );
}

#[test]
fn conversation_rejects_one_over_each_bound_independently() {
    // Count over, bytes far under: every item kind still counts.
    let too_many: Vec<ModelContextItem> = (0..=MAX_CONVERSATION_ITEMS).map(mixed_item).collect();
    assert!(
        too_many
            .iter()
            .map(ModelContextItem::payload_bytes)
            .sum::<usize>()
            < MAX_CONVERSATION_BYTES,
        "the count rejection must not be a byte rejection"
    );
    assert_limit(
        &request()
            .with_conversation(too_many)
            .expect_err("count over the bound is rejected"),
        "retained context",
    );

    // Bytes over by exactly one, count exactly at the bound.
    let mut over_bytes: Vec<ModelContextItem> = (0..MAX_CONVERSATION_ITEMS - 1)
        .map(|_| ModelContextItem::user_text("x".repeat(CHUNK)).expect("chunk builds"))
        .collect();
    over_bytes.push(ModelContextItem::user_text("x".repeat(CHUNK + 1)).expect("chunk builds"));
    assert_eq!(over_bytes.len(), MAX_CONVERSATION_ITEMS);
    assert_eq!(
        over_bytes
            .iter()
            .map(ModelContextItem::payload_bytes)
            .sum::<usize>(),
        MAX_CONVERSATION_BYTES + 1,
        "the byte rejection must be exactly one byte over"
    );
    assert_limit(
        &request()
            .with_conversation(over_bytes)
            .expect_err("aggregate over the bound is rejected"),
        "aggregate",
    );
}

#[test]
fn aggregate_byte_bound_counts_every_item_kinds_metadata() {
    // Four maximum-size texts fill the aggregate budget exactly with no
    // metadata; each extra item must still be rejected because its owned
    // identity bytes count against the same budget.
    let base = || -> Vec<ModelContextItem> {
        (0..MAX_CONVERSATION_BYTES / TEXT_BUDGET)
            .map(|_| ModelContextItem::user_text("x".repeat(TEXT_BUDGET)).expect("text builds"))
            .collect()
    };
    assert_eq!(base().len(), 4);
    assert_eq!(
        base()
            .iter()
            .map(ModelContextItem::payload_bytes)
            .sum::<usize>(),
        MAX_CONVERSATION_BYTES
    );
    request()
        .with_conversation(base())
        .expect("text-only conversation at the cap is accepted");

    let extras = [
        (
            "assistant text key",
            ModelContextItem::assistant_text("k", "x").expect("builds"),
            "k".len() + "x".len(),
        ),
        (
            "assistant call metadata",
            ModelContextItem::assistant_call("k", "r", tool_call()).expect("builds"),
            "k".len()
                + "r".len()
                + "run-1".len()
                + "turn-1".len()
                + "call-1".len()
                + "host_read".len()
                + r#"{"path":"src"}"#.len(),
        ),
        (
            "tool result metadata",
            result_item(""),
            RESULT_METADATA_BYTES,
        ),
    ];
    for (name, extra, extra_bytes) in extras {
        assert_eq!(extra.payload_bytes(), extra_bytes, "{name}");
        let mut items = base();
        items.push(extra);
        assert_eq!(
            items
                .iter()
                .map(ModelContextItem::payload_bytes)
                .sum::<usize>(),
            MAX_CONVERSATION_BYTES + extra_bytes,
            "{name}"
        );
        assert_limit(
            &request()
                .with_conversation(items)
                .expect_err("metadata tips the aggregate over"),
            "aggregate",
        );
    }
}

#[test]
fn aggregate_byte_bound_counts_result_host_metadata() {
    // Per result item, host metadata beyond content is call-1 + item-1 +
    // prov-ref-1 + host_read = 31 bytes. Four items land exactly on the
    // aggregate cap only when that metadata is counted.
    let exact_content = MAX_CONVERSATION_BYTES / 4 - RESULT_METADATA_BYTES;
    let exact: Vec<ModelContextItem> = (0..4)
        .map(|_| result_item(&"x".repeat(exact_content)))
        .collect();
    assert_eq!(
        exact
            .iter()
            .map(ModelContextItem::payload_bytes)
            .sum::<usize>(),
        MAX_CONVERSATION_BYTES
    );
    request()
        .with_conversation(exact)
        .expect("metadata-inclusive aggregate at the cap is accepted");

    // Content plus item key plus provider ref alone would fit; the host call
    // and tool-name bytes push the full aggregate over, so this must be
    // rejected. This is the regression the metadata-inclusive count guards.
    let undercounted_content = MAX_CONVERSATION_BYTES / 4 - 20;
    let undercounted: Vec<ModelContextItem> = (0..4)
        .map(|_| result_item(&"x".repeat(undercounted_content)))
        .collect();
    let full: usize = undercounted
        .iter()
        .map(ModelContextItem::payload_bytes)
        .sum();
    let host_metadata = 4 * ("call-1".len() + "host_read".len());
    assert!(
        full - host_metadata <= MAX_CONVERSATION_BYTES,
        "content plus item key plus provider ref alone would fit"
    );
    assert!(
        full > MAX_CONVERSATION_BYTES,
        "the full metadata-inclusive count exceeds"
    );
    assert_eq!(full, MAX_CONVERSATION_BYTES + 44);
    assert_limit(
        &request()
            .with_conversation(undercounted)
            .expect_err("host metadata is part of the aggregate"),
        "aggregate",
    );

    let one_over: Vec<ModelContextItem> = (0..4)
        .map(|_| result_item(&"x".repeat(exact_content + 1)))
        .collect();
    assert_eq!(
        one_over
            .iter()
            .map(ModelContextItem::payload_bytes)
            .sum::<usize>(),
        MAX_CONVERSATION_BYTES + 4
    );
    assert_limit(
        &request()
            .with_conversation(one_over)
            .expect_err("four metadata bytes over the cap are rejected"),
        "aggregate",
    );
}

#[test]
fn assistant_call_and_tool_result_preserve_correlated_identities() {
    let call = tool_call();
    let assistant =
        ModelContextItem::assistant_call("item-1", "prov-ref-1", call.clone()).expect("builds");
    let result = ModelContextItem::tool_result(
        call.call().clone(),
        "item-1",
        "prov-ref-1",
        call.tool().clone(),
        outcome("listed src"),
    )
    .expect("builds");

    let accepted = request()
        .with_conversation(vec![assistant, result])
        .expect("correlated pair builds");
    assert_eq!(accepted.run(), call.run());
    assert_eq!(accepted.turn(), call.turn());
    assert_eq!(accepted.conversation().len(), 2);

    match &accepted.conversation()[0] {
        ModelContextItem::AssistantCall {
            item_key,
            provider_ref,
            call: carried,
            reasoning,
        } => {
            assert_eq!(item_key.as_str(), "item-1");
            assert_eq!(provider_ref.as_str(), "prov-ref-1");
            assert_eq!(carried, &call);
            assert_eq!(reasoning, &None);
        }
        other => panic!("unexpected item {other:?}"),
    }
    match &accepted.conversation()[1] {
        ModelContextItem::ToolResult {
            call: result_call,
            item_key,
            provider_ref,
            tool,
            outcome: carried,
        } => {
            assert_eq!(result_call, call.call());
            assert_eq!(item_key.as_str(), "item-1");
            assert_eq!(provider_ref.as_str(), "prov-ref-1");
            assert_eq!(
                tool,
                call.tool(),
                "tool identity plus revision is preserved"
            );
            assert_eq!(carried.content(), "listed src");
        }
        other => panic!("unexpected item {other:?}"),
    }

    // Distinct provider keys and refs stay distinct; construction does not
    // alias one item's correlation onto another.
    let second =
        ModelContextItem::assistant_call("item-2", "prov-ref-2", call.clone()).expect("builds");
    let pair = request()
        .with_conversation(vec![
            ModelContextItem::assistant_call("item-1", "prov-ref-1", call).expect("builds"),
            second,
        ])
        .expect("pair builds");
    match (&pair.conversation()[0], &pair.conversation()[1]) {
        (
            ModelContextItem::AssistantCall {
                item_key: first_key,
                provider_ref: first_ref,
                ..
            },
            ModelContextItem::AssistantCall {
                item_key: second_key,
                provider_ref: second_ref,
                ..
            },
        ) => {
            assert_eq!(first_key.as_str(), "item-1");
            assert_eq!(first_ref.as_str(), "prov-ref-1");
            assert_eq!(second_key.as_str(), "item-2");
            assert_eq!(second_ref.as_str(), "prov-ref-2");
        }
        other => panic!("unexpected item pair {other:?}"),
    }
}

#[test]
fn with_conversation_replaces_previous_items_and_accepts_empty() {
    let first = request()
        .with_conversation(vec![ModelContextItem::user_text("first").expect("builds")])
        .expect("builds");
    assert_eq!(first.conversation().len(), 1);

    let second = first
        .with_conversation(vec![
            ModelContextItem::assistant_text("item-1", "second").expect("builds"),
            result_item("listed src"),
        ])
        .expect("replacement builds");
    assert_eq!(second.conversation().len(), 2);
    assert_eq!(second.run(), &run());
    assert_eq!(second.turn(), &turn());
    assert_eq!(second.profile(), "test-profile");
    assert_eq!(second.output_budget_bytes(), OUTPUT_BUDGET);
    match &second.conversation()[0] {
        ModelContextItem::AssistantText {
            item_key,
            text,
            reasoning,
        } => {
            assert_eq!(item_key.as_str(), "item-1");
            assert_eq!(text.as_str(), "second");
            assert_eq!(reasoning, &None);
        }
        other => panic!("stale item survived replacement: {other:?}"),
    }
    match &second.conversation()[1] {
        ModelContextItem::ToolResult {
            item_key,
            provider_ref,
            ..
        } => {
            assert_eq!(item_key.as_str(), "item-1");
            assert_eq!(provider_ref.as_str(), "prov-ref-1");
        }
        other => panic!("unexpected item {other:?}"),
    }

    let cleared = second
        .with_conversation(Vec::new())
        .expect("empty conversation builds");
    assert!(
        cleared.conversation().is_empty(),
        "an empty replacement clears the conversation"
    );
}
