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
    assert!(write_res.output.contains("bytes_written"));

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
    let read_json: serde_json::Value = serde_json::from_str(&read_res.output).expect("valid json from read_file");
    assert_eq!(read_json.get("total_lines").and_then(|v| v.as_u64()), Some(1));
    assert!(read_json.get("content").and_then(|v| v.as_str()).unwrap().contains("1|Hello Hermes Rust Core!"));

    let _ = tokio::fs::remove_file(&test_file).await;
    let _ = tokio::fs::remove_dir(&test_dir).await;
}

#[tokio::test]
async fn test_tool_registry_get_definitions() {
    let registry = ToolRegistry::new();
    let defs = registry.get_definitions();

    assert_eq!(defs.len(), 23);

    let names: Vec<&str> = defs
        .iter()
        .filter_map(|d| d.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()))
        .collect();

    assert!(names.contains(&"terminal"));
    assert!(names.contains(&"read_file"));
    assert!(names.contains(&"write_file"));
    assert!(names.contains(&"patch"));
    assert!(names.contains(&"search_files"));
    assert!(names.contains(&"web_search"));
    assert!(names.contains(&"web_extract"));
    assert!(names.contains(&"skills_list"));
    assert!(names.contains(&"skill_view"));
    assert!(names.contains(&"todo_list"));
    assert!(names.contains(&"session_search"));
    assert!(names.contains(&"memory"));

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
        session_id: Some(uuid::Uuid::nil()),
        platform: Some("telegram"),
        user_id: Some("12345"),
        chat_id: Some("67890"),
        chat_type: Some("dm"),
        thread_id: None,
        transcript_count: 8,
        estimated_prompt_tokens: 16000,
        context_window: 128000,
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


#[tokio::test]
async fn test_provider_profile_resolution_in_gateway() {
    use hermes_config::{
        GatewayConfig, GatewaySection, ProfileConfig, PluginsConfig,
        SessionStoreConfig, SessionStoreBackend, LoggingConfig, LogFormat, LogOutput,
        MetricsConfig, DashboardConfig, TuiConfig, AcpConfig,
    };
    use hermes_gateway_core::Gateway;
    use std::collections::HashMap;
    use std::path::PathBuf;

    let mut profiles = HashMap::new();
    profiles.insert(
        "default".to_string(),
        ProfileConfig {
            name: "default".to_string(),
            model_provider: "openrouter".to_string(),
            model: "anthropic/claude-sonnet-4.6".to_string(),
            temperature: 0.7,
            max_tokens: 4096,
            system_prompt: Some("Test prompt".to_string()),
            tools: Vec::new(),
            skills: Vec::new(),
            mcp_servers: Vec::new(),
            platform_overrides: HashMap::new(),
        },
    );

    let config = GatewayConfig {
        gateway: GatewaySection {
            name: "test-gateway".to_string(),
            version: "0.1.0".to_string(),
            max_concurrent_sessions: 10,
            default_profile: "default".to_string(),
            shutdown_timeout_secs: 5,
            restart_on_config_change: false,
        },
        profiles,
        platforms: HashMap::new(),
        plugins: PluginsConfig {
            enabled: false,
            directory: PathBuf::from("/tmp/plugins"),
            wasm_directory: None,
            python_plugins: false,
            allowed_plugins: Vec::new(),
            blocked_plugins: Vec::new(),
        },
        session_store: SessionStoreConfig {
            backend: SessionStoreBackend::Memory,
            redis: None,
            local_cache_size: 100,
            default_ttl_secs: 3600,
        },
        logging: LoggingConfig {
            level: "info".to_string(),
            format: LogFormat::Text,
            output: LogOutput::Stdout,
            file: None,
            max_file_size_mb: 10,
            max_files: 5,
        },
        metrics: MetricsConfig {
            enabled: false,
            port: 9090,
            path: "/metrics".to_string(),
        },
        dashboard: DashboardConfig {
            enabled: false,
            host: "127.0.0.1".to_string(),
            port: 8080,
            auth: None,
        },
        tui: TuiConfig {
            enabled: false,
            socket_path: None,
            port: None,
        },
        acp: AcpConfig {
            enabled: false,
            port: 9000,
        },
    };

    let gateway = Gateway::new(config).await.expect("Gateway should initialize");
    let provider_reg = gateway.provider_registry();
    let openrouter = provider_reg.get("openrouter").await.expect("openrouter profile should be present");
    assert_eq!(openrouter.name, "openrouter");
    assert_eq!(openrouter.base_url, "https://openrouter.ai/api/v1");

    let llm_client = gateway.llm_client();
    // Default base_url of LlmClient should match OpenRouter base_url
    assert_eq!(llm_client.base_url, "https://openrouter.ai/api/v1");
}

#[tokio::test]
async fn test_streaming_sse_parser_and_consumer_mock() {
    use hermes_gateway_core::stream::GatewayStreamConsumer;
    let raw_sse = "data: {\"choices\":[{\"delta\":{\"content\":\"Hello \"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"streaming \"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"world\"}}]}\n\ndata: [DONE]\n\n";
    let (tx, mut rx) = tokio::sync::mpsc::channel(10);

    tokio::spawn(async move {
        for line in raw_sse.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() { continue; }
            if let Some(rest) = trimmed.strip_prefix("data: ") {
                let rest = rest.trim();
                if rest == "[DONE]" { break; }
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(rest) {
                    if let Some(content) = val.get("choices")
                        .and_then(|c| c.get(0))
                        .and_then(|c| c.get("delta"))
                        .and_then(|d| d.get("content"))
                        .and_then(|s| s.as_str()) {
                            tx.send(content.to_string()).await.unwrap();
                    }
                }
            }
        }
    });

    let mut consumer = GatewayStreamConsumer::new(999);
    while let Some(chunk) = rx.recv().await {
        consumer.accumulated.push_str(&chunk);
    }
    assert_eq!(consumer.accumulated, "Hello streaming world");
    assert_eq!(consumer.chat_id, 999);
}

#[tokio::test]
async fn test_upstream_prompt_feedback_filter_extraction() {
    let raw_filter_json = json!({
        "response": {
            "promptFeedback": {
                "blockReason": "SAFETY",
                "blockReasonMessage": "Prompt violates upstream content policy."
            }
        }
    });

    let resp: ChatCompletionResponse = serde_json::from_value(raw_filter_json).expect("deserialization");
    assert!(resp.choices.is_empty());
    assert!(resp.response.is_some());
    let pf = resp.response.unwrap().prompt_feedback.unwrap();
    assert_eq!(pf.block_reason.as_deref(), Some("SAFETY"));
    assert_eq!(pf.block_reason_message.as_deref(), Some("Prompt violates upstream content policy."));

    let reason = pf.block_reason_message
        .as_deref()
        .or(pf.block_reason.as_deref())
        .unwrap_or("content policy violation");
    let refusal_text = format!("⚠️ This request was blocked by upstream safety filters: {}", reason);
    assert_eq!(
        refusal_text,
        "⚠️ This request was blocked by upstream safety filters: Prompt violates upstream content policy."
    );
    assert!(!refusal_text.contains("Echo:"));
}

#[tokio::test]
async fn test_error_recovery_zero_fake_echo() {
    let simulated_error = anyhow::anyhow!("Connection timeout to upstream provider");
    let error_reply = format!("⚠️ Upstream LLM error: {}. Sila guna arahan /model untuk tukar model.", simulated_error);

    assert!(error_reply.starts_with("⚠️ Upstream LLM error:"));
    assert!(error_reply.contains("Sila guna arahan /model untuk tukar model."));
    assert!(!error_reply.contains("Echo:"));
}

#[tokio::test]
async fn test_multi_session_model_override_independence() {
    use hermes_session_store::{MemorySessionStore, SessionStore};
    use hermes_protocol::Uuid;

    let store = MemorySessionStore::new();
    let session_a = Uuid::new_v4();
    let session_b = Uuid::new_v4();
    let default_model = "gemini-2.5-flash";

    // 1. Initial resolution without overrides falls back to default_model
    let model_a_initial = store.get_model_override(session_a).await.unwrap()
        .unwrap_or_else(|| default_model.to_string());
    let model_b_initial = store.get_model_override(session_b).await.unwrap()
        .unwrap_or_else(|| default_model.to_string());
    assert_eq!(model_a_initial, "gemini-2.5-flash");
    assert_eq!(model_b_initial, "gemini-2.5-flash");

    // 2. Set model override for Session A to Claude 3.7 Sonnet
    store.set_model_override(session_a, Some("freet/claude-3-7-sonnet".to_string())).await.unwrap();

    // Verify Session A uses Claude, while Session B independently remains default (no leakage!)
    let model_a_after = store.get_model_override(session_a).await.unwrap()
        .unwrap_or_else(|| default_model.to_string());
    let model_b_after = store.get_model_override(session_b).await.unwrap()
        .unwrap_or_else(|| default_model.to_string());
    assert_eq!(model_a_after, "freet/claude-3-7-sonnet");
    assert_eq!(model_b_after, "gemini-2.5-flash");

    // 3. Set model override for Session B to GPT-4o
    store.set_model_override(session_b, Some("openai/gpt-4o".to_string())).await.unwrap();

    // Verify both sessions have distinct, isolated models
    let model_a_final = store.get_model_override(session_a).await.unwrap()
        .unwrap_or_else(|| default_model.to_string());
    let model_b_final = store.get_model_override(session_b).await.unwrap()
        .unwrap_or_else(|| default_model.to_string());
    assert_eq!(model_a_final, "freet/claude-3-7-sonnet");
    assert_eq!(model_b_final, "openai/gpt-4o");

    // 4. Reset Session A (/clear or /reset)
    store.set_model_override(session_a, None).await.unwrap();
    let model_a_reset = store.get_model_override(session_a).await.unwrap()
        .unwrap_or_else(|| default_model.to_string());
    let model_b_retained = store.get_model_override(session_b).await.unwrap()
        .unwrap_or_else(|| default_model.to_string());
    assert_eq!(model_a_reset, "gemini-2.5-flash");
    assert_eq!(model_b_retained, "openai/gpt-4o");
}

#[tokio::test]
async fn test_interactive_model_picker_generator_and_callback_routing() {
    use hermes_gateway_core::{
        build_provider_keyboard, build_model_keyboard, model_picker_card, provider_picker_header,
        default_providers_list, ModelPickerState,
    };
    use hermes_session_store::SessionStoreFactory;
    use hermes_config::SessionStoreBackend;
    use std::collections::HashMap;
    use std::sync::Arc;
    use tokio::sync::RwLock;
    use uuid::Uuid;

    // 1. Verify Provider Picker Generator
    let providers = default_providers_list();
    let prov_kb = build_provider_keyboard(&providers, "9router");
    assert_eq!(prov_kb.inline_keyboard[0][0].text, "⭐ 9Router Gateway");
    assert_eq!(prov_kb.inline_keyboard[0][0].callback_data.as_deref(), Some("mp:9router"));

    let header = provider_picker_header("ag/gemini-3.8-flash", "9Router Gateway");
    assert!(header.contains("HERMES ROUTER // MODEL SELECTOR"));
    assert!(header.contains("ag/gemini-3.8-flash"));
    assert!(header.contains("Select a provider:"));

    // 2. Verify Model Picker Card and Keyboard
    let sample_models = vec![
        "ag/gemini-3.8-flash".to_string(),
        "thk/deepseek-v4.1-flash:free".to_string(),
        "gcli/grok-4.7-medium".to_string(),
    ];
    let (model_kb, page, total) = build_model_keyboard(&sample_models, 0, "ag/gemini-3.8-flash", "9router");
    assert_eq!(page, 1);
    assert_eq!(total, 1);
    assert_eq!(model_kb.inline_keyboard[0][0].text, "⭐ gemini-3.8-flash");
    assert_eq!(model_kb.inline_keyboard[0][0].callback_data.as_deref(), Some("mm:ag/gemini-3.8-flash"));

    let card = model_picker_card("9Router Gateway", 3, page, total, &sample_models, "ag/gemini-3.8-flash");
    assert!(card.contains("PROVIDER : 9Router Gateway"));
    assert!(card.contains("⭐ ag/gemini-3.8-flash"));
    assert!(card.contains("Select a target model below:"));

    // 3. Test Session Model Override persistence via mm:<model_id> callback simulation
    let store = Arc::new(SessionStoreFactory::create(SessionStoreBackend::Memory, None, 100, 3600).await.unwrap());
    let session_id = Uuid::new_v4();
    let chat_id = 999888777i64;

    let picker_states = Arc::new(RwLock::new(HashMap::new()));
    picker_states.write().await.insert(
        chat_id,
        ModelPickerState {
            message_id: 1234,
            chat_id,
            session_id,
            selected_provider: "tokenharbor".to_string(),
            selected_provider_name: "TokenHarbor".to_string(),
            current_model: "gemini-2.5-flash".to_string(),
            current_provider: "9router".to_string(),
            model_list: sample_models.clone(),
            model_page: 0,
        },
    );

    // Simulate model switch callback
    let chosen_model = "thk/deepseek-v4.1-flash:free";
    store.set_model_override(session_id, Some(chosen_model.to_string())).await.unwrap();

    let persisted = store.get_model_override(session_id).await.unwrap();
    assert_eq!(persisted, Some(chosen_model.to_string()));
}

#[tokio::test]
async fn test_tool_registry_patch_execution() {
    let registry = ToolRegistry::new();
    let test_dir = std::env::temp_dir().join("hermes_patch_test");
    let test_file = test_dir.join("sample.rs");
    let file_path_str = test_file.to_str().expect("Valid path");

    let _ = tokio::fs::create_dir_all(&test_dir).await;
    let initial_content = "fn main() {\n    let x = 10;\n    println!(\"{}\", x);\n}\n";
    tokio::fs::write(&test_file, initial_content).await.unwrap();

    let patch_call = ToolCall {
        id: "call_patch_1".to_string(),
        name: "patch".to_string(),
        arguments: json!({
            "path": file_path_str,
            "old_string": "    let x = 10;",
            "new_string": "    let x = 42;\n    let y = 100;"
        }),
    };

    let patch_res = registry.execute(&patch_call).await;
    assert!(patch_res.success, "Patch must succeed: {}", patch_res.output);
    let patch_json: serde_json::Value = serde_json::from_str(&patch_res.output).unwrap();
    assert_eq!(patch_json.get("success"), Some(&json!(true)));
    assert!(patch_json.get("diff").and_then(|v| v.as_str()).unwrap().contains("-    let x = 10;"));
    assert!(patch_json.get("diff").and_then(|v| v.as_str()).unwrap().contains("+    let x = 42;"));

    // Verify on disk
    let updated = tokio::fs::read_to_string(&test_file).await.unwrap();
    assert!(updated.contains("let x = 42;"));
    assert!(updated.contains("let y = 100;"));

    // Second run: test is_already_applied
    let patch_res_2 = registry.execute(&patch_call).await;
    assert!(patch_res_2.success);
    let json_2: serde_json::Value = serde_json::from_str(&patch_res_2.output).unwrap();
    assert_eq!(json_2.get("no_change"), Some(&json!(true)));

    let _ = tokio::fs::remove_file(&test_file).await;
    let _ = tokio::fs::remove_dir(&test_dir).await;
}

#[tokio::test]
async fn test_tool_registry_search_files_modes() {
    let registry = ToolRegistry::new();
    let test_dir = std::env::temp_dir().join("hermes_search_test");
    let dir_str = test_dir.to_str().unwrap();
    let file_a = test_dir.join("file_a.txt");
    let file_b = test_dir.join("file_b.txt");

    let _ = tokio::fs::create_dir_all(&test_dir).await;
    tokio::fs::write(&file_a, "target_token_123 in file a\nsecond line").await.unwrap();
    tokio::fs::write(&file_b, "different text\ntarget_token_123 in file b\nthird line").await.unwrap();

    // 1. Content search mode = "content"
    let call_content = ToolCall {
        id: "call_search_1".to_string(),
        name: "search_files".to_string(),
        arguments: json!({
            "path": dir_str,
            "pattern": "target_token_123",
            "target": "content"
        }),
    };
    let res_content = registry.execute(&call_content).await;
    assert!(res_content.success);
    let json_c: serde_json::Value = serde_json::from_str(&res_content.output).unwrap();
    assert_eq!(json_c.get("total_count").and_then(|v| v.as_u64()), Some(2));
    let matches = json_c.get("matches").and_then(|v| v.as_array()).unwrap();
    assert_eq!(matches.len(), 2);

    // 2. Mode = "files_only"
    let call_files_only = ToolCall {
        id: "call_search_2".to_string(),
        name: "search_files".to_string(),
        arguments: json!({
            "path": dir_str,
            "pattern": "target_token_123",
            "target": "content",
            "output_mode": "files_only"
        }),
    };
    let res_files_only = registry.execute(&call_files_only).await;
    assert!(res_files_only.success);
    let json_fo: serde_json::Value = serde_json::from_str(&res_files_only.output).unwrap();
    assert_eq!(json_fo.get("total_count").and_then(|v| v.as_u64()), Some(2));
    assert_eq!(json_fo.get("files").and_then(|v| v.as_array()).unwrap().len(), 2);

    // 3. Mode = "count"
    let call_count = ToolCall {
        id: "call_search_3".to_string(),
        name: "search_files".to_string(),
        arguments: json!({
            "path": dir_str,
            "pattern": "target_token_123",
            "target": "content",
            "output_mode": "count"
        }),
    };
    let res_count = registry.execute(&call_count).await;
    assert!(res_count.success);
    let json_cnt: serde_json::Value = serde_json::from_str(&res_count.output).unwrap();
    assert_eq!(json_cnt.get("total_count").and_then(|v| v.as_u64()), Some(2));

    // 4. Target = "files" (glob match)
    let call_find_files = ToolCall {
        id: "call_search_4".to_string(),
        name: "search_files".to_string(),
        arguments: json!({
            "path": dir_str,
            "pattern": "*.txt",
            "target": "files"
        }),
    };
    let res_find = registry.execute(&call_find_files).await;
    assert!(res_find.success);
    let json_find: serde_json::Value = serde_json::from_str(&res_find.output).unwrap();
    assert_eq!(json_find.get("total_count").and_then(|v| v.as_u64()), Some(2));

    let _ = tokio::fs::remove_file(&file_a).await;
    let _ = tokio::fs::remove_file(&file_b).await;
    let _ = tokio::fs::remove_dir(&test_dir).await;
}


#[tokio::test]
async fn test_tool_registry_web_search_and_extract_definitions_and_execution() {
    let registry = ToolRegistry::new();
    let defs = registry.get_definitions();

    assert_eq!(defs.len(), 23);

    let names: Vec<&str> = defs
        .iter()
        .filter_map(|d| d.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()))
        .collect();

    assert!(names.contains(&"web_search"));
    assert!(names.contains(&"web_extract"));
    assert!(names.contains(&"skills_list"));
    assert!(names.contains(&"skill_view"));

    // 1. SSRF rejection in execute(web_extract)
    let call_ssrf = ToolCall {
        id: "call_ssrf_1".to_string(),
        name: "web_extract".to_string(),
        arguments: json!({
            "urls": ["http://127.0.0.1:8080/admin", "http://169.254.169.254/latest/meta-data"]
        }),
    };
    let res_ssrf = registry.execute(&call_ssrf).await;
    assert!(res_ssrf.success);
    let val_ssrf: serde_json::Value = serde_json::from_str(&res_ssrf.output).unwrap();
    let results_arr = val_ssrf.get("results").and_then(|r| r.as_array()).unwrap();
    assert_eq!(results_arr.len(), 2);
    assert!(results_arr[0].get("error").is_some());
    assert!(results_arr[0].get("error").and_then(|e| e.as_str()).unwrap().contains("SSRF security violation"));
    assert!(results_arr[1].get("error").is_some());
    assert!(results_arr[1].get("error").and_then(|e| e.as_str()).unwrap().contains("SSRF security violation"));

    // 2. web_search execution test
    let call_search = ToolCall {
        id: "call_ws_1".to_string(),
        name: "web_search".to_string(),
        arguments: json!({
            "query": "rust programming language",
            "limit": 3
        }),
    };
    let res_search = registry.execute(&call_search).await;
    assert!(res_search.success);
    let val_search: serde_json::Value = serde_json::from_str(&res_search.output).unwrap();
    assert_eq!(val_search.get("success").and_then(|s| s.as_bool()), Some(true));
    assert!(val_search.get("data").is_some());
}

#[tokio::test]
async fn test_tool_registry_skills_list_and_view() {
    let registry = ToolRegistry::new();

    // 1. Test skills_list call
    let call_list = ToolCall {
        id: "call_skills_1".to_string(),
        name: "skills_list".to_string(),
        arguments: json!({}),
    };
    let res_list = registry.execute(&call_list).await;
    assert!(res_list.success);
    let parsed_list: serde_json::Value = serde_json::from_str(&res_list.output).unwrap();
    assert_eq!(parsed_list.get("success").and_then(|v| v.as_bool()), Some(true));

    // 2. Test skill_view call with non-existent skill
    let call_view = ToolCall {
        id: "call_skills_2".to_string(),
        name: "skill_view".to_string(),
        arguments: json!({
            "name": "non-existent-skill-xyz"
        }),
    };
    let res_view = registry.execute(&call_view).await;
    assert!(!res_view.success);
    let parsed_view: serde_json::Value = serde_json::from_str(&res_view.output).unwrap();
    assert_eq!(parsed_view.get("success").and_then(|v| v.as_bool()), Some(false));
}

#[tokio::test]
async fn test_busy_guard_and_stop_cancellation() {
    use hermes_gateway_core::{BusyPolicy, SlashCommand};
    use dashmap::DashMap;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;
    use uuid::Uuid;

    let active_turns = Arc::new(DashMap::new());
    let session_id = Uuid::new_v4();

    // 1. Session is idle
    assert!(!active_turns.contains_key(&session_id));

    // 2. Register active turn with CancellationToken
    let token = CancellationToken::new();
    active_turns.insert(session_id, token.clone());
    assert!(active_turns.contains_key(&session_id));
    assert!(!token.is_cancelled());

    // 3. BusyPolicy check: /model should be Rejected
    let model_cmd = SlashCommand::Model(Some("new-model".to_string()));
    assert_eq!(model_cmd.busy_policy(), BusyPolicy::Reject);

    // 4. BusyPolicy check: /status, /context, /whoami, /profile should be Dispatched
    assert_eq!(SlashCommand::Status.busy_policy(), BusyPolicy::Dispatch);
    assert_eq!(SlashCommand::Context.busy_policy(), BusyPolicy::Dispatch);
    assert_eq!(SlashCommand::Whoami.busy_policy(), BusyPolicy::Dispatch);
    assert_eq!(SlashCommand::Profile.busy_policy(), BusyPolicy::Dispatch);
    assert_eq!(SlashCommand::Diff(None).busy_policy(), BusyPolicy::Dispatch);

    // 5. BusyPolicy check: /stop and /clear should be InterruptThenDispatch
    assert_eq!(SlashCommand::Stop.busy_policy(), BusyPolicy::InterruptThenDispatch);
    assert_eq!(SlashCommand::Clear.busy_policy(), BusyPolicy::InterruptThenDispatch);

    // 6. Simulate /stop action: cancel token and clear active turn
    if let Some((_, tok)) = active_turns.remove(&session_id) {
        tok.cancel();
    }

    assert!(token.is_cancelled());
    assert!(!active_turns.contains_key(&session_id));
}

#[tokio::test]
async fn test_slash_commands_context_whoami_profile_diff() {
    use hermes_gateway_core::{handle_command, CommandContext, SlashCommand};
    use uuid::Uuid;

    let session_id = Uuid::new_v4();
    let ctx = CommandContext {
        gateway_name: "hermes-gateway-test",
        gateway_version: "0.2.0",
        active_profile: "production-profile",
        active_model: "ag/gemini-3.8-flash",
        telegram_connected: true,
        memory_stats: Some("32.5 MB RSS".to_string()),
        session_id: Some(session_id),
        platform: Some("telegram"),
        user_id: Some("tg_123456"),
        chat_id: Some("chat_987654"),
        chat_type: Some("dm"),
        thread_id: Some("thread_111"),
        transcript_count: 15,
        estimated_prompt_tokens: 25000,
        context_window: 100000,
    };

    // /context output test
    let ctx_out = handle_command(SlashCommand::Context, &ctx, |_| Ok(()));
    assert!(ctx_out.contains("⚡ <b>CONTEXT WINDOW</b>"));
    assert!(ctx_out.contains("ag/gemini-3.8-flash"));
    assert!(ctx_out.contains("Window:</b> 100,000 tokens"));
    assert!(ctx_out.contains("In use:</b> 25,000 / 100,000 (25.0%)"));
    assert!(ctx_out.contains("Bar:</b> <code>["));
    assert!(ctx_out.contains("Headroom:</b> 75,000 tokens"));
    assert!(ctx_out.contains("Messages in transcript:</b> 15"));

    // /whoami output test
    let who_out = handle_command(SlashCommand::Whoami, &ctx, |_| Ok(()));
    assert!(who_out.contains("You</b> — telegram (DM)"));
    assert!(who_out.contains("tg_123456"));
    assert!(who_out.contains("chat_987654"));
    assert!(who_out.contains("thread_111"));
    assert!(who_out.contains("production-profile"));
    assert!(who_out.contains(&session_id.to_string()));
    assert!(who_out.contains("Tier:</b> unrestricted"));

    // /profile output test
    let prof_out = handle_command(SlashCommand::Profile, &ctx, |_| Ok(()));
    assert!(prof_out.contains("Agent Profile Configuration"));
    assert!(prof_out.contains("production-profile"));
    assert!(prof_out.contains("ag/gemini-3.8-flash"));
    assert!(prof_out.contains(&session_id.to_string()));

    // /stop output test
    let stop_out = handle_command(SlashCommand::Stop, &ctx, |_| Ok(()));
    assert_eq!(stop_out, "⏹️ Agent execution stopped.");

    // /diff output test
    let diff_out = handle_command(SlashCommand::Diff(None), &ctx, |_| Ok(()));
    assert!(!diff_out.is_empty());
}


#[tokio::test]
async fn test_compress_command_and_preflight_auto_compaction_integration() {
    use hermes_gateway_core::{
        parse_command, handle_compress, SlashCommand, ContextCompressor,
        SUMMARY_PREFIX, HISTORICAL_TASK_HEADING,
    };
    use hermes_session_store::{MemorySessionStore, SessionStore};
    use hermes_protocol::{ChatMessage, Uuid};
    use hermes_gateway_core::llm::LlmClient;
    use std::sync::Arc;

    // 1. Verify command parsing for /compress and /compact
    assert_eq!(parse_command("/compress"), Some(SlashCommand::Compress(None)));
    assert_eq!(parse_command("/compact"), Some(SlashCommand::Compress(None)));
    assert_eq!(
        parse_command("/compress backend api"),
        Some(SlashCommand::Compress(Some("backend api".to_string())))
    );

    // 2. Setup mock session with 10 turns
    let store: Arc<Box<dyn SessionStore>> = Arc::new(Box::new(MemorySessionStore::new()));
    let session_id = Uuid::new_v4();

    for i in 1..=20 {
        let long_payload = format!("Comprehensive diagnostic turn {}. System reports high CPU utilization, memory pressure, active database locks in thread pool, and repeated HTTP 504 gateway timeout incidents across worker nodes. Detailed stack trace: error in worker_pool.rs line 214 caused by mutex deadlock in session scheduler. Investigated logs from /var/log/hermes/service.log with 100 matching error patterns and memory heap dump analysis.", i);
        if i % 2 == 1 {
            store.append_message(session_id, ChatMessage::user(format!("Step {}: {}", i, long_payload))).await.unwrap();
        } else {
            store.append_message(session_id, ChatMessage::assistant(format!("Step {}: Remediation details: {}", i, long_payload))).await.unwrap();
        }
    }

    let initial_msgs = store.get_messages(session_id, None).await.unwrap();
    assert_eq!(initial_msgs.len(), 20);
    let initial_tokens = ContextCompressor::estimate_tokens(&initial_msgs);

    // 3. Test ContextCompressor autonomous pre-flight threshold evaluation
    // With 10 messages (~200 tokens) and small context window (200 tokens), 0.85 threshold is met
    let mut compressor = ContextCompressor::new(0.85, 4);
    assert!(compressor.should_compress(initial_tokens, 100)); // initial_tokens is ~130 tokens, exceeding 85% of 100
    assert!(!compressor.should_compress(initial_tokens, 200_000));

    // 4. Execute compaction on the session
    let client = LlmClient::new(None, None);
    let result = compressor
        .compress_session(session_id, &store, &client, "gemini-2.5-flash", Some("backend error"), true)
        .await
        .unwrap();

    assert!(result.compacted);
    assert_eq!(result.pre_count, 20);
    // Compaction anchor + 4 tail messages = 5
    assert_eq!(result.post_count, 5);
    assert!(result.tokens_saved > 0);

    // 5. Verify format of the /compress confirmation message
    let confirmation = handle_compress(result.pre_count, result.post_count, result.tokens_saved);
    assert_eq!(
        confirmation,
        format!("🗜️ Context compacted: 20 messages → 5 messages ({} tokens saved).", result.tokens_saved)
    );

    // 6. Verify session store state after compaction
    let compacted_history = store.get_messages(session_id, None).await.unwrap();
    assert_eq!(compacted_history.len(), 5);

    // Message 0 is the compaction anchor
    let anchor = &compacted_history[0];
    assert_eq!(anchor.role, "user");
    let content = anchor.content.as_deref().unwrap();
    assert!(content.starts_with(SUMMARY_PREFIX));
    assert!(content.contains(HISTORICAL_TASK_HEADING));
    assert!(content.contains("## Goal"));
    assert!(content.contains("## Completed Actions"));

    // Messages 1..=4 are the protected tail messages preserved verbatim
    assert!(compacted_history[1].content.as_deref().unwrap().starts_with("Step 17:"));
    assert!(compacted_history[2].content.as_deref().unwrap().starts_with("Step 18:"));
    assert!(compacted_history[3].content.as_deref().unwrap().starts_with("Step 19:"));
    assert!(compacted_history[4].content.as_deref().unwrap().starts_with("Step 20:"));
}

#[tokio::test]
async fn test_browser_automation_suite_integration() {
    let registry = ToolRegistry::new();

    // 1. Verify schema definitions
    let defs = registry.get_definitions();
    let browser_tools = ["browser_navigate", "browser_snapshot", "browser_click", "browser_type", "browser_take_screenshot", "browser_exec"];
    for tool_name in &browser_tools {
        assert!(defs.iter().any(|d| d.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()) == Some(*tool_name)));
    }

    // 2. Test SSRF protection via execute(browser_navigate)
    let ssrf_call = ToolCall {
        id: "browser_nav_ssrf".to_string(),
        name: "browser_navigate".to_string(),
        arguments: json!({
            "url": "http://192.168.1.1/admin"
        }),
    };
    let res = registry.execute(&ssrf_call).await;
    assert!(!res.success);
    assert!(res.output.contains("SSRF security violation") || res.output.contains("Blocked"));

    // 3. Test missing required argument via execute(browser_type)
    let invalid_type_call = ToolCall {
        id: "browser_type_inv".to_string(),
        name: "browser_type".to_string(),
        arguments: json!({
            "selector": "input#username"
        }),
    };
    let res = registry.execute(&invalid_type_call).await;
    assert!(!res.success);
    assert!(res.output.contains("Missing required 'text'"));

    // 4. Test missing selector in browser_type
    let missing_sel_call = ToolCall {
        id: "browser_type_nosel".to_string(),
        name: "browser_type".to_string(),
        arguments: json!({
            "text": "hello"
        }),
    };
    let res = registry.execute(&missing_sel_call).await;
    assert!(!res.success);
    assert!(res.output.contains("Missing required 'selector'"));

    // 5. Test missing code in browser_exec
    let missing_code_call = ToolCall {
        id: "browser_exec_nocode".to_string(),
        name: "browser_exec".to_string(),
        arguments: json!({}),
    };
    let res = registry.execute(&missing_code_call).await;
    assert!(!res.success);

    // 6. Test browser_take_screenshot execution
    let screenshot_call = ToolCall {
        id: "browser_shot_1".to_string(),
        name: "browser_take_screenshot".to_string(),
        arguments: json!({
            "format": "png"
        }),
    };
    let res_shot = registry.execute(&screenshot_call).await;
    // Without active Chrome CDP, it cleanly returns failure error without crashing
    assert!(res_shot.output.contains("error") || res_shot.output.contains("Failed to connect") || res_shot.success);

    assert!(res.output.contains("Missing required 'code'"));
}
#[tokio::test]
async fn test_execute_code_tool_definition() {
    let registry = ToolRegistry::new();
    let defs = registry.get_definitions();

    let exec_code_def = defs
        .iter()
        .find(|d| d.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()) == Some("execute_code"))
        .expect("execute_code definition must be present");

    assert_eq!(exec_code_def.get("type").and_then(|t| t.as_str()), Some("function"));
    let func = exec_code_def.get("function").expect("function object");
    assert_eq!(func.get("name").and_then(|n| n.as_str()), Some("execute_code"));
    let desc = func.get("description").and_then(|d| d.as_str()).expect("description");
    assert!(desc.contains("Run Python that calls Hermes tools programmatically"));
    assert!(desc.contains("from hermes_tools import"));

    let params = func.get("parameters").expect("parameters");
    assert_eq!(params.get("type").and_then(|t| t.as_str()), Some("object"));
    let props = params.get("properties").expect("properties");
    assert!(props.get("code").is_some());
    assert!(props.get("reset").is_some());

    let req = params.get("required").and_then(|r| r.as_array()).expect("required");
    assert!(req.contains(&json!("code")));
}

#[tokio::test]
async fn test_execute_code_integration() {
    let registry = ToolRegistry::new();

    // 1. Basic Python calculation
    let call_basic = ToolCall {
        id: "call_exec_1".to_string(),
        name: "execute_code".to_string(),
        arguments: json!({
            "session_id": "test_integ_exec",
            "code": "result = 123 * 456\nprint(f'CALC_RESULT={result}')"
        }),
    };

    let res_basic = registry.execute(&call_basic).await;
    assert_eq!(res_basic.tool_call_id, "call_exec_1");
    assert!(res_basic.success, "execute_code should succeed: {}", res_basic.output);
    let parsed: serde_json::Value = serde_json::from_str(&res_basic.output).expect("valid json");
    assert_eq!(parsed.get("status").and_then(|v| v.as_str()), Some("success"));
    let out = parsed.get("output").and_then(|v| v.as_str()).unwrap();
    assert!(out.contains("CALC_RESULT=56088"));

    // 2. State persistence across turns
    let call_persist = ToolCall {
        id: "call_exec_2".to_string(),
        name: "execute_code".to_string(),
        arguments: json!({
            "session_id": "test_integ_exec",
            "code": "print(f'PERSIST_VERIFY={result + 12}')"
        }),
    };
    let res_persist = registry.execute(&call_persist).await;
    assert!(res_persist.success);
    let parsed_persist: serde_json::Value = serde_json::from_str(&res_persist.output).unwrap();
    let out_persist = parsed_persist.get("output").and_then(|v| v.as_str()).unwrap();
    assert!(out_persist.contains("PERSIST_VERIFY=56100"));

    // 3. Reset flag test
    let call_reset = ToolCall {
        id: "call_exec_3".to_string(),
        name: "execute_code".to_string(),
        arguments: json!({
            "session_id": "test_integ_exec",
            "reset": true,
            "code": "print('result_exists:', 'result' in globals() or 'result' in locals())"
        }),
    };
    let res_reset = registry.execute(&call_reset).await;
    assert!(res_reset.success);
    let parsed_reset: serde_json::Value = serde_json::from_str(&res_reset.output).unwrap();
    let out_reset = parsed_reset.get("output").and_then(|v| v.as_str()).unwrap();
    assert!(out_reset.contains("result_exists: False"));

    // 4. Missing required code argument
    let call_nocode = ToolCall {
        id: "call_exec_4".to_string(),
        name: "execute_code".to_string(),
        arguments: json!({}),
    };
    let res_nocode = registry.execute(&call_nocode).await;
    assert!(!res_nocode.success);
    assert!(res_nocode.output.contains("No code provided"));
}


#[tokio::test]
async fn test_todo_list_tool_definition_and_execution() {
    let registry = ToolRegistry::new();
    let defs = registry.get_definitions();

    // 1. Verify schema definition
    let todo_def = defs
        .iter()
        .find(|d| d.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()) == Some("todo_list"))
        .expect("todo_list tool definition must exist");

    let func = todo_def.get("function").unwrap();
    assert_eq!(func.get("name").and_then(|n| n.as_str()), Some("todo_list"));
    let params = func.get("parameters").unwrap();
    let props = params.get("properties").unwrap();
    assert!(props.get("todos").is_some());
    assert!(props.get("merge").is_some());

    // 2. Read empty initial list
    let read_call = ToolCall {
        id: "call_todo_read".to_string(),
        name: "todo_list".to_string(),
        arguments: json!({}),
    };
    let read_res = registry.execute(&read_call).await;
    assert!(read_res.success);
    let read_json: serde_json::Value = serde_json::from_str(&read_res.output).unwrap();
    assert_eq!(read_json["todos"].as_array().unwrap().len(), 0);
    assert_eq!(read_json["revision"], 0);
    assert_eq!(read_json["summary"]["total"], 0);

    // 3. Write items (replace / fresh)
    let write_call = ToolCall {
        id: "call_todo_write".to_string(),
        name: "todo_list".to_string(),
        arguments: json!({
            "todos": [
                { "id": "t1", "content": "First task", "status": "pending" },
                { "id": "t2", "content": "Second task", "status": "in_progress" }
            ],
            "merge": false
        }),
    };
    let write_res = registry.execute(&write_call).await;
    assert!(write_res.success);
    let write_json: serde_json::Value = serde_json::from_str(&write_res.output).unwrap();
    assert_eq!(write_json["todos"].as_array().unwrap().len(), 2);
    assert_eq!(write_json["revision"], 1);
    assert_eq!(write_json["summary"]["total"], 2);
    assert_eq!(write_json["summary"]["pending"], 1);
    assert_eq!(write_json["summary"]["in_progress"], 1);

    // 4. Merge update
    let merge_call = ToolCall {
        id: "call_todo_merge".to_string(),
        name: "todo_list".to_string(),
        arguments: json!({
            "todos": [
                { "id": "t1", "status": "completed" },
                { "id": "t3", "content": "Third task", "status": "pending" }
            ],
            "merge": true
        }),
    };
    let merge_res = registry.execute(&merge_call).await;
    assert!(merge_res.success);
    let merge_json: serde_json::Value = serde_json::from_str(&merge_res.output).unwrap();
    assert_eq!(merge_json["todos"].as_array().unwrap().len(), 3);
    assert_eq!(merge_json["revision"], 2);
    assert_eq!(merge_json["summary"]["total"], 3);
    assert_eq!(merge_json["summary"]["completed"], 1);
    assert_eq!(merge_json["summary"]["in_progress"], 1);
    assert_eq!(merge_json["summary"]["pending"], 1);

    // 5. Verify format_for_injection from registry.todo_store
    let formatted = registry.todo_store.format_for_injection().await;
    assert!(formatted.is_some());
    let text = formatted.unwrap();
    assert!(text.contains("[Your active task list was preserved across context compression]"));
    assert!(text.contains("- [>] t2. Second task (in_progress)"));
    assert!(text.contains("- [ ] t3. Third task (pending)"));
    assert!(!text.contains("t1")); // completed item omitted from active injection
}

#[tokio::test]
async fn test_cronjob_manage_tool_integration() {
    let cron_scheduler = std::sync::Arc::new(hermes_cron::CronScheduler::new(None).await.expect("scheduler init"));
    let registry = ToolRegistry::new().with_cron_scheduler(cron_scheduler);

    // 1. Verify cronjob_manage tool definition exists
    let defs = registry.get_definitions();
    let cron_def = defs.iter().find(|d| d["function"]["name"] == "cronjob_manage");
    assert!(cron_def.is_some(), "cronjob_manage tool definition must be registered");

    // 2. Action: create
    let create_call = ToolCall {
        id: "call_cron_create".to_string(),
        name: "cronjob_manage".to_string(),
        arguments: json!({
            "action": "create",
            "schedule": "every 30m",
            "prompt": "Run periodic health scan",
            "name": "health-scan",
            "deliver": "origin",
            "pinned": true
        }),
    };
    let create_res = registry.execute(&create_call).await;
    assert!(create_res.success, "create should succeed: {}", create_res.output);
    let create_json: serde_json::Value = serde_json::from_str(&create_res.output).unwrap();
    assert_eq!(create_json["success"], true);
    let job_id = create_json["job_id"].as_str().unwrap().to_string();
    assert_eq!(create_json["name"], "health-scan");
    assert_eq!(create_json["job"]["pinned"], true);
    assert_eq!(create_json["job"]["state"], "scheduled");

    // 3. Action: list
    let list_call = ToolCall {
        id: "call_cron_list".to_string(),
        name: "cronjob_manage".to_string(),
        arguments: json!({
            "action": "list"
        }),
    };
    let list_res = registry.execute(&list_call).await;
    assert!(list_res.success, "list should succeed");
    let list_json: serde_json::Value = serde_json::from_str(&list_res.output).unwrap();
    assert_eq!(list_json["success"], true);
    assert_eq!(list_json["count"], 1);
    assert_eq!(list_json["jobs"][0]["job_id"], job_id);

    // 4. Action: update
    let update_call = ToolCall {
        id: "call_cron_update".to_string(),
        name: "cronjob_manage".to_string(),
        arguments: json!({
            "action": "update",
            "job_id": job_id,
            "prompt": "Run deep health scan",
            "name": "deep-health-scan"
        }),
    };
    let update_res = registry.execute(&update_call).await;
    assert!(update_res.success, "update should succeed: {}", update_res.output);
    let update_json: serde_json::Value = serde_json::from_str(&update_res.output).unwrap();
    assert_eq!(update_json["success"], true);
    assert_eq!(update_json["job"]["name"], "deep-health-scan");

    // 5. Action: pause
    let pause_call = ToolCall {
        id: "call_cron_pause".to_string(),
        name: "cronjob_manage".to_string(),
        arguments: json!({
            "action": "pause",
            "job_id": job_id,
            "paused_reason": "scheduled maintenance"
        }),
    };
    let pause_res = registry.execute(&pause_call).await;
    assert!(pause_res.success, "pause should succeed: {}", pause_res.output);
    let pause_json: serde_json::Value = serde_json::from_str(&pause_res.output).unwrap();
    assert_eq!(pause_json["job"]["state"], "paused");
    assert_eq!(pause_json["job"]["enabled"], false);
    assert_eq!(pause_json["job"]["paused_reason"], "scheduled maintenance");

    // 6. Action: resume
    let resume_call = ToolCall {
        id: "call_cron_resume".to_string(),
        name: "cronjob_manage".to_string(),
        arguments: json!({
            "action": "resume",
            "job_id": job_id
        }),
    };
    let resume_res = registry.execute(&resume_call).await;
    assert!(resume_res.success, "resume should succeed: {}", resume_res.output);
    let resume_json: serde_json::Value = serde_json::from_str(&resume_res.output).unwrap();
    assert_eq!(resume_json["job"]["state"], "scheduled");
    assert_eq!(resume_json["job"]["enabled"], true);

    // 7. Action: run
    let run_call = ToolCall {
        id: "call_cron_run".to_string(),
        name: "cronjob_manage".to_string(),
        arguments: json!({
            "action": "run",
            "job_id": job_id,
            "prompt": "Transient fire context"
        }),
    };
    let run_res = registry.execute(&run_call).await;
    assert!(run_res.success, "run should succeed: {}", run_res.output);
    let run_json: serde_json::Value = serde_json::from_str(&run_res.output).unwrap();
    assert_eq!(run_json["success"], true);
    assert_eq!(run_json["job"]["executed"], true);
    assert_eq!(run_json["job"]["execution_mode"], "background");

    // 8. Action: remove
    let remove_call = ToolCall {
        id: "call_cron_remove".to_string(),
        name: "cronjob_manage".to_string(),
        arguments: json!({
            "action": "remove",
            "job_id": job_id
        }),
    };
    let remove_res = registry.execute(&remove_call).await;
    assert!(remove_res.success, "remove should succeed: {}", remove_res.output);
    let remove_json: serde_json::Value = serde_json::from_str(&remove_res.output).unwrap();
    assert_eq!(remove_json["success"], true);
    assert_eq!(remove_json["removed_job"]["id"], job_id);

    // Verify empty list after remove
    let list_res2 = registry.execute(&list_call).await;
    let list_json2: serde_json::Value = serde_json::from_str(&list_res2.output).unwrap();
    assert_eq!(list_json2["count"], 0);
}


#[tokio::test]
async fn test_background_review_and_reflection_integration() {
    use hermes_gateway_core::reflection::{
        BackgroundReviewConfig, BackgroundReviewEngine, LearningSignal,
        SKILL_REVIEW_PROMPT, MEMORY_REVIEW_PROMPT, COMBINED_REVIEW_PROMPT,
    };
    use hermes_gateway_core::llm::{ChatMessage, LlmClient};
    use hermes_skills::SkillRegistry;
    use std::sync::Arc;
    use uuid::Uuid;

    // Verify prompts are non-empty and well-formed
    assert!(SKILL_REVIEW_PROMPT.contains("Review the conversation above and update the skill library"));
    assert!(MEMORY_REVIEW_PROMPT.contains("Review the conversation above and consider saving to memory"));
    assert!(COMBINED_REVIEW_PROMPT.contains("**Memory**") && COMBINED_REVIEW_PROMPT.contains("**Skills**"));

    let skill_registry = Arc::new(SkillRegistry::default());
    let provider_profile = hermes_gateway_core::providers::ProviderProfile::default();
    let llm_client = Arc::new(LlmClient::from_provider_profile(&provider_profile, None, None));

    let config = BackgroundReviewConfig {
        enabled: true,
        interval_turns: 1,
        min_messages: 2,
        max_history_messages: 20,
    };

    let engine = Arc::new(BackgroundReviewEngine::new(config, llm_client, skill_registry.clone()));

    // 1. Signal detection test
    let history = vec![
        ChatMessage::user("How do I build this crate?".to_string()),
        ChatMessage::assistant("You run cargo build directly.".to_string()),
        ChatMessage::user("Stop doing that, don't build without checking dependencies first!".to_string()),
        ChatMessage::assistant("Understood, the solution is checking Cargo.lock before running builds.".to_string()),
    ];

    let signals = BackgroundReviewEngine::detect_learning_signals(&history);
    assert_eq!(signals.len(), 2);
    assert!(matches!(signals[0], LearningSignal::UserCorrection(_)));
    assert!(matches!(signals[1], LearningSignal::TechnicalSolution(_)));

    assert!(engine.should_review(&history));

    // 2. Non-blocking maybe_spawn_review invocation
    let session_id = Uuid::new_v4();
    engine.maybe_spawn_review(session_id, &history, "test-model");

    // 3. Test skill curation/persistence
    let sample_skill_md = r#"---
name: "cargo-lock-check"
description: "Verify Cargo.lock before invoking cargo build"
---
# Cargo Lock Check
Always check Cargo.lock presence and freshness before running cargo build.
"#;
    BackgroundReviewEngine::persist_extracted_learnings(&skill_registry, sample_skill_md).await;

    let loaded = skill_registry.get("cargo-lock-check").await;
    assert!(loaded.is_some(), "Curated skill should be registered in SkillRegistry");
    let skill = loaded.unwrap();
    assert_eq!(skill.name, "cargo-lock-check");
    assert_eq!(skill.description, "Verify Cargo.lock before invoking cargo build");
    assert!(skill.content.contains("Always check Cargo.lock"));
}

#[tokio::test]
async fn test_delegate_task_tool_integration() {
    use hermes_gateway_core::delegation::DelegationManager;
    use hermes_gateway_core::tools::{ToolCall, ToolRegistry};
    use std::sync::Arc;

    let delegation_manager = Arc::new(DelegationManager::new());
    let registry = ToolRegistry::new().with_delegation_manager(delegation_manager.clone());

    // 1. Verify delegate_task definition schema
    let defs = registry.get_definitions();
    let delegate_def = defs.iter().find(|d| {
        d.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()) == Some("delegate_task")
    }).expect("delegate_task tool must be present in ToolRegistry definitions");

    let params = &delegate_def["function"]["parameters"];
    assert!(params["properties"].get("tasks").is_some());
    assert!(params["properties"].get("action").is_some());
    assert!(params["properties"].get("subagent_id").is_some());
    assert!(params["properties"].get("message").is_some());

    // 2. Test action="list" via execute
    let list_call = ToolCall {
        id: "call_list_1".to_string(),
        name: "delegate_task".to_string(),
        arguments: serde_json::json!({
            "action": "list"
        }),
    };
    let list_result = registry.execute(&list_call).await;
    assert!(list_result.success);
    let list_val: serde_json::Value = serde_json::from_str(&list_result.output).unwrap();
    assert_eq!(list_val["action"], "list");
    assert_eq!(list_val["count"], 0);

    // 3. Test action="spawn" via execute
    let spawn_call = ToolCall {
        id: "call_spawn_1".to_string(),
        name: "delegate_task".to_string(),
        arguments: serde_json::json!({
            "action": "spawn",
            "tasks": [
                {
                    "goal": "Calculate arithmetic 2 + 2",
                    "context": "Testing subagent delegation engine in Rust"
                }
            ]
        }),
    };
    let spawn_result = registry.execute(&spawn_call).await;
    assert!(spawn_result.success, "Spawn should succeed: {}", spawn_result.output);
    let spawn_val: serde_json::Value = serde_json::from_str(&spawn_result.output).unwrap();
    assert_eq!(spawn_val["action"], "spawn");
    assert_eq!(spawn_val["count"], 1);
    let subagent_res = &spawn_val["results"][0];
    assert_eq!(subagent_res["status"], "completed");
    assert!(subagent_res["summary"].as_str().unwrap().contains("Calculate arithmetic 2 + 2"));

    // 4. Test action="steer" and action="stop" on non-existent subagent
    let steer_call = ToolCall {
        id: "call_steer_1".to_string(),
        name: "delegate_task".to_string(),
        arguments: serde_json::json!({
            "action": "steer",
            "subagent_id": "non_existent_sub",
            "message": "Change strategy now"
        }),
    };
    let steer_result = registry.execute(&steer_call).await;
    assert!(!steer_result.success);
    assert!(steer_result.output.contains("No live subagent"));

    let stop_call = ToolCall {
        id: "call_stop_1".to_string(),
        name: "delegate_task".to_string(),
        arguments: serde_json::json!({
            "action": "stop",
            "subagent_id": "non_existent_sub"
        }),
    };
    let stop_result = registry.execute(&stop_call).await;
    assert!(!stop_result.success);
    assert!(stop_result.output.contains("No live subagent"));
}

#[tokio::test]
async fn test_session_search_tool_integration() {
    use hermes_protocol::{ChatMessage, Platform, Session, SessionState};
    use hermes_session_store::{MemorySessionStore, SessionStore};
    use std::sync::Arc;

    let store: Arc<Box<dyn SessionStore>> = Arc::new(Box::new(MemorySessionStore::new()));
    let registry = ToolRegistry::new().with_session_store(store.clone());

    // 1. Verify session_search tool definition exists in get_definitions
    let defs = registry.get_definitions();
    let ss_def = defs.iter().find(|d| d["function"]["name"] == "session_search");
    assert!(ss_def.is_some(), "session_search definition must be registered in ToolRegistry");

    // 2. Populate store with test sessions
    let sid1 = uuid::Uuid::new_v4();
    let sid2 = uuid::Uuid::new_v4();

    let mut session1 = Session {
        id: sid1,
        profile: "default".to_string(),
        platform: Platform::Telegram,
        platform_session_id: "tg_integration_1".to_string(),
        state: SessionState::Active,
        created_at: time::OffsetDateTime::now_utc(),
        updated_at: time::OffsetDateTime::now_utc(),
        metadata: std::collections::HashMap::new(),
    };
    session1.metadata.insert("title".to_string(), serde_json::json!("Rust Gateway Core Discussion"));
    store.set(&session1).await.unwrap();

    let mut session2 = Session {
        id: sid2,
        profile: "default".to_string(),
        platform: Platform::Discord,
        platform_session_id: "dc_integration_2".to_string(),
        state: SessionState::Active,
        created_at: time::OffsetDateTime::now_utc(),
        updated_at: time::OffsetDateTime::now_utc(),
        metadata: std::collections::HashMap::new(),
    };
    session2.metadata.insert("title".to_string(), serde_json::json!("Python Recall Architecture"));
    store.set(&session2).await.unwrap();

    store.append_message(sid1, ChatMessage::user("Can you explain the session_search tool implementation?")).await.unwrap();
    store.append_message(sid1, ChatMessage::assistant("It implements discovery, browse, read, and scroll shapes in Rust.")).await.unwrap();
    store.append_message(sid1, ChatMessage::user("What is the maximum message content cap?")).await.unwrap();
    store.append_message(sid1, ChatMessage::assistant("The cap is 2000 characters per message to prevent context explosion.")).await.unwrap();

    store.append_message(sid2, ChatMessage::user("How does FTS5 ranking demote cron jobs?")).await.unwrap();
    store.append_message(sid2, ChatMessage::assistant("Cron jobs are demoted to prevent recall blindness.")).await.unwrap();

    // 3. Test Browse Shape via ToolRegistry::execute
    let browse_call = ToolCall {
        id: "call_ss_browse".to_string(),
        name: "session_search".to_string(),
        arguments: serde_json::json!({}),
    };
    let browse_res = registry.execute(&browse_call).await;
    assert!(browse_res.success);
    let browse_val: serde_json::Value = serde_json::from_str(&browse_res.output).unwrap();
    assert_eq!(browse_val["mode"], "browse");
    assert_eq!(browse_val["count"], 2);

    // 4. Test Discovery Shape via ToolRegistry::execute
    let disc_call = ToolCall {
        id: "call_ss_disc".to_string(),
        name: "session_search".to_string(),
        arguments: serde_json::json!({
            "query": "maximum message content cap",
            "limit": 5,
            "detail": "adaptive"
        }),
    };
    let disc_res = registry.execute(&disc_call).await;
    assert!(disc_res.success);
    let disc_val: serde_json::Value = serde_json::from_str(&disc_res.output).unwrap();
    assert_eq!(disc_val["mode"], "discover");
    assert_eq!(disc_val["count"], 1);
    assert_eq!(disc_val["results"][0]["session_id"], sid1.to_string());

    // 5. Test Read Shape via ToolRegistry::execute
    let read_call = ToolCall {
        id: "call_ss_read".to_string(),
        name: "session_search".to_string(),
        arguments: serde_json::json!({
            "session_id": sid1.to_string(),
            "head": 2,
            "tail": 2
        }),
    };
    let read_res = registry.execute(&read_call).await;
    assert!(read_res.success);
    let read_val: serde_json::Value = serde_json::from_str(&read_res.output).unwrap();
    assert_eq!(read_val["mode"], "read");
    assert_eq!(read_val["session_id"], sid1.to_string());
    assert_eq!(read_val["message_count"], 4);

    // 6. Test Scroll Shape via ToolRegistry::execute
    let scroll_call = ToolCall {
        id: "call_ss_scroll".to_string(),
        name: "session_search".to_string(),
        arguments: serde_json::json!({
            "session_id": sid1.to_string(),
            "around_message_id": "1",
            "window": 1
        }),
    };
    let scroll_res = registry.execute(&scroll_call).await;
    assert!(scroll_res.success);
    let scroll_val: serde_json::Value = serde_json::from_str(&scroll_res.output).unwrap();
    assert_eq!(scroll_val["mode"], "scroll");
    assert_eq!(scroll_val["session_id"], sid1.to_string());
    let messages = scroll_val["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[1]["anchor"], true);
}


#[tokio::test]
async fn test_memory_tool_integration() {
    use hermes_gateway_core::memory::MemoryStore;
    use std::sync::Arc;
    use uuid::Uuid;

    let dir = std::env::temp_dir().join(format!("hermes_mem_int_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let store = Arc::new(MemoryStore::with_base_dir(dir.clone()));
    let registry = ToolRegistry::new().with_memory_store(store.clone());

    // 1. The memory definition is registered.
    let defs = registry.get_definitions();
    let mem_def = defs.iter().find(|d| d["function"]["name"] == "memory");
    assert!(mem_def.is_some(), "memory definition must be registered in ToolRegistry");

    // 2. Add a user-profile fact through ToolRegistry dispatch.
    let add_user = ToolCall {
        id: "call_mem_user".to_string(),
        name: "memory".to_string(),
        arguments: serde_json::json!({
            "action": "add",
            "target": "user",
            "content": "Prefers terse, result-first replies."
        }),
    };
    let res_user = registry.execute(&add_user).await;
    assert!(res_user.success, "user add must succeed: {}", res_user.output);
    let v_user: serde_json::Value = serde_json::from_str(&res_user.output).unwrap();
    assert_eq!(v_user["success"], serde_json::json!(true));
    assert_eq!(v_user["target"], serde_json::json!("user"));

    // 3. Add an agent note on the other target through the same dispatch.
    let add_mem = ToolCall {
        id: "call_mem_agent".to_string(),
        name: "memory".to_string(),
        arguments: serde_json::json!({
            "action": "add",
            "target": "memory",
            "content": "Gateway runs a single-node scheduler."
        }),
    };
    let res_mem = registry.execute(&add_mem).await;
    assert!(res_mem.success, "memory add must succeed: {}", res_mem.output);
    let v_mem: serde_json::Value = serde_json::from_str(&res_mem.output).unwrap();
    assert_eq!(v_mem["target"], serde_json::json!("memory"));
    assert_eq!(v_mem["entry_count"], serde_json::json!(1));

    // 4. The two targets are isolated and both persisted to disk in the delimiter format.
    assert_eq!(store.entries_for("user"), vec!["Prefers terse, result-first replies.".to_string()]);
    assert_eq!(store.entries_for("memory"), vec!["Gateway runs a single-node scheduler.".to_string()]);

    // 5. A batch call frees room and adds atomically in one dispatch.
    let batch = ToolCall {
        id: "call_mem_batch".to_string(),
        name: "memory".to_string(),
        arguments: serde_json::json!({
            "target": "memory",
            "operations": [
                {"action": "remove", "old_text": "single-node scheduler"},
                {"action": "add", "content": "Gateway uses a single-node scheduler with disk persistence."}
            ]
        }),
    };
    let res_batch = registry.execute(&batch).await;
    assert!(res_batch.success, "batch must succeed: {}", res_batch.output);
    let v_batch: serde_json::Value = serde_json::from_str(&res_batch.output).unwrap();
    assert_eq!(v_batch["success"], serde_json::json!(true));
    assert_eq!(v_batch["entry_count"], serde_json::json!(1));
    assert_eq!(
        store.entries_for("memory"),
        vec!["Gateway uses a single-node scheduler with disk persistence.".to_string()]
    );

    // 6. Old file format round-trips: reload from disk into a fresh store.
    let reloaded = MemoryStore::with_base_dir(dir.clone());
    assert_eq!(
        reloaded.entries_for("memory"),
        vec!["Gateway uses a single-node scheduler with disk persistence.".to_string()]
    );

    let _ = std::fs::remove_dir_all(&dir);
}
