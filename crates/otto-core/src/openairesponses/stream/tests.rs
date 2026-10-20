use super::*;
use crate::model::FinishReason;

/// Feeds `body` in slices of `chunk` bytes (`0` meaning one push) and
/// returns the assembled result together with every emitted event.
fn assemble(body: &[u8], chunk: usize) -> (Result<Response, StreamError>, Vec<StreamEvent>) {
    assemble_with_compaction(body, chunk, false)
}

fn assemble_with_compaction(
    body: &[u8],
    chunk: usize,
    request_remote_compaction: bool,
) -> (Result<Response, StreamError>, Vec<StreamEvent>) {
    let mut events = Vec::new();
    let mut sink = |event: StreamEvent| events.push(event);
    let mut assembler =
        StreamAssembler::with_remote_compaction(request_remote_compaction, "test-provider");
    let step = if chunk == 0 { body.len().max(1) } else { chunk };
    let mut result = Ok(());
    for slice in body.chunks(step) {
        result = assembler.push(slice, &mut sink);
        if result.is_err() {
            break;
        }
    }
    match result {
        Err(error) => (Err(error), events),
        Ok(()) => (assembler.finish(&mut sink), events),
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn captures_requested_compaction_without_exposing_opaque_content() {
    for item_type in ["compaction", "compaction_summary"] {
        let body = format!(
            concat!(
                "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}}\n\n",
                "data: {{\"type\":\"response.output_item.done\",\"item\":{{\"type\":\"{}\",\"id\":\"cmp_1\",\"encrypted_content\":\"opaque-secret\"}}}}\n\n",
                "data: {{\"type\":\"response.completed\",\"response\":{{\"status\":\"completed\"}}}}\n\n",
            ),
            item_type
        );
        let (response, events) = assemble_with_compaction(body.as_bytes(), 0, true);
        let response = response.expect("requested compaction assembles");
        assert_eq!(response.message.blocks, [Block::text("hi")]);
        assert_eq!(events, [StreamEvent::TextDelta { text: "hi".into() }]);
        assert_eq!(
            response.remote_compaction,
            Some(RemoteCompaction {
                provider: "test-provider".into(),
                id: "cmp_1".into(),
                encrypted_content: "opaque-secret".into(),
            })
        );
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn validates_compaction_cardinality_and_content() {
    let cases = [
        (
            "empty encrypted content",
            true,
            concat!(
                "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"compaction\",\"id\":\"c\",\"encrypted_content\":\"\"}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
            ),
            "responses remote compaction protocol error: compaction encrypted_content is empty",
        ),
        (
            "duplicate aliases",
            true,
            concat!(
                "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"compaction\",\"id\":\"c1\",\"encrypted_content\":\"one\"}}\n\n",
                "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"compaction_summary\",\"id\":\"c2\",\"encrypted_content\":\"two\"}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
            ),
            "responses remote compaction protocol error: multiple compactions were returned",
        ),
        (
            "requested but missing",
            true,
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
            "responses remote compaction protocol error: requested compaction was not returned",
        ),
        (
            "unsolicited",
            false,
            concat!(
                "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"compaction\",\"id\":\"c\",\"encrypted_content\":\"opaque-secret\"}}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
            ),
            "responses remote compaction protocol error: compaction was returned without being requested",
        ),
    ];
    for (name, requested, body, expected) in cases {
        let (result, events) = assemble_with_compaction(body.as_bytes(), 0, requested);
        let error = result.expect_err(name);
        assert_eq!(error.to_string(), expected, "{name}");
        assert!(!error.to_string().contains("opaque-secret"), "{name}");
        assert!(events.is_empty(), "{name}");
    }
}

const TEXT_AND_TOOL_CALL: &str = concat!(
    "event: response.output_text.delta\n",
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"I will read. \"}\n\n",
    "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"item-1\",\"call_id\":\"call-1\",\"name\":\"read\"}}\n\n",
    "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"item-1\",\"delta\":\"{\\\"pa\"}\n\n",
    "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"item-1\",\"delta\":\"th\\\":\\\"README.md\\\"}\"}\n\n",
    "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"item-1\",\"call_id\":\"call-1\",\"name\":\"read\",\"arguments\":\"{}\"}}\n\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"usage\":{\"input_tokens\":11,\"output_tokens\":7,\"input_tokens_details\":{\"cached_tokens\":4}}}}\n\n",
);

fn check_text_and_tool_call(response: &Response, events: &[StreamEvent]) {
    assert_eq!(response.message.role, Role::Assistant);
    assert_eq!(
        response.message.finish_reason,
        Some(FinishReason::ToolCalls)
    );
    assert_eq!(
        response.message.usage,
        Some(Usage {
            input_tokens: 11,
            output_tokens: 7,
            cached_input_tokens: 4
        })
    );
    assert_eq!(response.message.blocks.len(), 2);
    assert_eq!(response.message.blocks[0].block_type, BlockType::Text);
    assert_eq!(response.message.blocks[0].text, "I will read. ");
    let call = &response.message.blocks[1];
    assert_eq!(call.block_type, BlockType::ToolCall);
    assert_eq!(call.tool_call_id, "call-1");
    assert_eq!(call.tool_name, "read");
    assert_eq!(
        call.arguments.as_ref().map(|raw| raw.get()),
        Some(r#"{"path":"README.md"}"#)
    );
    assert_eq!(
        events,
        [
            StreamEvent::TextDelta {
                text: "I will read. ".into()
            },
            StreamEvent::ToolCallDelta {
                tool_call_id: "call-1".into(),
                tool_name: "read".into(),
                arguments: String::new()
            },
            StreamEvent::ToolCallDelta {
                tool_call_id: "call-1".into(),
                tool_name: "read".into(),
                arguments: "{\"pa".into()
            },
            StreamEvent::ToolCallDelta {
                tool_call_id: "call-1".into(),
                tool_name: "read".into(),
                arguments: "th\":\"README.md\"}".into()
            },
        ]
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn streams_text_and_assembles_tool_call() {
    let (response, events) = assemble(TEXT_AND_TOOL_CALL.as_bytes(), 0);
    check_text_and_tool_call(&response.expect("stream assembles"), &events);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn assembly_is_independent_of_chunk_boundaries() {
    for chunk in 1..=3 {
        let (response, events) = assemble(TEXT_AND_TOOL_CALL.as_bytes(), chunk);
        check_text_and_tool_call(&response.expect("stream assembles"), &events);
    }
}

/// `response.output_item.done` fills in arguments only when the delta
/// events produced none, so a backend that sends both spellings does not
/// double the text.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn done_item_supplies_arguments_only_when_no_delta_arrived() {
    let body = concat!(
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"i1\",\"call_id\":\"c1\",\"name\":\"read\"}}\n\n",
        "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"id\":\"i1\",\"call_id\":\"c1\",\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"A\\\"}\"}}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
    );
    let (response, _) = assemble(body.as_bytes(), 0);
    let response = response.expect("stream assembles");
    assert_eq!(
        response.message.blocks[0]
            .arguments
            .as_ref()
            .map(|r| r.get()),
        Some(r#"{"path":"A"}"#)
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn keeps_tool_call_identity_across_interleaved_items() {
    let body = concat!(
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"i1\",\"call_id\":\"c1\",\"name\":\"read\"}}\n\n",
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"i2\",\"call_id\":\"c2\",\"name\":\"write\"}}\n\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"i2\",\"delta\":\"{\\\"path\\\":\\\"B\\\"}\"}\n\n",
        "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"i1\",\"delta\":\"{\\\"path\\\":\\\"A\\\"}\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
    );
    let (response, events) = assemble(body.as_bytes(), 0);
    let response = response.expect("stream assembles");
    assert_eq!(response.message.blocks.len(), 2);
    assert_eq!(response.message.blocks[0].tool_call_id, "c1");
    assert_eq!(
        response.message.blocks[0]
            .arguments
            .as_ref()
            .map(|r| r.get()),
        Some(r#"{"path":"A"}"#)
    );
    assert_eq!(response.message.blocks[1].tool_call_id, "c2");
    assert_eq!(
        response.message.blocks[1]
            .arguments
            .as_ref()
            .map(|r| r.get()),
        Some(r#"{"path":"B"}"#)
    );
    assert_eq!(events.len(), 4);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn reasoning_summary_deltas_stream_and_become_one_reasoning_block() {
    let body = concat!(
        "data: {\"type\":\"response.reasoning_summary_text.delta\",\"item_id\":\"r1\",\"summary_index\":0,\"delta\":\"Read \"}\n\n",
        "data: {\"type\":\"response.reasoning_summary_text.delta\",\"item_id\":\"r1\",\"summary_index\":0,\"delta\":\"the file\"}\n\n",
        "data: {\"type\":\"response.reasoning_summary_text.delta\",\"item_id\":\"r1\",\"summary_index\":1,\"delta\":\"Then answer\"}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
    );
    for chunk in [0, 1, 7] {
        let (result, events) = assemble(body.as_bytes(), chunk);
        let response = result.expect("assembled");
        let reasoning = |text: &str| StreamEvent::ReasoningDelta { text: text.into() };
        assert_eq!(
            events,
            vec![
                reasoning("Read "),
                reasoning("the file"),
                reasoning("\n\n"),
                reasoning("Then answer"),
                StreamEvent::TextDelta { text: "hi".into() },
            ]
        );
        assert_eq!(
            response.message.blocks,
            vec![
                Block::reasoning("Read the file\n\nThen answer"),
                Block::text("hi")
            ]
        );
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn a_reasoning_delta_marks_the_stream_emitted() {
    let mut assembler = StreamAssembler::new();
    let mut sink = |_: StreamEvent| {};
    assembler
            .push(
                b"data: {\"type\":\"response.reasoning_summary_text.delta\",\"item_id\":\"r1\",\"summary_index\":0,\"delta\":\"x\"}\n\n",
                &mut sink,
            )
            .expect("push");
    assert!(assembler.emitted());
}

/// A non-function output item, a reasoning summary for example, is ignored
/// and produces no tool call.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn ignores_output_items_that_are_not_function_calls() {
    let body = concat!(
        "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"message\",\"id\":\"i1\"}}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
    );
    let (response, events) = assemble(body.as_bytes(), 0);
    let response = response.expect("stream assembles");
    assert_eq!(response.message.blocks.len(), 1);
    assert_eq!(response.message.blocks[0].text, "hi");
    assert_eq!(response.message.finish_reason, Some(FinishReason::Stop));
    assert_eq!(events, [StreamEvent::TextDelta { text: "hi".into() }]);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn incomplete_response_with_max_output_tokens_finishes_as_length() {
    let body = concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n",
        "data: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"incomplete\",\"incomplete_details\":{\"reason\":\"max_output_tokens\"}}}\n\n",
    );
    let (response, _) = assemble(body.as_bytes(), 0);
    assert_eq!(
        response.expect("stream assembles").message.finish_reason,
        Some(FinishReason::Length)
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn reported_errors_carry_the_server_detail() {
    let cases: &[(&str, &str, &str)] = &[
        (
            "error code and message",
            "data: {\"type\":\"error\",\"code\":\"server_error\",\"message\":\"try later\",\"param\":null}\n\n",
            "responses stream reported error: server_error: try later",
        ),
        (
            "error with a nested error object",
            "data: {\"type\":\"error\",\"status\":429,\"error\":{\"type\":\"usage_limit_reached\",\"code\":\"rate_limit_exceeded\",\"message\":\"The usage limit has been reached\"}}\n\n",
            "responses stream reported error: rate_limit_exceeded: The usage limit has been reached",
        ),
        (
            "nested type is retained when code is empty",
            "data: {\"type\":\"error\",\"error\":{\"type\":\"server_error\",\"code\":\"\",\"message\":\"try later\"}}\n\n",
            "responses stream reported error: server_error: try later",
        ),
        (
            "response error type precedes nested error code",
            "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"\",\"type\":\"server_error\",\"message\":\"response message\"}},\"error\":{\"code\":\"nested_code\",\"message\":\"nested message\"},\"code\":\"top_code\",\"message\":\"top message\"}\n\n",
            "responses stream reported response.failed: server_error: response message",
        ),
        (
            "empty nested classification falls through to top level",
            "data: {\"type\":\"error\",\"error\":{\"code\":\"\",\"type\":\"\",\"message\":\"nested message\"},\"code\":\"top_code\",\"message\":\"top message\"}\n\n",
            "responses stream reported error: top_code: top message",
        ),
        (
            "selected classification uses a fallback message",
            "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"response_code\",\"message\":\"\"}},\"error\":{\"message\":\"nested message\"},\"message\":\"top message\"}\n\n",
            "responses stream reported response.failed: response_code: nested message",
        ),
        (
            "error message only",
            "data: {\"type\":\"error\",\"code\":null,\"message\":\"boom\"}\n\n",
            "responses stream reported error: boom",
        ),
        (
            "failed response error",
            "data: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":{\"code\":\"rate_limit\",\"message\":\"slow down\"}}}\n\n",
            "responses stream reported response.failed: rate_limit: slow down",
        ),
        (
            "failed response with null error",
            "data: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\",\"error\":null}}\n\n",
            "responses stream reported response.failed",
        ),
    ];
    for (name, body, want) in cases {
        let (result, _) = assemble(body.as_bytes(), 0);
        let error = result.expect_err(name).to_string();
        assert_eq!(error, *want, "{name}");
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn rejects_malformed_failed_and_truncated_streams() {
    let cases: &[(&str, &str, &str)] = &[
        (
            "malformed JSON",
            "data: {not-json}\n\n",
            "decode responses stream",
        ),
        (
            "missing completion",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\n",
            "responses stream ended without response.completed",
        ),
        (
            "reported failure",
            "data: {\"type\":\"response.failed\",\"response\":{\"status\":\"failed\"}}\n\n",
            "responses stream reported response.failed",
        ),
        (
            "reported error",
            "data: {\"type\":\"error\",\"message\":\"boom\"}\n\n",
            "responses stream reported error",
        ),
        (
            "malformed tool arguments",
            concat!(
                "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"id\":\"i1\",\"call_id\":\"bad\",\"name\":\"read\"}}\n\n",
                "data: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"i1\",\"delta\":\"{\"}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
            ),
            "tool call \"bad\" has malformed arguments",
        ),
    ];
    for (name, body, want) in cases {
        let (result, _) = assemble(body.as_bytes(), 0);
        let error = result.expect_err(name).to_string();
        assert!(error.contains(want), "{name}: error = {error:?}");
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn accepts_sse_framing_variants() {
    let body = concat!(
        ": keepalive\r\n\r\n",
        "data\r\n",
        "data:{\"type\":\"response.output_text.delta\",\r\n",
        "data:\"delta\":\"hi\"}\r\n\r\n",
        "event: response.completed\r\n",
        "data:{\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}",
    );
    let (response, events) = assemble(body.as_bytes(), 0);
    let response = response.expect("stream assembles");
    assert_eq!(response.message.blocks[0].text, "hi");
    assert_eq!(events, [StreamEvent::TextDelta { text: "hi".into() }]);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn event_field_supplies_only_an_empty_json_type() {
    let cases: &[(&str, &str, &str)] = &[
        (
            "missing JSON type",
            "event: error\ndata: {\"code\":\"server_error\",\"message\":\"boom\"}\n\n",
            "responses stream reported error: server_error: boom",
        ),
        (
            "empty JSON type",
            "event: error\ndata: {\"type\":\"\",\"message\":\"boom\"}\n\n",
            "responses stream reported error: boom",
        ),
        (
            "JSON type wins conflicts",
            "event: response.completed\ndata: {\"type\":\"error\",\"message\":\"boom\"}\n\n",
            "responses stream reported error: boom",
        ),
    ];
    for (name, body, want) in cases {
        let (result, _) = assemble(body.as_bytes(), 0);
        assert_eq!(result.expect_err(name).to_string(), *want, "{name}");
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn rejects_oversized_lines_before_accumulating_them() {
    let mut sink = |_: StreamEvent| {};
    for ending in [b"\n".as_slice(), b"\r\n".as_slice()] {
        let mut assembler = StreamAssembler::new();
        let mut bytes = vec![b'x'; MAX_SSE_LINE_BYTES];
        bytes.extend_from_slice(ending);
        assert_eq!(
            assembler.push(&bytes, &mut sink),
            Ok(()),
            "the line ending is excluded from the limit"
        );
        assert!(assembler.line.is_empty());
    }

    for suffix in [b"x".as_slice(), b"\rx".as_slice()] {
        let mut assembler = StreamAssembler::new();
        let mut bytes = vec![b'x'; MAX_SSE_LINE_BYTES];
        bytes.extend_from_slice(suffix);
        assert_eq!(
            assembler.push(&bytes, &mut sink),
            Err(StreamError::LineTooLarge {
                limit: MAX_SSE_LINE_BYTES
            })
        );
        assert!(assembler.line.len() <= MAX_SSE_LINE_BYTES + 1);
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn rejects_oversized_events_before_accumulating_the_last_data_line() {
    let mut assembler = StreamAssembler::new();
    let mut sink = |_: StreamEvent| {};
    let line = vec![b'x'; 64 * 1024];
    let mut accepted = 0usize;
    loop {
        let mut field = b"data:".to_vec();
        field.extend_from_slice(&line);
        field.push(b'\n');
        match assembler.push(&field, &mut sink) {
            Ok(()) => accepted += 1,
            Err(error) => {
                assert_eq!(
                    error,
                    StreamError::EventTooLarge {
                        limit: MAX_SSE_EVENT_BYTES
                    }
                );
                break;
            }
        }
    }
    assert_eq!(assembler.data_lines.len(), accepted);
    assert!(assembler.data_bytes <= MAX_SSE_EVENT_BYTES);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn accepts_completed_event_larger_than_64_kib_on_one_line() {
    let instructions = "x".repeat(200 * 1024);
    let body = format!(
        "data: {{\"type\":\"response.completed\",\"response\":{{\"status\":\"completed\",\"instructions\":\"{instructions}\"}}}}\n\n"
    );
    assert!(body.len() > 64 * 1024);
    for chunk in [0, 4096] {
        let (result, _) = assemble(body.as_bytes(), chunk);
        assert!(result.is_ok(), "{result:?}");
    }
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn data_line_limit_matches_event_limit() {
    let mut sink = |_: StreamEvent| {};
    let mut assembler = StreamAssembler::new();
    let mut at_limit = b"data: ".to_vec();
    at_limit.extend(std::iter::repeat_n(b'x', MAX_SSE_EVENT_BYTES));
    assert_eq!(assembler.push(&at_limit, &mut sink), Ok(()));
    assert_eq!(assembler.push(b"\n", &mut sink), Ok(()));
    assert_eq!(assembler.data_bytes, MAX_SSE_EVENT_BYTES);

    let mut assembler = StreamAssembler::new();
    // Without the optional space, the line stays within the line cap.
    let mut over = b"data:".to_vec();
    over.extend(std::iter::repeat_n(b'x', MAX_SSE_EVENT_BYTES + 1));
    assert_eq!(assembler.push(&over, &mut sink), Ok(()));
    assert_eq!(
        assembler.push(b"\n", &mut sink),
        Err(StreamError::EventTooLarge {
            limit: MAX_SSE_EVENT_BYTES
        })
    );
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn usage_absence_is_preserved() {
    let body =
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n";
    let (response, _) = assemble(body.as_bytes(), 0);
    assert_eq!(response.expect("stream assembles").message.usage, None);
}

#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn ignores_bytes_after_the_terminal_event_and_reports_emission() {
    let body = concat!(
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"a\"}\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\"}}\n\n",
        "data: {not-json}\n\n",
    );
    let mut events = Vec::new();
    let mut sink = |event: StreamEvent| events.push(event);
    let mut assembler = StreamAssembler::new();
    assembler.push(body.as_bytes(), &mut sink).expect("push");
    assert!(assembler.emitted());
    assert!(assembler.is_done());
    let response = assembler.finish(&mut sink).expect("stream assembles");
    assert_eq!(response.message.blocks.len(), 1);
    assert_eq!(events.len(), 1);
}

/// The provider hands its own sink straight to the assembler, so
/// [`crate::provider::StreamSink`] has to coerce to the callback type.
#[cfg_attr(target_arch = "wasm32", wasm_bindgen_test::wasm_bindgen_test)]
#[cfg_attr(not(target_arch = "wasm32"), test)]
fn accepts_the_provider_stream_sink() {
    let mut count = 0usize;
    let mut sink = |_event: StreamEvent| count += 1;
    let sink: crate::provider::StreamSink<'_> = &mut sink;
    let mut assembler = StreamAssembler::new();
    assembler
        .push(
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n\n".as_bytes(),
            sink,
        )
        .expect("push");
    assert_eq!(count, 1);
}
