use hermes_gateway_core::llm::{
    ChatCompletionRequest, ChatCompletionResponse, ChatMessage, ChatResponse,
    ToolCallFunctionRaw, ToolCallRaw, ToolCallRawExt,
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
        stream: false,
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


#[tokio::test]
async fn test_prompt_assembler_empty_fallback() {
    use hermes_gateway_core::PromptAssembler;

    let empty_dir = std::env::temp_dir().join("hermes_test_prompt_empty");
    let _ = tokio::fs::create_dir_all(&empty_dir).await;

    let assembler = PromptAssembler::with_overrides(Some(empty_dir.clone()), Some(empty_dir.clone()));
    let prompt = assembler.build_system_prompt(None).await;
    assert!(prompt.is_empty(), "Prompt should be empty when no files and no base prompt");

    let prompt_with_base = assembler.build_system_prompt(Some("Base system instruction")).await;
    assert_eq!(prompt_with_base, "# PROFILE INSTRUCTIONS\n\nBase system instruction");

    let _ = tokio::fs::remove_dir_all(&empty_dir).await;
}

#[tokio::test]
async fn test_prompt_assembler_combines_all_sections() {
    use hermes_gateway_core::PromptAssembler;

    let test_dir = std::env::temp_dir().join("hermes_test_prompt_full");
    let hermes_dir = test_dir.join("hermes_home");
    let memories_dir = hermes_dir.join("memories");
    let cwd_dir = test_dir.join("cwd");

    let _ = tokio::fs::create_dir_all(&memories_dir).await;
    let _ = tokio::fs::create_dir_all(&cwd_dir).await;

    tokio::fs::write(hermes_dir.join("SOUL.md"), "Default persona and soul definition.").await.unwrap();
    tokio::fs::write(cwd_dir.join("AGENTS.md"), "Zero-refusal law and operational doctrine.").await.unwrap();
    tokio::fs::write(memories_dir.join("MEMORY.md"), "Estate layout and infrastructure credentials.").await.unwrap();
    tokio::fs::write(memories_dir.join("USER.md"), "User profile and preferences.").await.unwrap();
    tokio::fs::write(cwd_dir.join("system.md"), "System level guidelines.").await.unwrap();

    let assembler = PromptAssembler::with_overrides(Some(hermes_dir), Some(cwd_dir));
    let assembled = assembler.build_system_prompt(Some("Profile base prompt")).await;

    assert!(assembled.contains("# SOUL\n\nDefault persona and soul definition."));
    assert!(assembled.contains("# OPERATIONAL DOCTRINE (AGENTS.md)\n\nZero-refusal law and operational doctrine."));
    assert!(assembled.contains("# MEMORY & ESTATE RULES\n\nEstate layout and infrastructure credentials."));
    assert!(assembled.contains("# USER PROFILE\n\nUser profile and preferences."));
    assert!(assembled.contains("# SYSTEM INSTRUCTIONS\n\nSystem level guidelines."));
    assert!(assembled.contains("# PROFILE INSTRUCTIONS\n\nProfile base prompt"));

    // Check delimiters
    assert!(assembled.contains("\n\n---\n\n"));

    let _ = tokio::fs::remove_dir_all(&test_dir).await;
}


#[tokio::test]
async fn test_slash_command_parsing_suite() {
    use hermes_gateway_core::{parse_command, SlashCommand};

    assert_eq!(parse_command("/status"), Some(SlashCommand::Status));
    assert_eq!(parse_command("  /status  "), Some(SlashCommand::Status));
    assert_eq!(parse_command("/status@HermesGatewayBot"), Some(SlashCommand::Status));

    assert_eq!(parse_command("/model"), Some(SlashCommand::Model(None)));
    assert_eq!(
        parse_command("/model custom/model"),
        Some(SlashCommand::Model(Some("custom/model".to_string())))
    );
    assert_eq!(
        parse_command("/model   ag/gemini-3.8-flash  "),
        Some(SlashCommand::Model(Some("ag/gemini-3.8-flash".to_string())))
    );
    assert_eq!(
        parse_command("/model@HermesBot freet/claude-5-sonnet"),
        Some(SlashCommand::Model(Some("freet/claude-5-sonnet".to_string())))
    );

    assert_eq!(parse_command("/clear"), Some(SlashCommand::Clear));
    assert_eq!(parse_command("/reset"), Some(SlashCommand::Clear));
    assert_eq!(parse_command("/new"), Some(SlashCommand::Clear));

    assert_eq!(parse_command("/help"), Some(SlashCommand::Help));
    assert_eq!(parse_command("/start"), Some(SlashCommand::Start));
    assert_eq!(parse_command("/commands"), Some(SlashCommand::Help));

    assert_eq!(parse_command("hello world"), None);
    assert_eq!(parse_command("not /a/command"), None);
}

#[tokio::test]
async fn test_slash_command_output_handling() {
    use hermes_gateway_core::{handle_command, CommandContext, SlashCommand};

    let ctx = CommandContext {
        gateway_name: "hermes-gateway-test",
        gateway_version: "0.2.0",
        active_profile: "default",
        active_model: "test-model-default",
        telegram_connected: true,
        memory_stats: Some("12.3 MB RSS".to_string()),
    };

    // Test /start and /help output
    let start_out = handle_command(SlashCommand::Start, &ctx, |_| Ok(()));
    assert!(start_out.contains("Greetings!"));
    assert!(start_out.contains("test-model-default"));
    assert!(start_out.contains("Available Slash Commands:"));

    let help_out = handle_command(SlashCommand::Help, &ctx, |_| Ok(()));
    assert!(help_out.contains("Greetings!"));
    assert!(help_out.contains("/status"));
    assert!(help_out.contains("/model"));
    assert!(help_out.contains("/clear"));

    // Test /status output
    let status_out = handle_command(SlashCommand::Status, &ctx, |_| Ok(()));
    assert!(status_out.contains("Gateway Status"));
    assert!(status_out.contains("hermes-gateway-test v0.2.0"));
    assert!(status_out.contains("default"));
    assert!(status_out.contains("test-model-default"));
    assert!(status_out.contains("Connected"));
    assert!(status_out.contains("12.3 MB RSS"));

    // Test /model query output
    let model_query_out = handle_command(SlashCommand::Model(None), &ctx, |_| Ok(()));
    assert!(model_query_out.contains("Active Model Configuration"));
    assert!(model_query_out.contains("Current Model:</b> <code>test-model-default</code>"));
    assert!(model_query_out.contains("/model &lt;model_name&gt;"));

    // Test /model switch output
    let mut recorded_switch = String::new();
    let model_switch_out = handle_command(
        SlashCommand::Model(Some("freet/claude-5-sonnet".to_string())),
        &ctx,
        |target| {
            recorded_switch = target.to_string();
            Ok(())
        },
    );
    assert_eq!(recorded_switch, "freet/claude-5-sonnet");
    assert!(model_switch_out.contains("Model Switched Successfully"));
    assert!(model_switch_out.contains("freet/claude-5-sonnet"));

    // Test /clear output
    let clear_out = handle_command(SlashCommand::Clear, &ctx, |_| Ok(()));
    assert!(clear_out.contains("Session Reset"));
    assert!(clear_out.contains("Conversation turn memory has been cleared."));
}

#[tokio::test]
async fn test_multi_turn_session_conversation_memory() {
    use hermes_session_store::{MemorySessionStore, SessionStore};
    use hermes_protocol::{ChatMessage, Uuid};

    let store = MemorySessionStore::new();
    let session_id = Uuid::new_v4();

    // Turn 1: User says my favorite fruit is Durian
    let user_msg_1 = ChatMessage::user("My favorite fruit is Durian");
    store.append_message(session_id, user_msg_1).await.unwrap();

    // Assistant acknowledges in Turn 1
    let assistant_msg_1 = ChatMessage::assistant("Got it! I will remember that your favorite fruit is Durian.");
    store.append_message(session_id, assistant_msg_1).await.unwrap();

    // Verify session store has Turn 1
    let history_turn_1 = store.get_messages(session_id, Some(30)).await.unwrap();
    assert_eq!(history_turn_1.len(), 2);
    assert_eq!(history_turn_1[0].role, "user");
    assert_eq!(history_turn_1[0].content.as_deref(), Some("My favorite fruit is Durian"));
    assert_eq!(history_turn_1[1].role, "assistant");
    assert_eq!(history_turn_1[1].content.as_deref(), Some("Got it! I will remember that your favorite fruit is Durian."));

    // Turn 2: User asks what fruit do I like?
    let user_msg_2 = ChatMessage::user("What fruit do I like?");
    // When Turn 2 is processed by Gateway, it loads history_turn_1
    let loaded_history = store.get_messages(session_id, Some(30)).await.unwrap();
    
    // Construct simulated LLM payload with system prompt + history + new user message
    let mut turn_2_messages = Vec::new();
    turn_2_messages.push(ChatMessage::system("You are Hermes assistant."));
    for h in loaded_history {
        if h.role != "system" {
            turn_2_messages.push(h);
        }
    }
    turn_2_messages.push(user_msg_2.clone());
    store.append_message(session_id, user_msg_2).await.unwrap();

    // Assistant answers Turn 2 recalling Durian from context
    let assistant_msg_2 = ChatMessage::assistant("You told me earlier that your favorite fruit is Durian!");
    store.append_message(session_id, assistant_msg_2).await.unwrap();

    // Verify full chronological transcript has 4 messages
    let full_history = store.get_messages(session_id, Some(30)).await.unwrap();
    assert_eq!(full_history.len(), 4);
    assert_eq!(full_history[0].role, "user");
    assert_eq!(full_history[1].role, "assistant");
    assert_eq!(full_history[2].role, "user");
    assert_eq!(full_history[3].role, "assistant");
    assert_eq!(full_history[3].content.as_deref(), Some("You told me earlier that your favorite fruit is Durian!"));

    // Turn 3: User resets conversation via /clear
    store.clear_messages(session_id).await.unwrap();
    let history_after_clear = store.get_messages(session_id, Some(30)).await.unwrap();
    assert!(history_after_clear.is_empty(), "Conversation history must be completely evicted on /clear");
}
