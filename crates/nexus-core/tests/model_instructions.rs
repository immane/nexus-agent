#![forbid(unsafe_code)]

use nexus_core::{
    ErrorCategory, Limits, MAX_CONTEXT_NOTE_BYTES, MAX_CONVERSATION_BYTES, MAX_INSTRUCTIONS_BYTES,
    ModelContextItem, ModelInstructions, ModelRequest, RunId, TurnId,
};

fn request() -> ModelRequest {
    ModelRequest::new(
        RunId::new("run-1").unwrap(),
        TurnId::new("turn-1").unwrap(),
        "test-profile",
        vec![],
        None,
        1024,
    )
    .unwrap()
}

fn conversation(bytes: usize) -> Vec<ModelContextItem> {
    let mut remaining = bytes;
    let mut items = Vec::new();
    while remaining > 0 {
        let size = remaining.min(Limits::M0_TEST_TOOL_OUTPUT_BYTES);
        items.push(ModelContextItem::user_text("x".repeat(size)).unwrap());
        remaining -= size;
    }
    items
}

#[test]
fn instructions_default_and_request_default_are_empty() {
    let instructions = ModelInstructions::default();
    assert_eq!(instructions.agent(), "");
    assert_eq!(instructions.environment(), "");
    assert_eq!(instructions.payload_bytes(), 0);
    assert_eq!(instructions.rendered_text(), "");
    assert_eq!(instructions, ModelInstructions::new("", "").unwrap());
    assert_eq!(request().instructions(), &instructions);
}

#[test]
fn rendering_preserves_contents_and_skips_only_empty_fields() {
    for (agent, environment, rendered) in [
        ("agent", "", "agent"),
        ("", "environment", "environment"),
        ("agent", "environment", "agent\n\nenvironment"),
        (" \n", "é", " \n\n\né"),
    ] {
        let instructions = ModelInstructions::new(agent, environment).unwrap();
        assert_eq!(instructions.agent(), agent);
        assert_eq!(instructions.environment(), environment);
        assert_eq!(instructions.payload_bytes(), rendered.len());
        assert_eq!(instructions.rendered_text(), rendered);
        assert_eq!(
            request()
                .with_instructions(instructions.clone())
                .unwrap()
                .instructions(),
            &instructions
        );
    }
}

#[test]
fn instructions_enforce_aggregate_utf8_byte_limit() {
    assert_eq!(MAX_INSTRUCTIONS_BYTES, 16 * 1024);
    for (agent, environment) in [
        ("a".repeat(MAX_INSTRUCTIONS_BYTES), String::new()),
        (String::new(), "e".repeat(MAX_INSTRUCTIONS_BYTES)),
        (
            "é".repeat(MAX_INSTRUCTIONS_BYTES / 4),
            "e".repeat(MAX_INSTRUCTIONS_BYTES / 2 - 2),
        ),
    ] {
        let instructions = ModelInstructions::new(&agent, &environment).unwrap();
        assert_eq!(instructions.payload_bytes(), MAX_INSTRUCTIONS_BYTES);
        let error = ModelInstructions::new(agent, format!("{environment}x")).unwrap_err();
        assert_eq!(error.category(), ErrorCategory::ResourceLimit);
    }
    assert!(ModelInstructions::new("é".repeat(MAX_INSTRUCTIONS_BYTES / 2 + 1), "").is_err());
}

#[test]
fn instructions_and_conversation_share_budget_in_both_builder_orders() {
    let instructions = ModelInstructions::new("a".repeat(MAX_INSTRUCTIONS_BYTES), "").unwrap();
    let available = MAX_CONVERSATION_BYTES - instructions.payload_bytes();
    for bytes in [available, available + 1] {
        let instructions_first = request()
            .with_instructions(instructions.clone())
            .unwrap()
            .with_conversation(conversation(bytes));
        let conversation_first = request()
            .with_conversation(conversation(bytes))
            .unwrap()
            .with_instructions(instructions.clone());
        if bytes == available {
            assert_eq!(instructions_first.unwrap(), conversation_first.unwrap());
        } else {
            assert_eq!(
                instructions_first.unwrap_err().category(),
                ErrorCategory::ResourceLimit
            );
            assert_eq!(
                conversation_first.unwrap_err().category(),
                ErrorCategory::ResourceLimit
            );
        }
    }
}

#[test]
fn builders_replace_payloads_instead_of_accumulating_them() {
    let instructions = ModelInstructions::new("a".repeat(MAX_INSTRUCTIONS_BYTES), "").unwrap();
    let request = request()
        .with_instructions(instructions.clone())
        .unwrap()
        .with_conversation(conversation(
            MAX_CONVERSATION_BYTES - MAX_INSTRUCTIONS_BYTES,
        ))
        .unwrap()
        .with_instructions(instructions)
        .unwrap()
        .with_conversation(vec![])
        .unwrap()
        .with_instructions(ModelInstructions::default())
        .unwrap()
        .with_conversation(conversation(MAX_CONVERSATION_BYTES))
        .unwrap();
    assert_eq!(request.instructions().payload_bytes(), 0);
}

#[test]
fn runtime_context_note_is_separate_from_history_and_charged_as_rendered() {
    let note = "Earlier context was omitted.";
    let with_note = request()
        .with_instructions(ModelInstructions::new("host", "env").unwrap())
        .unwrap()
        .with_context_note(note)
        .unwrap();
    assert!(with_note.conversation().is_empty());
    assert_eq!(with_note.context_note(), note);
    assert_eq!(
        with_note.rendered_instructions().as_deref(),
        Some("host\n\nenv\n\nRuntime context note:\nEarlier context was omitted.")
    );
    assert_eq!(
        with_note.context_note_bytes(),
        note.len() + "Runtime context note:\n".len() + 2
    );

    let max_note = "x".repeat(MAX_CONTEXT_NOTE_BYTES);
    assert!(request().with_context_note(max_note).is_ok());
    assert!(
        request()
            .with_context_note("x".repeat(MAX_CONTEXT_NOTE_BYTES + 1))
            .is_err()
    );

    let note_cost = "note".len() + "Runtime context note:\n".len();
    let exact = request()
        .with_context_note("note")
        .unwrap()
        .with_conversation(conversation(MAX_CONVERSATION_BYTES - note_cost));
    assert!(exact.is_ok());
    let over = request()
        .with_context_note("note")
        .unwrap()
        .with_conversation(conversation(MAX_CONVERSATION_BYTES - note_cost + 1));
    assert_eq!(over.unwrap_err().category(), ErrorCategory::ResourceLimit);
}
