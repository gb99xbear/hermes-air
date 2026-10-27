use hermes_gateway_core::llm::{
    ChatCompletionRequest, ChatCompletionResponse, ChatMessage, ChatResponse,
    ToolCallFunctionRaw, ToolCallRaw,
};
use hermes_gateway_core::tools::{ToolCall, ToolRegistry};
use serde_json::json;

#[tokio::test]
async fn test_tool_registry_terminal_execution() {
    let registry = ToolRegistry::new();
    let call = ToolCall {
        id: "call_term_1".to_string(),
        name: "terminal".to_string(),
        arguments: json!({
            "command": r#"echo "hermes-rust-test""#
        }),
    };

    let result = registry.execute(&call).await;
    assert_eq!(result.tool_call_id, "call_term_1");
    assert!(result.success, "Expected terminal command to succeed");
    assert!(
        result.output.contains("hermes-rust-test"),
        "Expected output to contain hermes-rust-test, got: {}",
        result.output
    );
}

#[tokio::test]
async fn test_tool_registry_terminal_failure() {
    let registry = ToolRegistry::new();
    let call = ToolCall {
        id: "call_term_fail".to_string(),
        name: "terminal".to_string(),
        arguments: json!({
            "command": "exit 42"
        }),
    };

    let result = registry.execute(&call).await;
    assert_eq!(result.tool_call_id, "call_term_fail");
    assert!(!result.success, "Expected exit 42 to fail");
}

#[tokio::test]
async fn test_tool_registry_file_write_and_read() {
    let registry = ToolRegistry::new();
    let test_dir = std::env::temp_dir().join("hermes_core_test");
    let test_file = test_dir.join("test_io.txt");
    let file_path_str = test_file.to_str().expect("Valid path");

    let write_call = ToolCall {
        id: "call_write_1".to_string(),
        name: "write_file".to_string(),
        arguments: json!({
            "path": file_path_str,
            "content": "Hello Hermes Rust Core! Integration test payload 12345."
        }),
    };

    let write_res = registry.execute(&write_call).await;
    assert_eq!(write_res.tool_call_id, "call_write_1");
    assert!(write_res.success, "File write should succeed: {}", write_res.output);
    assert!(write_res.output.contains("Successfully wrote"));

    let read_call = ToolCall {
        id: "call_read_1".to_string(),
        name: "read_file".to_string(),
        arguments: json!({
            "path": file_path_str
        }),
    };

    let read_res = registry.execute(&read_call).await;
    assert_eq!(read_res.tool_call_id, "call_read_1");
    assert!(read_res.success, "File read should succeed: {}", read_res.output);
    assert_eq!(
        read_res.output,
        "Hello Hermes Rust Core! Integration test payload 12345."
    );

    let _ = tokio::fs::remove_file(&test_file).await;
    let _ = tokio::fs::remove_dir(&test_dir).await;
}

#[tokio::test]
async fn test_tool_registry_get_definitions() {
    let registry = ToolRegistry::new();
    let defs = registry.get_definitions();

    assert_eq!(defs.len(), 3);

    let names: Vec<&str> = defs
        .iter()
        .filter_map(|d| d.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()))
        .collect();

    assert!(names.contains(&"terminal"));
    assert!(names.contains(&"read_file"));
    assert!(names.contains(&"write_file"));

    for def in &defs {
        assert_eq!(def.get("type").and_then(|t| t.as_str()), Some("function"));
        let func = def.get("function").expect("function object");
        assert!(func.get("name").is_some());
        assert!(func.get("description").is_some());
        let params = func.get("parameters").expect("parameters schema");
        assert_eq!(params.get("type").and_then(|t| t.as_str()), Some("object"));
        assert!(params.get("properties").is_some());
        assert!(params.get("required").is_some());
    }
}

#[tokio::test]
async fn test_tool_registry_unknown_tool() {
    let registry = ToolRegistry::new();
    let call = ToolCall {
        id: "call_unknown".to_string(),
        name: "non_existent_tool".to_string(),
        arguments: json!({}),
    };

    let result = registry.execute(&call).await;
    assert_eq!(result.tool_call_id, "call_unknown");
    assert!(!result.success);
    assert!(result.output.contains("Unknown tool: non_existent_tool"));
}

#[test]
fn test_chat_message_serialization_deserialization() {
    let msg_user = ChatMessage {
        role: "user".to_string(),
        content: Some("Hello, LLM!".to_string()),
        tool_calls: None,
        tool_call_id: None,
    };
    let json_user = serde_json::to_string(&msg_user).expect("serialize user message");
    assert!(json_user.contains(r#""role":"user""#));
    assert!(json_user.contains(r#""content":"Hello, LLM!""#));
    assert!(!json_user.contains("tool_calls"));
    assert!(!json_user.contains("tool_call_id"));

    let de_user: ChatMessage = serde_json::from_str(&json_user).expect("deserialize user message");
    assert_eq!(de_user.role, "user");
    assert_eq!(de_user.content, Some("Hello, LLM!".to_string()));
    assert!(de_user.tool_calls.is_none());
    assert!(de_user.tool_call_id.is_none());

    let msg_tool = ChatMessage {
        role: "tool".to_string(),
        content: Some("file created".to_string()),
        tool_calls: None,
        tool_call_id: Some("call_abc123".to_string()),
    };
    let json_tool = serde_json::to_string(&msg_tool).expect("serialize tool message");
    assert!(json_tool.contains(r#""role":"tool""#));
    assert!(json_tool.contains(r#""tool_call_id":"call_abc123""#));

    let msg_assistant = ChatMessage {
        role: "assistant".to_string(),
        content: None,
        tool_calls: Some(vec![ToolCallRaw {
            id: "call_def456".to_string(),
            call_type: "function".to_string(),
            function: ToolCallFunctionRaw {
                name: "terminal".to_string(),
                arguments: r#"{"command":"ls"}"#.to_string(),
            },
        }]),
        tool_call_id: None,
    };
    let json_assistant = serde_json::to_string(&msg_assistant).expect("serialize assistant message");
    assert!(json_assistant.contains(r#""role":"assistant""#));
    assert!(json_assistant.contains(r#""call_def456""#));
    assert!(json_assistant.contains(r#""function""#));

    let de_assistant: ChatMessage = serde_json::from_str(&json_assistant).expect("deserialize assistant message");
    assert_eq!(de_assistant.role, "assistant");
    let tc_raw = &de_assistant.tool_calls.expect("has tool calls")[0];
    let tc = tc_raw.to_tool_call();
    assert_eq!(tc.id, "call_def456");
    assert_eq!(tc.name, "terminal");
    assert_eq!(tc.arguments.get("command").and_then(|v| v.as_str()), Some("ls"));
}

#[test]
fn test_chat_completion_request_with_tools_serialization() {
    let registry = ToolRegistry::new();
    let defs = registry.get_definitions();

    let req = ChatCompletionRequest {
        model: "hermes-3-llama-3.1-8b".to_string(),
        messages: vec![ChatMessage {
            role: "user".to_string(),
            content: Some("Please list files".to_string()),
            tool_calls: None,
            tool_call_id: None,
        }],
        temperature: Some(0.5),
        max_tokens: Some(1024),
        tools: Some(defs),
        tool_choice: Some("auto".to_string()),
    };

    let req_json = serde_json::to_string(&req).expect("serialize ChatCompletionRequest");
    assert!(req_json.contains(r#""model":"hermes-3-llama-3.1-8b""#));
    assert!(req_json.contains(r#""temperature":0.5"#));
    assert!(req_json.contains(r#""max_tokens":1024"#));
    assert!(req_json.contains(r#""tool_choice":"auto""#));
    assert!(req_json.contains(r#""tools":["#));
    assert!(req_json.contains(r#""terminal""#));
}

#[test]
fn test_chat_response_parsing_text_and_tool_calls() {
    let text_response_json = json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": "Hello, I am Hermes agent!",
                "tool_calls": []
            }
        }]
    });

    let resp_text: ChatCompletionResponse =
        serde_json::from_value(text_response_json).expect("deserialize text response");
    let msg_text = resp_text.choices.into_iter().next().unwrap().message;
    assert_eq!(msg_text.content.as_deref(), Some("Hello, I am Hermes agent!"));

    let parsed_text_resp = if let Some(ref calls) = msg_text.tool_calls {
        if !calls.is_empty() {
            ChatResponse::ToolCalls(calls.iter().map(|c| c.to_tool_call()).collect())
        } else {
            ChatResponse::Text(msg_text.content.clone().unwrap_or_default())
        }
    } else {
        ChatResponse::Text(msg_text.content.clone().unwrap_or_default())
    };

    match parsed_text_resp {
        ChatResponse::Text(t) => assert_eq!(t, "Hello, I am Hermes agent!"),
        ChatResponse::ToolCalls(_) => panic!("Expected Text response"),
    }

    let tool_response_json = json!({
        "choices": [{
            "message": {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_run_999",
                    "type": "function",
                    "function": {
                        "name": "terminal",
                        "arguments": r#"{"command":"uname -a"}"#
                    }
                }]
            }
        }]
    });

    let resp_tool: ChatCompletionResponse =
        serde_json::from_value(tool_response_json).expect("deserialize tool response");
    let msg_tool = resp_tool.choices.into_iter().next().unwrap().message;

    let parsed_tool_resp = if let Some(ref calls) = msg_tool.tool_calls {
        if !calls.is_empty() {
            ChatResponse::ToolCalls(calls.iter().map(|c| c.to_tool_call()).collect())
        } else {
            ChatResponse::Text(msg_tool.content.clone().unwrap_or_default())
        }
    } else {
        ChatResponse::Text(msg_tool.content.clone().unwrap_or_default())
    };

    match parsed_tool_resp {
        ChatResponse::ToolCalls(calls) => {
            assert_eq!(calls.len(), 1);
            assert_eq!(calls[0].id, "call_run_999");
            assert_eq!(calls[0].name, "terminal");
            assert_eq!(
                calls[0].arguments.get("command").and_then(|v| v.as_str()),
                Some("uname -a")
            );
        }
        ChatResponse::Text(_) => panic!("Expected ToolCalls response"),
    }
}

#[tokio::test]
async fn test_mock_agent_loop_multi_turn() {
    let registry = ToolRegistry::new();
    let mut messages: Vec<ChatMessage> = Vec::new();

    let user_msg = ChatMessage {
        role: "user".to_string(),
        content: Some("Check system date".to_string()),
        tool_calls: None,
        tool_call_id: None,
    };
    messages.push(user_msg);

    let assistant_step1_raw = json!({
        "role": "assistant",
        "content": null,
        "tool_calls": [{
            "id": "call_date_001",
            "type": "function",
            "function": {
                "name": "terminal",
                "arguments": r#"{"command":"date +%Y"}"#
            }
        }]
    });
    let assistant_step1_msg: hermes_gateway_core::llm::ChatResponseMessage =
        serde_json::from_value(assistant_step1_raw).unwrap();

    let calls: Vec<ToolCall> = assistant_step1_msg
        .tool_calls
        .as_ref()
        .unwrap()
        .iter()
        .map(|c| c.to_tool_call())
        .collect();

    messages.push(ChatMessage {
        role: "assistant".to_string(),
        content: assistant_step1_msg.content,
        tool_calls: assistant_step1_msg.tool_calls,
        tool_call_id: None,
    });

    for call in calls {
        let result = registry.execute(&call).await;
        assert!(result.success);
        assert_eq!(result.tool_call_id, "call_date_001");
        assert_eq!(result.output.trim().len(), 4);

        messages.push(ChatMessage {
            role: "tool".to_string(),
            content: Some(result.output),
            tool_calls: None,
            tool_call_id: Some(result.tool_call_id),
        });
    }

    assert_eq!(messages.len(), 3);
    assert_eq!(messages[0].role, "user");
    assert_eq!(messages[1].role, "assistant");
    assert_eq!(messages[2].role, "tool");
    assert_eq!(messages[2].tool_call_id, Some("call_date_001".to_string()));

    let assistant_step2_raw = json!({
        "role": "assistant",
        "content": format!("The current year is {}.", messages[2].content.as_deref().unwrap().trim()),
        "tool_calls": null
    });
    let assistant_step2_msg: hermes_gateway_core::llm::ChatResponseMessage =
        serde_json::from_value(assistant_step2_raw).unwrap();

    messages.push(ChatMessage {
        role: "assistant".to_string(),
        content: assistant_step2_msg.content.clone(),
        tool_calls: None,
        tool_call_id: None,
    });

    assert_eq!(messages.len(), 4);
    assert!(assistant_step2_msg.content.unwrap().contains("The current year is "));
}
