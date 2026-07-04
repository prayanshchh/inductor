use std::{
    fs,
    path::PathBuf,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

use context::{ApproxTokenCounter, BlobStore, ContextLimits, prepare_context};
use context::{ModelEffort, ProviderFamily};
use futures_util::StreamExt;
use harness_core::{
    AllowRule, AllowRuleKind, ApprovalPolicy, MessagePart, ModelInfo, PermissionDecision,
    ProviderCapabilities, SessionEvent, StopReason, ToolCallId,
};
use provider_core::{ProviderAuth, ProviderAuthKind, ProviderPlugin};
use secrecy::SecretString;

use super::*;

// --- Pure parsing tests ----------------------------------------------------

#[test]
fn parse_tool_call_returns_none_without_envelope() {
    assert!(parse_tool_call("just a plain answer, no tools here").is_none());
}

#[test]
fn parse_tool_call_extracts_name_and_input() {
    let text = "thinking...\n<inductor_tool_call>{\"name\":\"read_file\",\"input\":{\"path\":\"Cargo.toml\"}}</inductor_tool_call>";

    let parsed = parse_tool_call(text).unwrap().unwrap();

    assert_eq!(parsed.name, "read_file");
    assert_eq!(parsed.input, json!({ "path": "Cargo.toml" }));
}

#[test]
fn parse_tool_call_defaults_missing_input_to_empty_object() {
    let parsed = parse_tool_call("<inductor_tool_call>{\"name\":\"grep\"}</inductor_tool_call>")
        .unwrap()
        .unwrap();

    assert_eq!(parsed.input, json!({}));
}

#[test]
fn parse_tool_call_reports_unterminated_envelope() {
    let error = parse_tool_call("<inductor_tool_call>{\"name\":\"grep\"}")
        .unwrap()
        .unwrap_err();

    assert_eq!(error, ToolCallParseError::Unterminated);
}

#[test]
fn parse_tool_call_reports_invalid_json() {
    let error = parse_tool_call("<inductor_tool_call>not json</inductor_tool_call>")
        .unwrap()
        .unwrap_err();

    assert!(matches!(error, ToolCallParseError::InvalidJson(_)));
}

#[test]
fn parse_tool_call_reports_missing_name() {
    let error = parse_tool_call("<inductor_tool_call>{\"input\":{}}</inductor_tool_call>")
        .unwrap()
        .unwrap_err();

    assert_eq!(error, ToolCallParseError::MissingName);
}

#[test]
fn web_fetch_is_classified_as_network_access() {
    let call = ParsedToolCall {
        name: "web_fetch".to_string(),
        input: json!({ "url": "https://example.com" }),
    };

    assert!(risk::classify(&call).contains(&RiskFlag::NetworkAccess));
}

#[test]
fn allow_store_supports_wildcard_tool_rules() {
    let mut allow = AllowStore::new();
    allow.add(AllowRule {
        kind: AllowRuleKind::ToolName,
        value: "web_*".to_string(),
    });

    let call = ParsedToolCall {
        name: "web_fetch".to_string(),
        input: json!({ "url": "https://example.com" }),
    };

    assert!(allow.is_allowed(&call));
}

// --- Transcript rendering --------------------------------------------------

#[test]
fn render_prompt_includes_preamble_and_transcript() {
    let mut state = SessionState::new(SessionId::new());
    state.push(Role::User, "read the file");
    state.push(Role::Tool, "read_file result:\nhello");

    let counter = ApproxTokenCounter;
    let preamble = generic_tools_preamble();
    let prompt = prepare_context(
        &preamble,
        &state.context_messages(),
        &ContextLimits::default(),
        &counter,
    )
    .unwrap()
    .prompt;

    assert!(prompt.contains("Inductor coding agent"));
    assert!(prompt.contains("User:\nread the file"));
    assert!(prompt.contains("Tool:\nread_file result:\nhello"));
    assert!(prompt.trim_end().ends_with("Assistant:"));
}

#[test]
fn context_preparation_compacts_when_soft_limit_is_exceeded() {
    let mut state = SessionState::new(SessionId::new());
    for index in 0..10 {
        state.push(Role::User, format!("message {index} {}", "x".repeat(80)));
    }

    let counter = ApproxTokenCounter;
    let preamble = generic_tools_preamble();
    let prepared = prepare_context(
        &preamble,
        &state.context_messages(),
        &ContextLimits::new(100, 2_000, 1024),
        &counter,
    )
    .unwrap();

    assert!(prepared.compacted);
    assert!(prepared.prompt.contains("Compacted"));
}

#[test]
fn effort_prompt_hint_is_added_for_claude() {
    let preamble = system_preamble_for_effort(
        ProviderFamily::Claude,
        ModelEffort::High,
        &test_environment(),
    );

    assert!(preamble.contains("high reasoning effort"));
}

#[test]
fn native_tools_preamble_includes_environment_context() {
    let preamble = system_preamble_for_effort(
        ProviderFamily::Codex,
        ModelEffort::Medium,
        &test_environment(),
    );

    assert!(preamble.contains("<env>"));
    assert!(preamble.contains("Model: test-model"));
    assert!(preamble.contains("Workspace root: /tmp/inductor-workspace"));
    assert!(preamble.contains("Is workspace a git repo: yes"));
    assert!(preamble.contains("Current date (UTC): 2026-06-12"));
}

#[test]
fn provider_request_preparer_builds_complete_turn_request() {
    let temp = TempDir::new("provider-request-prep");
    let tools = ToolRuntime::new(temp.path()).unwrap();
    let mut state = SessionState::new(SessionId::new());
    state.push(Role::User, "inspect this image");
    let image = harness_core::ImageAttachment {
        path: Some("screen.png".to_string()),
        mime_type: "image/png".to_string(),
        base64_data: "abc123".to_string(),
        width: Some(10),
        height: Some(20),
        file_size: 6,
    };
    let mut config = HarnessConfig::new("gpt-5.5");
    config.provider_family = ProviderFamily::Codex;
    config.model_effort = ModelEffort::High;
    config.approval_policy = ApprovalPolicy::OnRequest;

    let prepared = ProviderRequestPreparer::prepare(ProviderRequestInput {
        session_id: state.session_id,
        round: 2,
        state: &state,
        turn_images: vec![image.clone()],
        config: &config,
        tools: &tools,
    })
    .unwrap();

    assert!(matches!(
        prepared.context_event,
        SessionEvent::ContextPrepared { token_count, .. } if token_count > 0
    ));
    assert_eq!(prepared.request.session_id, state.session_id);
    assert_eq!(prepared.request.model, "gpt-5.5");
    assert!(prepared.request.prompt.contains("inspect this image"));
    assert!(
        prepared
            .request
            .system_prompt
            .as_deref()
            .unwrap()
            .contains("<env>")
    );
    assert!(
        prepared
            .request
            .tool_names
            .contains(&"read_file".to_string())
    );
    assert_eq!(prepared.request.metadata["round"], json!(2));
    assert_eq!(prepared.request.metadata["model_effort"], json!("high"));
    assert_eq!(prepared.request.images, vec![image.clone()]);
    assert!(prepared.request.messages.iter().any(|message| {
        message.role == "user"
            && message.parts.iter().any(|part| matches!(part, MessagePart::Image { image: part_image } if part_image == &image))
    }));
}

#[test]
fn provider_request_preparer_uses_canonical_xhigh_effort() {
    let temp = TempDir::new("provider-request-xhigh-effort");
    let tools = ToolRuntime::new(temp.path()).unwrap();
    let mut state = SessionState::new(SessionId::new());
    state.push(Role::User, "think harder");
    let mut config = HarnessConfig::new("test-model");
    config.model_effort = ModelEffort::XHigh;

    let prepared = ProviderRequestPreparer::prepare(ProviderRequestInput {
        session_id: state.session_id,
        round: 0,
        state: &state,
        turn_images: Vec::new(),
        config: &config,
        tools: &tools,
    })
    .unwrap();

    assert_eq!(prepared.request.metadata["model_effort"], json!("xhigh"));
}

#[test]
fn prompt_composer_orders_configured_and_plugin_layers() {
    let prompt = PromptRuntimeConfig::default().with_system_layer("Configured prompt layer.");
    let hooks = PluginHooks::default().with_system_prompt_layer("Plugin prompt layer.");
    let layers = PromptComposer::layers(
        ProviderFamily::Claude,
        ModelEffort::High,
        &test_environment(),
        &prompt,
        &hooks,
    );

    assert_eq!(
        layers.iter().map(|layer| layer.name).collect::<Vec<_>>(),
        vec!["base", "environment", "configured", "plugin", "effort"]
    );

    let composed = PromptComposer::compose(
        ProviderFamily::Claude,
        ModelEffort::High,
        &test_environment(),
        &prompt,
        &hooks,
    );
    assert!(composed.contains("Configured prompt layer."));
    assert!(composed.contains("Plugin prompt layer."));
}

#[test]
fn provider_request_preparer_applies_plugin_hooks() {
    let temp = TempDir::new("provider-request-hooks");
    let tools = ToolRuntime::new(temp.path()).unwrap();
    let mut state = SessionState::new(SessionId::new());
    state.push(Role::User, "use a plugin hook");
    let mut config = HarnessConfig::new("test-model");
    config.hooks = PluginHooks::default()
        .with_request_metadata("plugin", json!("example"))
        .with_advertised_tool("plugin_tool")
        .with_system_prompt_layer("Plugin hook instructions.");

    let prepared = ProviderRequestPreparer::prepare(ProviderRequestInput {
        session_id: state.session_id,
        round: 0,
        state: &state,
        turn_images: Vec::new(),
        config: &config,
        tools: &tools,
    })
    .unwrap();

    assert_eq!(prepared.request.metadata["plugin"], json!("example"));
    assert!(
        prepared
            .request
            .tool_names
            .contains(&"plugin_tool".to_string())
    );
    assert!(
        prepared
            .request
            .system_prompt
            .as_deref()
            .unwrap()
            .contains("Plugin hook instructions.")
    );
}

fn test_environment() -> SystemEnvironment {
    SystemEnvironment {
        model: "test-model".to_string(),
        cwd: PathBuf::from("/tmp/inductor-workspace"),
        workspace_root: PathBuf::from("/tmp/inductor-workspace"),
        memory_file: Some(PathBuf::from("/tmp/inductor-source/.inductor/memory.md")),
        is_git_repo: true,
        platform: "test-os",
        date_utc: "2026-06-12".to_string(),
    }
}

#[test]
fn multimodal_prompt_is_split_into_text_and_images() {
    let payload = json!({
        "text": "describe this screenshot",
        "images": [{
            "path": "screen.png",
            "mime_type": "image/png",
            "base64_data": "abc123",
            "width": 10,
            "height": 20,
            "file_size": 6
        }]
    });
    let parsed = parse_multimodal_prompt(&format!("__MULTIMODAL_MESSAGE__:{payload}"));

    assert_eq!(parsed.text, "describe this screenshot");
    assert_eq!(parsed.images.len(), 1);
    assert_eq!(parsed.images[0].mime_type, "image/png");
}

#[test]
fn plain_prompt_has_no_images() {
    let parsed = parse_multimodal_prompt("hello");

    assert_eq!(parsed.text, "hello");
    assert!(parsed.images.is_empty());
}

// --- Tool dispatch through the real ToolRuntime ----------------------------

#[test]
fn execute_tool_call_dispatches_read_file() {
    let temp = TempDir::new("dispatch-read");
    fs::write(temp.path().join("hello.txt"), "from disk").unwrap();
    let runtime = ToolRuntime::new(temp.path()).unwrap();

    let call = ParsedToolCall {
        name: "read_file".to_string(),
        input: json!({ "path": "hello.txt" }),
    };
    let result = execute_tool_call(&runtime, &call).unwrap();

    assert_eq!(result.output, "from disk");
}

#[test]
fn execute_tool_call_rejects_unknown_tool() {
    let temp = TempDir::new("dispatch-unknown");
    let runtime = ToolRuntime::new(temp.path()).unwrap();

    let call = ParsedToolCall {
        name: "delete_everything".to_string(),
        input: json!({}),
    };

    assert!(matches!(
        execute_tool_call(&runtime, &call),
        Err(ToolExecError::UnknownTool(_))
    ));
}

#[test]
fn execute_tool_call_requires_string_fields() {
    let temp = TempDir::new("dispatch-missing-field");
    let runtime = ToolRuntime::new(temp.path()).unwrap();

    let call = ParsedToolCall {
        name: "read_file".to_string(),
        input: json!({}),
    };

    assert!(matches!(
        execute_tool_call(&runtime, &call),
        Err(ToolExecError::MissingField { field: "path", .. })
    ));
}

#[test]
fn execute_tool_call_surfaces_workspace_escape() {
    let temp = TempDir::new("dispatch-escape");
    let runtime = ToolRuntime::new(temp.path()).unwrap();

    let call = ParsedToolCall {
        name: "read_file".to_string(),
        input: json!({ "path": "../secret.txt" }),
    };

    assert!(matches!(
        execute_tool_call(&runtime, &call),
        Err(ToolExecError::Runtime(_))
    ));
}

#[test]
fn execute_tool_call_dispatches_edit_file() {
    let temp = TempDir::new("dispatch-edit");
    fs::write(temp.path().join("hello.txt"), "hello world").unwrap();
    let runtime = ToolRuntime::new(temp.path()).unwrap();

    let call = ParsedToolCall {
        name: "edit_file".to_string(),
        input: json!({
            "path": "hello.txt",
            "old": "world",
            "new": "inductor"
        }),
    };
    let result = execute_tool_call(&runtime, &call).unwrap();

    assert_eq!(result.name.as_str(), "edit_file");
    assert_eq!(
        fs::read_to_string(temp.path().join("hello.txt")).unwrap(),
        "hello inductor"
    );
}

#[test]
fn execute_tool_call_dispatches_structured_patch() {
    let temp = TempDir::new("dispatch-structured-patch");
    fs::write(temp.path().join("hello.txt"), "hello world").unwrap();
    let runtime = ToolRuntime::new(temp.path()).unwrap();

    let call = ParsedToolCall {
        name: "apply_patch_structured".to_string(),
        input: json!({
            "operations": [
                {
                    "type": "edit",
                    "path": "hello.txt",
                    "old": "world",
                    "new": "inductor",
                    "expected_hash": null
                }
            ]
        }),
    };
    execute_tool_call(&runtime, &call).unwrap();

    assert_eq!(
        fs::read_to_string(temp.path().join("hello.txt")).unwrap(),
        "hello inductor"
    );
}

#[test]
fn execute_tool_call_dispatches_apply_patch() {
    let temp = TempDir::new("dispatch-apply-patch");
    let runtime = ToolRuntime::new(temp.path()).unwrap();

    let call = ParsedToolCall {
        name: "apply_patch".to_string(),
        input: json!({
            "patch": "*** Begin Patch\n*** Add File: hello.txt\n+hello inductor\n*** End Patch\n"
        }),
    };
    let result = execute_tool_call(&runtime, &call).unwrap();

    assert_eq!(result.name.as_str(), "apply_patch");
    assert_eq!(
        fs::read_to_string(temp.path().join("hello.txt")).unwrap(),
        "hello inductor\n"
    );
}

#[test]
fn execute_tool_call_dispatches_line_aware_apply_patch() {
    let temp = TempDir::new("dispatch-line-aware-apply-patch");
    fs::write(temp.path().join("hello.txt"), "same\nsame\nsame\n").unwrap();
    let runtime = ToolRuntime::new(temp.path()).unwrap();

    let call = ParsedToolCall {
        name: "apply_patch".to_string(),
        input: json!({
            "operations": [{
                "op": "update",
                "path": "hello.txt",
                "start_line": 2,
                "end_line": 2,
                "old": "same\n",
                "new": "changed\n"
            }]
        }),
    };
    let result = execute_tool_call(&runtime, &call).unwrap();

    assert_eq!(result.name.as_str(), "apply_patch");
    assert_eq!(
        fs::read_to_string(temp.path().join("hello.txt")).unwrap(),
        "same\nchanged\nsame\n"
    );
}

#[tokio::test]
async fn execute_tool_call_cancels_bash() {
    let temp = TempDir::new("dispatch-cancel-bash");
    let runtime = ToolRuntime::new(temp.path()).unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let call = ParsedToolCall {
        name: ToolName::Bash.as_str().to_string(),
        input: json!({ "command": "sleep 5" }),
    };

    let err = execute_tool_call_cancellable(&runtime, &call, cancel)
        .await
        .unwrap_err();

    assert!(matches!(
        err,
        ToolExecError::Runtime(tools::ToolError::CommandCancelled { .. })
    ));
}

// --- Full loop with a test-only stub provider ------------------------------

/// A test-only provider that replays scripted assistant turns.
///
/// This is NOT a runtime provider. It exists purely so the loop can be
/// exercised without live Claude/Codex auth. Each `stream_turn` call pops the
/// next scripted reply and emits it as one `TextDelta` followed by `Result`.
struct ScriptedProvider {
    replies: Mutex<std::collections::VecDeque<String>>,
}

impl ScriptedProvider {
    fn new(replies: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self {
            replies: Mutex::new(replies.into_iter().map(Into::into).collect()),
        }
    }
}

#[async_trait::async_trait]
impl ProviderPlugin for ScriptedProvider {
    fn id(&self) -> &'static str {
        "scripted-test-stub"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            token_counting: false,
            tool_calling: false,
        }
    }

    async fn list_models(&self, _auth: &ProviderAuth) -> anyhow::Result<Vec<ModelInfo>> {
        Ok(Vec::new())
    }

    async fn stream_turn(
        &self,
        _auth: &ProviderAuth,
        req: TurnRequest,
        _cancel: CancellationToken,
        _permissions: provider_core::PermissionResponses,
        _tool_responses: provider_core::ToolResponses,
        _question_responses: provider_core::QuestionResponses,
        _question_requests: provider_core::QuestionRequests,
    ) -> anyhow::Result<Pin<Box<dyn Stream<Item = anyhow::Result<SessionEvent>> + Send>>> {
        let reply = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| "no more scripted replies".to_string());
        let session_id = req.session_id;

        let stream = async_stream::try_stream! {
            yield SessionEvent::TextDelta { session_id, text: reply };
            yield SessionEvent::Result { session_id, stop_reason: StopReason::EndTurn };
        };

        Ok(Box::pin(stream))
    }
}

struct StartFailingProvider;

#[async_trait::async_trait]
impl ProviderPlugin for StartFailingProvider {
    fn id(&self) -> &'static str {
        "start-failing-test-stub"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            token_counting: false,
            tool_calling: false,
        }
    }

    async fn list_models(&self, _auth: &ProviderAuth) -> anyhow::Result<Vec<ModelInfo>> {
        Ok(Vec::new())
    }

    async fn stream_turn(
        &self,
        _auth: &ProviderAuth,
        _req: TurnRequest,
        _cancel: CancellationToken,
        _permissions: provider_core::PermissionResponses,
        _tool_responses: provider_core::ToolResponses,
        _question_responses: provider_core::QuestionResponses,
        _question_requests: provider_core::QuestionRequests,
    ) -> anyhow::Result<Pin<Box<dyn Stream<Item = anyhow::Result<SessionEvent>> + Send>>> {
        anyhow::bail!("start boom")
    }
}

struct StreamFailingProvider;

#[async_trait::async_trait]
impl ProviderPlugin for StreamFailingProvider {
    fn id(&self) -> &'static str {
        "stream-failing-test-stub"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            token_counting: false,
            tool_calling: false,
        }
    }

    async fn list_models(&self, _auth: &ProviderAuth) -> anyhow::Result<Vec<ModelInfo>> {
        Ok(Vec::new())
    }

    async fn stream_turn(
        &self,
        _auth: &ProviderAuth,
        req: TurnRequest,
        _cancel: CancellationToken,
        _permissions: provider_core::PermissionResponses,
        _tool_responses: provider_core::ToolResponses,
        _question_responses: provider_core::QuestionResponses,
        _question_requests: provider_core::QuestionRequests,
    ) -> anyhow::Result<Pin<Box<dyn Stream<Item = anyhow::Result<SessionEvent>> + Send>>> {
        let session_id = req.session_id;
        let stream = async_stream::try_stream! {
            yield SessionEvent::TextDelta {
                session_id,
                text: "partial answer".to_string(),
            };
            Err::<(), anyhow::Error>(anyhow::anyhow!("stream boom"))?;
        };
        Ok(Box::pin(stream))
    }
}

struct EndingWithoutResultProvider;

#[async_trait::async_trait]
impl ProviderPlugin for EndingWithoutResultProvider {
    fn id(&self) -> &'static str {
        "ending-without-result-test-stub"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            token_counting: false,
            tool_calling: false,
        }
    }

    async fn list_models(&self, _auth: &ProviderAuth) -> anyhow::Result<Vec<ModelInfo>> {
        Ok(Vec::new())
    }

    async fn stream_turn(
        &self,
        _auth: &ProviderAuth,
        req: TurnRequest,
        _cancel: CancellationToken,
        _permissions: provider_core::PermissionResponses,
        _tool_responses: provider_core::ToolResponses,
        _question_responses: provider_core::QuestionResponses,
        _question_requests: provider_core::QuestionRequests,
    ) -> anyhow::Result<Pin<Box<dyn Stream<Item = anyhow::Result<SessionEvent>> + Send>>> {
        let session_id = req.session_id;
        let stream = async_stream::try_stream! {
            yield SessionEvent::TextDelta {
                session_id,
                text: "partial answer".to_string(),
            };
        };
        Ok(Box::pin(stream))
    }
}

struct NativeCheckpointWaitProvider;

#[async_trait::async_trait]
impl ProviderPlugin for NativeCheckpointWaitProvider {
    fn id(&self) -> &'static str {
        "native-checkpoint-wait-test-stub"
    }

    fn capabilities(&self) -> ProviderCapabilities {
        ProviderCapabilities {
            streaming: true,
            token_counting: false,
            tool_calling: true,
        }
    }

    async fn list_models(&self, _auth: &ProviderAuth) -> anyhow::Result<Vec<ModelInfo>> {
        Ok(Vec::new())
    }

    async fn stream_turn(
        &self,
        _auth: &ProviderAuth,
        req: TurnRequest,
        _cancel: CancellationToken,
        _permissions: provider_core::PermissionResponses,
        mut tool_responses: provider_core::ToolResponses,
        _question_responses: provider_core::QuestionResponses,
        _question_requests: provider_core::QuestionRequests,
    ) -> anyhow::Result<Pin<Box<dyn Stream<Item = anyhow::Result<SessionEvent>> + Send>>> {
        let session_id = req.session_id;
        let stream = async_stream::try_stream! {
            let bash_call_id = ToolCallId::new();
            yield SessionEvent::ToolCallRequested {
                session_id,
                tool_call_id: bash_call_id,
                name: ToolName::Bash.as_str().to_string(),
                input_json: json!({
                    "command": "printf partial; sleep 1; printf done"
                }),
            };
            let checkpoint = tool_responses
                .recv()
                .await
                .ok_or_else(|| anyhow::anyhow!("missing checkpoint response"))?;
            assert_eq!(checkpoint.tool_call_id, bash_call_id);
            assert!(checkpoint.output.contains("command_id: bash-"));
            let command_id = checkpoint
                .output
                .split("command_id: ")
                .nth(1)
                .and_then(|rest| rest.split_whitespace().next())
                .map(|value| value.trim_end_matches('.').to_string())
                .ok_or_else(|| anyhow::anyhow!("missing command_id in checkpoint"))?;

            let wait_call_id = ToolCallId::new();
            yield SessionEvent::ToolCallRequested {
                session_id,
                tool_call_id: wait_call_id,
                name: ToolName::BashWait.as_str().to_string(),
                input_json: json!({
                    "command_id": command_id,
                    "timeout_secs": 2,
                }),
            };
            let final_output = tool_responses
                .recv()
                .await
                .ok_or_else(|| anyhow::anyhow!("missing wait response"))?;
            assert_eq!(final_output.tool_call_id, wait_call_id);
            assert!(!final_output.is_error);
            assert!(final_output.output.contains("Final output"));
            assert!(final_output.output.contains("partialdone"));
            yield SessionEvent::Result {
                session_id,
                stop_reason: StopReason::EndTurn,
            };
        };
        Ok(Box::pin(stream))
    }
}

fn test_auth() -> ProviderAuth {
    ProviderAuth::new(
        ProviderAuthKind::SessionToken,
        SecretString::from(String::new()),
    )
}

async fn collect_events(
    mut stream: Pin<Box<dyn Stream<Item = anyhow::Result<SessionEvent>> + Send + '_>>,
) -> Vec<SessionEvent> {
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event.unwrap());
    }
    events
}

#[tokio::test]
async fn loop_executes_tool_then_finishes() {
    let temp = TempDir::new("loop-read");
    fs::write(temp.path().join("hello.txt"), "file body").unwrap();
    let runtime = ToolRuntime::new(temp.path()).unwrap();

    let provider = ScriptedProvider::new([
        "Let me read it.\n<inductor_tool_call>{\"name\":\"read_file\",\"input\":{\"path\":\"hello.txt\"}}</inductor_tool_call>",
        "The file contains: file body",
    ]);
    let auth = test_auth();
    let mut state = SessionState::new(SessionId::new());

    let mut allow = AllowStore::new();
    let events = collect_events(run_turn(
        &provider,
        &auth,
        &runtime,
        &AutoApprove,
        &mut allow,
        &mut state,
        "read hello.txt".to_string(),
        HarnessConfig::new("test-model"),
        CancellationToken::new(),
        provider_core::empty_permission_responses(),
        provider_core::empty_question_responses(),
        provider_core::empty_question_requests(),
    ))
    .await;

    // The tool ran and returned the file contents.
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::ToolCallResult { output, .. } if output == "file body"
    )));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, SessionEvent::TextStart { .. }))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, SessionEvent::TextEnd { .. }))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::ToolInputEnd { input_json, .. } if input_json == &json!({ "path": "hello.txt" })
    )));
    // The loop ended on a normal final answer.
    assert!(matches!(
        events.last().unwrap(),
        SessionEvent::Result {
            stop_reason: StopReason::EndTurn,
            ..
        }
    ));
    // Transcript captured user, assistant, tool, assistant.
    assert_eq!(state.transcript.len(), 4);
}

#[tokio::test]
async fn loop_uses_cached_read_hash_instead_of_model_expected_hash() {
    let temp = TempDir::new("loop-cached-hash-edit");
    fs::write(temp.path().join("hello.txt"), "hello world").unwrap();
    let runtime = ToolRuntime::new(temp.path()).unwrap();

    let provider = ScriptedProvider::new([
        "Read first.\n<inductor_tool_call>{\"name\":\"read_file\",\"input\":{\"path\":\"hello.txt\"}}</inductor_tool_call>",
        "Now edit.\n<inductor_tool_call>{\"name\":\"edit_file\",\"input\":{\"path\":\"hello.txt\",\"old\":\"world\",\"new\":\"inductor\",\"expected_hash\":\"bogus\"}}</inductor_tool_call>",
        "Done.",
    ]);
    let auth = test_auth();
    let mut state = SessionState::new(SessionId::new());
    let mut allow = AllowStore::new();

    let events = collect_events(run_turn(
        &provider,
        &auth,
        &runtime,
        &AutoApprove,
        &mut allow,
        &mut state,
        "edit hello.txt".to_string(),
        HarnessConfig::new("test-model"),
        CancellationToken::new(),
        provider_core::empty_permission_responses(),
        provider_core::empty_question_responses(),
        provider_core::empty_question_requests(),
    ))
    .await;

    assert_eq!(
        fs::read_to_string(temp.path().join("hello.txt")).unwrap(),
        "hello inductor"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::ToolCallResult { output, .. } if output.contains("applied 1 edit")
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::ToolCallStart { name, input_json, .. }
            if name == "edit_file"
                && input_json.get("expected_hash").is_none()
                && input_json["path"] == "hello.txt"
    )));
    assert!(!events.iter().any(|event| matches!(
        event,
        SessionEvent::ToolCallError { message, .. } if message.contains("stale edit")
    )));
}

#[tokio::test]
async fn loop_requires_fresh_read_before_line_patch_after_same_file_write() {
    let temp = TempDir::new("loop-line-patch-fresh-read-required");
    fs::write(temp.path().join("hello.txt"), "one\ntwo\nthree\n").unwrap();
    let runtime = ToolRuntime::new(temp.path()).unwrap();

    let provider = ScriptedProvider::new([
        "<inductor_tool_call>{\"name\":\"read_file\",\"input\":{\"path\":\"hello.txt\",\"start_line\":1,\"end_line\":3}}</inductor_tool_call>",
        "<inductor_tool_call>{\"name\":\"apply_patch\",\"input\":{\"operations\":[{\"op\":\"update\",\"path\":\"hello.txt\",\"start_line\":2,\"end_line\":2,\"old\":\"two\\n\",\"new\":\"TWO\\n\"}]}}</inductor_tool_call>",
        "<inductor_tool_call>{\"name\":\"apply_patch\",\"input\":{\"operations\":[{\"op\":\"update\",\"path\":\"hello.txt\",\"start_line\":3,\"end_line\":3,\"old\":\"three\\n\",\"new\":\"THREE\\n\"}]}}</inductor_tool_call>",
        "Done.",
    ]);
    let auth = test_auth();
    let mut state = SessionState::new(SessionId::new());
    let mut allow = AllowStore::new();

    let events = collect_events(run_turn(
        &provider,
        &auth,
        &runtime,
        &AutoApprove,
        &mut allow,
        &mut state,
        "edit hello.txt".to_string(),
        HarnessConfig::new("test-model"),
        CancellationToken::new(),
        provider_core::empty_permission_responses(),
        provider_core::empty_question_responses(),
        provider_core::empty_question_requests(),
    ))
    .await;

    assert_eq!(
        fs::read_to_string(temp.path().join("hello.txt")).unwrap(),
        "one\nTWO\nthree\n"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::ToolCallError { message, .. }
            if message.contains("requires a fresh read_file")
                && message.contains("hello.txt")
    )));
}

#[tokio::test]
async fn loop_allows_line_patch_after_fresh_read_following_write() {
    let temp = TempDir::new("loop-line-patch-fresh-read-allows");
    fs::write(temp.path().join("hello.txt"), "one\ntwo\nthree\n").unwrap();
    let runtime = ToolRuntime::new(temp.path()).unwrap();

    let provider = ScriptedProvider::new([
        "<inductor_tool_call>{\"name\":\"read_file\",\"input\":{\"path\":\"hello.txt\",\"start_line\":1,\"end_line\":3}}</inductor_tool_call>",
        "<inductor_tool_call>{\"name\":\"apply_patch\",\"input\":{\"operations\":[{\"op\":\"update\",\"path\":\"hello.txt\",\"start_line\":2,\"end_line\":2,\"old\":\"two\\n\",\"new\":\"TWO\\n\"}]}}</inductor_tool_call>",
        "<inductor_tool_call>{\"name\":\"read_file\",\"input\":{\"path\":\"hello.txt\",\"start_line\":3,\"end_line\":3}}</inductor_tool_call>",
        "<inductor_tool_call>{\"name\":\"apply_patch\",\"input\":{\"operations\":[{\"op\":\"update\",\"path\":\"hello.txt\",\"start_line\":3,\"end_line\":3,\"old\":\"three\\n\",\"new\":\"THREE\\n\"}]}}</inductor_tool_call>",
        "Done.",
    ]);
    let auth = test_auth();
    let mut state = SessionState::new(SessionId::new());
    let mut allow = AllowStore::new();

    let events = collect_events(run_turn(
        &provider,
        &auth,
        &runtime,
        &AutoApprove,
        &mut allow,
        &mut state,
        "edit hello.txt".to_string(),
        HarnessConfig::new("test-model"),
        CancellationToken::new(),
        provider_core::empty_permission_responses(),
        provider_core::empty_question_responses(),
        provider_core::empty_question_requests(),
    ))
    .await;

    assert_eq!(
        fs::read_to_string(temp.path().join("hello.txt")).unwrap(),
        "one\nTWO\nTHREE\n"
    );
    assert!(!events.iter().any(|event| matches!(
        event,
        SessionEvent::ToolCallError { message, .. }
            if message.contains("requires a fresh read_file")
    )));
}

#[tokio::test]
async fn provider_start_error_becomes_terminal_error_result() {
    let temp = TempDir::new("provider-start-error");
    let runtime = ToolRuntime::new(temp.path()).unwrap();
    let provider = StartFailingProvider;
    let auth = test_auth();
    let mut state = SessionState::new(SessionId::new());
    let mut allow = AllowStore::new();

    let events = collect_events(run_turn(
        &provider,
        &auth,
        &runtime,
        &AutoApprove,
        &mut allow,
        &mut state,
        "hello".to_string(),
        HarnessConfig::new("test-model"),
        CancellationToken::new(),
        provider_core::empty_permission_responses(),
        provider_core::empty_question_responses(),
        provider_core::empty_question_requests(),
    ))
    .await;

    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::Error { message, .. } if message.contains("provider failed to start turn")
            && message.contains("start boom")
    )));
    assert!(matches!(
        events.last().unwrap(),
        SessionEvent::Result {
            stop_reason: StopReason::Error,
            ..
        }
    ));
}

#[tokio::test]
async fn provider_stream_error_becomes_terminal_error_result() {
    let temp = TempDir::new("provider-stream-error");
    let runtime = ToolRuntime::new(temp.path()).unwrap();
    let provider = StreamFailingProvider;
    let auth = test_auth();
    let mut state = SessionState::new(SessionId::new());
    let mut allow = AllowStore::new();

    let events = collect_events(run_turn(
        &provider,
        &auth,
        &runtime,
        &AutoApprove,
        &mut allow,
        &mut state,
        "hello".to_string(),
        HarnessConfig::new("test-model"),
        CancellationToken::new(),
        provider_core::empty_permission_responses(),
        provider_core::empty_question_responses(),
        provider_core::empty_question_requests(),
    ))
    .await;

    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::TextEnd { text, .. } if text == "partial answer"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::Error { message, .. } if message.contains("provider stream failed")
            && message.contains("stream boom")
    )));
    assert!(matches!(
        events.last().unwrap(),
        SessionEvent::Result {
            stop_reason: StopReason::Error,
            ..
        }
    ));
}

#[tokio::test]
async fn provider_eof_without_result_becomes_terminal_error_result() {
    let temp = TempDir::new("provider-eof-error");
    let runtime = ToolRuntime::new(temp.path()).unwrap();
    let provider = EndingWithoutResultProvider;
    let auth = test_auth();
    let mut state = SessionState::new(SessionId::new());
    let mut allow = AllowStore::new();

    let events = collect_events(run_turn(
        &provider,
        &auth,
        &runtime,
        &AutoApprove,
        &mut allow,
        &mut state,
        "hello".to_string(),
        HarnessConfig::new("test-model"),
        CancellationToken::new(),
        provider_core::empty_permission_responses(),
        provider_core::empty_question_responses(),
        provider_core::empty_question_requests(),
    ))
    .await;

    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::TextEnd { text, .. } if text == "partial answer"
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::Error { message, .. } if message.contains("ended without a terminal result")
    )));
    assert!(matches!(
        events.last().unwrap(),
        SessionEvent::Result {
            stop_reason: StopReason::Error,
            ..
        }
    ));
}

#[tokio::test]
async fn long_running_bash_emits_progress_before_result() {
    let temp = TempDir::new("loop-progress");
    let runtime = ToolRuntime::new(temp.path()).unwrap();

    let provider = ScriptedProvider::new([
        "<inductor_tool_call>{\"name\":\"bash\",\"input\":{\"command\":\"sleep 6; echo done\"}}</inductor_tool_call>",
        "done",
    ]);
    let auth = test_auth();
    let mut state = SessionState::new(SessionId::new());
    let mut config = HarnessConfig::new("test-model");
    config.approval_policy = ApprovalPolicy::Never;

    let mut allow = AllowStore::new();
    let events = collect_events(run_turn(
        &provider,
        &auth,
        &runtime,
        &AutoApprove,
        &mut allow,
        &mut state,
        "run a slow command".to_string(),
        config,
        CancellationToken::new(),
        provider_core::empty_permission_responses(),
        provider_core::empty_question_responses(),
        provider_core::empty_question_requests(),
    ))
    .await;

    let start_index = events
        .iter()
        .position(
            |event| matches!(event, SessionEvent::ToolCallStart { name, .. } if name == "bash"),
        )
        .expect("tool start should be visible before completion");
    let progress_index = events
        .iter()
        .position(|event| matches!(event, SessionEvent::ToolCallProgress { message, .. } if message.contains("still running for")))
        .expect("long-running tool should emit stopwatch progress");
    let result_index = events
        .iter()
        .position(|event| matches!(event, SessionEvent::ToolCallResult { output, .. } if output.contains("done")))
        .expect("tool should still finish normally");

    assert!(start_index < progress_index);
    assert!(progress_index < result_index);
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::Status {
            status: SessionStatus::RunningTools,
            ..
        }
    )));
}

#[tokio::test]
async fn bash_checkpoint_returns_partial_output_to_model() {
    let temp = TempDir::new("loop-checkpoint");
    let runtime = ToolRuntime::new(temp.path()).unwrap();

    let provider = ScriptedProvider::new([
        "<inductor_tool_call>{\"name\":\"bash\",\"input\":{\"command\":\"printf partial-output; sleep 5; printf late-output\"}}</inductor_tool_call>",
        "I will stop here.",
    ]);
    let auth = test_auth();
    let mut state = SessionState::new(SessionId::new());
    let mut config = HarnessConfig::new("test-model");
    config.approval_policy = ApprovalPolicy::Never;
    config.tool_model_checkpoint_after = std::time::Duration::from_secs(1);

    let mut allow = AllowStore::new();
    let events = collect_events(run_turn(
        &provider,
        &auth,
        &runtime,
        &AutoApprove,
        &mut allow,
        &mut state,
        "run a command that may hang".to_string(),
        config,
        CancellationToken::new(),
        provider_core::empty_permission_responses(),
        provider_core::empty_question_responses(),
        provider_core::empty_question_requests(),
    ))
    .await;

    let message = events
        .iter()
        .find_map(|event| match event {
            SessionEvent::ToolCallError { message, .. } => Some(message),
            _ => None,
        })
        .expect("checkpoint should be surfaced as a model-visible tool error");

    assert!(message.contains("reached the tool checkpoint"));
    assert!(message.contains("command_id: bash-"));
    assert!(message.contains("The command is still running in the background"));
    assert!(message.contains("bash_wait"));
    assert!(message.contains("bash_kill"));
    assert!(message.contains("Partial output captured before the checkpoint"));
    assert!(message.contains("partial-output"));
    assert!(matches!(
        events.last().unwrap(),
        SessionEvent::Result {
            stop_reason: StopReason::EndTurn,
            ..
        }
    ));
}

#[tokio::test]
async fn native_provider_can_wait_for_checkpointed_bash_final_output() {
    let temp = TempDir::new("loop-checkpoint-wait");
    let runtime = ToolRuntime::new(temp.path()).unwrap();
    let provider = NativeCheckpointWaitProvider;
    let auth = test_auth();
    let mut state = SessionState::new(SessionId::new());
    let mut config = HarnessConfig::new("test-model");
    config.approval_policy = ApprovalPolicy::Never;
    config.tool_model_checkpoint_after = std::time::Duration::from_millis(100);

    let mut allow = AllowStore::new();
    let events = collect_events(run_turn(
        &provider,
        &auth,
        &runtime,
        &AutoApprove,
        &mut allow,
        &mut state,
        "run a command and wait for it".to_string(),
        config,
        CancellationToken::new(),
        provider_core::empty_permission_responses(),
        provider_core::empty_question_responses(),
        provider_core::empty_question_requests(),
    ))
    .await;

    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::ToolCallResult {
            output,
            exit_code: Some(0),
            ..
        } if output.contains("Final output") && output.contains("partialdone")
    )));
    assert!(matches!(
        events.last().unwrap(),
        SessionEvent::Result {
            stop_reason: StopReason::EndTurn,
            ..
        }
    ));
}

#[tokio::test]
async fn loop_surfaces_tool_error_and_continues() {
    let temp = TempDir::new("loop-escape");
    let runtime = ToolRuntime::new(temp.path()).unwrap();

    let provider = ScriptedProvider::new([
        "<inductor_tool_call>{\"name\":\"read_file\",\"input\":{\"path\":\"../secret.txt\"}}</inductor_tool_call>",
        "Sorry, that path is outside the workspace.",
    ]);
    let auth = test_auth();
    let mut state = SessionState::new(SessionId::new());

    let mut allow = AllowStore::new();
    let events = collect_events(run_turn(
        &provider,
        &auth,
        &runtime,
        &AutoDeny,
        &mut allow,
        &mut state,
        "read ../secret.txt".to_string(),
        HarnessConfig::new("test-model"),
        CancellationToken::new(),
        provider_core::empty_permission_responses(),
        provider_core::empty_question_responses(),
        provider_core::empty_question_requests(),
    ))
    .await;

    // Outside-workspace access is now flagged as a risk and asks for permission.
    // AutoDeny rejects it, so we get a ToolCallError.
    assert!(
        events
            .iter()
            .any(|event| matches!(event, SessionEvent::PermissionRequest { .. }))
    );
    assert!(
        events
            .iter()
            .any(|event| matches!(event, SessionEvent::ToolCallError { .. }))
    );
    assert!(matches!(
        events.last().unwrap(),
        SessionEvent::Result {
            stop_reason: StopReason::EndTurn,
            ..
        }
    ));
}

#[tokio::test]
async fn loop_does_not_stop_at_configured_tool_round_limit() {
    let temp = TempDir::new("loop-maxrounds");
    fs::write(temp.path().join("loop.txt"), "x").unwrap();
    let runtime = ToolRuntime::new(temp.path()).unwrap();

    let repeated = "<inductor_tool_call>{\"name\":\"read_file\",\"input\":{\"path\":\"loop.txt\"}}</inductor_tool_call>";
    let provider = ScriptedProvider::new([repeated, repeated, "done"]);

    let auth = test_auth();
    let mut state = SessionState::new(SessionId::new());
    let mut config = HarnessConfig::new("test-model");
    config.max_tool_rounds = 1;

    let mut allow = AllowStore::new();
    let events = collect_events(run_turn(
        &provider,
        &auth,
        &runtime,
        &AutoApprove,
        &mut allow,
        &mut state,
        "loop forever".to_string(),
        config,
        CancellationToken::new(),
        provider_core::empty_permission_responses(),
        provider_core::empty_question_responses(),
        provider_core::empty_question_requests(),
    ))
    .await;

    assert!(matches!(
        events.last().unwrap(),
        SessionEvent::Result {
            stop_reason: StopReason::EndTurn,
            ..
        }
    ));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, SessionEvent::ToolCallResult { .. }))
            .count(),
        2
    );
}

#[tokio::test]
async fn loop_allows_repeated_identical_tool_calls() {
    let temp = TempDir::new("loop-repeat");
    fs::write(temp.path().join("hello.txt"), "file body").unwrap();
    let runtime = ToolRuntime::new(temp.path()).unwrap();

    let repeated = "<inductor_tool_call>{\"name\":\"read_file\",\"input\":{\"path\":\"hello.txt\"}}</inductor_tool_call>";
    let provider = ScriptedProvider::new([repeated, repeated, repeated, repeated, "done"]);
    let auth = test_auth();
    let mut state = SessionState::new(SessionId::new());
    let mut allow = AllowStore::new();

    let events = collect_events(run_turn(
        &provider,
        &auth,
        &runtime,
        &AutoApprove,
        &mut allow,
        &mut state,
        "repeat".to_string(),
        HarnessConfig::new("test-model"),
        CancellationToken::new(),
        provider_core::empty_permission_responses(),
        provider_core::empty_question_responses(),
        provider_core::empty_question_requests(),
    ))
    .await;

    assert!(matches!(
        events.last().unwrap(),
        SessionEvent::Result {
            stop_reason: StopReason::EndTurn,
            ..
        }
    ));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, SessionEvent::ToolCallResult { .. }))
            .count(),
        4
    );
    assert!(!events.iter().any(|event| matches!(
        event,
        SessionEvent::Error { message, .. } if message.contains("repeated identical tool call")
    )));
}

#[tokio::test]
async fn loop_stubs_large_tool_output_and_writes_blob() {
    let temp = TempDir::new("loop-blob");
    fs::write(temp.path().join("large.txt"), "x".repeat(512)).unwrap();
    let runtime = ToolRuntime::new(temp.path()).unwrap();

    let provider = ScriptedProvider::new([
        "<inductor_tool_call>{\"name\":\"read_file\",\"input\":{\"path\":\"large.txt\"}}</inductor_tool_call>",
        "done",
    ]);
    let auth = test_auth();
    let mut state = SessionState::new(SessionId::new());
    let mut config = HarnessConfig::new("test-model");
    config.context.limits.tool_result_inline_bytes = 32;
    config.context.blob_root = Some(temp.path().join("blobs"));

    let mut allow = AllowStore::new();
    let events = collect_events(run_turn(
        &provider,
        &auth,
        &runtime,
        &AutoApprove,
        &mut allow,
        &mut state,
        "read large.txt".to_string(),
        config,
        CancellationToken::new(),
        provider_core::empty_permission_responses(),
        provider_core::empty_question_responses(),
        provider_core::empty_question_requests(),
    ))
    .await;

    let output = events
        .iter()
        .find_map(|event| match event {
            SessionEvent::ToolCallResult { output, .. } => Some(output),
            _ => None,
        })
        .unwrap();

    assert!(output.contains("Inductor truncated"));
    assert!(
        temp.path()
            .join("blobs")
            .read_dir()
            .unwrap()
            .next()
            .is_some()
    );
}

#[test]
fn read_blob_tool_returns_bounded_stored_output_slice() {
    let temp = TempDir::new("read-blob-tool");
    let blob_root = temp.path().join("blobs");
    let store = BlobStore::new(&blob_root);
    let blob = store
        .store(b"0123456789abcdefghijklmnopqrstuvwxyz")
        .unwrap();
    let mut config = HarnessConfig::new("test-model");
    config.context.blob_root = Some(blob_root);

    let result = read_blob_tool_result(
        &config,
        &json!({
            "blob_id": blob.id,
            "start_byte": 10,
            "limit_bytes": 5,
        }),
    )
    .unwrap();

    assert_eq!(result.name, ToolName::ReadBlob);
    assert!(result.output.contains("bytes 10..15"));
    assert!(result.output.ends_with("abcde"));
    assert_eq!(result.metadata["start_byte"], 10);
    assert_eq!(result.metadata["end_byte"], 15);
    assert_eq!(result.metadata["truncated"], true);
}

// --- Phase 6: approval gate ------------------------------------------------

/// Records every approval request it sees, then returns a fixed decision.
struct RecordingApprover {
    decision: PermissionDecision,
    seen: Mutex<Vec<ApprovalRequest>>,
}

impl RecordingApprover {
    fn new(decision: PermissionDecision) -> Self {
        Self {
            decision,
            seen: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait::async_trait]
impl Approver for RecordingApprover {
    async fn decide(&self, request: &ApprovalRequest) -> PermissionDecision {
        self.seen.lock().unwrap().push(request.clone());
        self.decision
    }
}

#[tokio::test]
async fn risky_command_pauses_for_approval_and_denial_blocks_it() {
    let temp = TempDir::new("approval-deny");
    let runtime = ToolRuntime::new(temp.path()).unwrap();

    // Model asks to run a risky command, then gives up.
    let provider = ScriptedProvider::new([
        "<inductor_tool_call>{\"name\":\"bash\",\"input\":{\"command\":\"rm -rf build\"}}</inductor_tool_call>",
        "Understood, I won't remove it.",
    ]);
    let auth = test_auth();
    let approver = RecordingApprover::new(PermissionDecision::Deny);
    let mut allow = AllowStore::new();
    let mut state = SessionState::new(SessionId::new());

    let events = collect_events(run_turn(
        &provider,
        &auth,
        &runtime,
        &approver,
        &mut allow,
        &mut state,
        "clean the build dir".to_string(),
        HarnessConfig::new("test-model"), // default OnRequest
        CancellationToken::new(),
        provider_core::empty_permission_responses(),
        provider_core::empty_question_responses(),
        provider_core::empty_question_requests(),
    ))
    .await;

    // A permission request was emitted and the approver saw the risk flags.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::PermissionRequest { .. }))
    );
    let seen = approver.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].risk_flags.contains(&RiskFlag::RecursiveRemove));

    // Denied: a tool error, and the command never produced a result.
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::ToolCallError { .. }))
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, SessionEvent::ToolCallResult { .. }))
    );
}

#[tokio::test]
async fn approved_outside_read_file_executes() {
    let workspace = TempDir::new("approval-outside-workspace");
    let outside = TempDir::new("approval-outside-target");
    let outside_file = outside.path().join("memory.md");
    fs::write(&outside_file, "outside memory").unwrap();
    let runtime = ToolRuntime::new(workspace.path()).unwrap();

    let provider = ScriptedProvider::new([
        format!(
            "<inductor_tool_call>{{\"name\":\"read_file\",\"input\":{{\"path\":{}}}}}</inductor_tool_call>",
            serde_json::to_string(&outside_file.display().to_string()).unwrap()
        ),
        "read it".to_string(),
    ]);
    let auth = test_auth();
    let approver = RecordingApprover::new(PermissionDecision::Allow);
    let mut allow = AllowStore::new();
    let mut state = SessionState::new(SessionId::new());

    let events = collect_events(run_turn(
        &provider,
        &auth,
        &runtime,
        &approver,
        &mut allow,
        &mut state,
        "read outside memory".to_string(),
        HarnessConfig::new("test-model"),
        CancellationToken::new(),
        provider_core::empty_permission_responses(),
        provider_core::empty_question_responses(),
        provider_core::empty_question_requests(),
    ))
    .await;

    assert!(
        events
            .iter()
            .any(|event| matches!(event, SessionEvent::PermissionRequest { .. }))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::ToolCallResult { output, .. } if output.contains("outside memory")
    )));
}

#[tokio::test]
async fn never_policy_runs_outside_read_without_prompt() {
    let workspace = TempDir::new("never-outside-workspace");
    let outside = TempDir::new("never-outside-target");
    let outside_file = outside.path().join("memory.md");
    fs::write(&outside_file, "outside memory").unwrap();
    let runtime = ToolRuntime::unrestricted(workspace.path()).unwrap();

    let provider = ScriptedProvider::new([
        format!(
            "<inductor_tool_call>{{\"name\":\"read_file\",\"input\":{{\"path\":{}}}}}</inductor_tool_call>",
            serde_json::to_string(&outside_file.display().to_string()).unwrap()
        ),
        "read it".to_string(),
    ]);
    let auth = test_auth();
    let approver = RecordingApprover::new(PermissionDecision::Deny);
    let mut allow = AllowStore::new();
    let mut state = SessionState::new(SessionId::new());
    let mut config = HarnessConfig::new("test-model");
    config.approval_policy = ApprovalPolicy::Never;

    let events = collect_events(run_turn(
        &provider,
        &auth,
        &runtime,
        &approver,
        &mut allow,
        &mut state,
        "read outside memory".to_string(),
        config,
        CancellationToken::new(),
        provider_core::empty_permission_responses(),
        provider_core::empty_question_responses(),
        provider_core::empty_question_requests(),
    ))
    .await;

    assert!(
        !events
            .iter()
            .any(|event| matches!(event, SessionEvent::PermissionRequest { .. }))
    );
    assert!(events.iter().any(|event| matches!(
        event,
        SessionEvent::ToolCallResult { output, .. } if output.contains("outside memory")
    )));
}

#[tokio::test]
async fn benign_command_does_not_pause_under_on_request() {
    let temp = TempDir::new("approval-benign");
    let runtime = ToolRuntime::new(temp.path()).unwrap();

    let provider = ScriptedProvider::new([
        "<inductor_tool_call>{\"name\":\"bash\",\"input\":{\"command\":\"echo hi\"}}</inductor_tool_call>",
        "done",
    ]);
    let auth = test_auth();
    let approver = RecordingApprover::new(PermissionDecision::Deny);
    let mut allow = AllowStore::new();
    let mut state = SessionState::new(SessionId::new());

    let events = collect_events(run_turn(
        &provider,
        &auth,
        &runtime,
        &approver,
        &mut allow,
        &mut state,
        "say hi".to_string(),
        HarnessConfig::new("test-model"),
        CancellationToken::new(),
        provider_core::empty_permission_responses(),
        provider_core::empty_question_responses(),
        provider_core::empty_question_requests(),
    ))
    .await;

    // No approval needed for a benign command; it ran to a result.
    assert!(approver.seen.lock().unwrap().is_empty());
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::ToolCallResult { .. }))
    );
}

#[tokio::test]
async fn permission_request_includes_write_diff_preview() {
    let temp = TempDir::new("permission-diff-preview");
    fs::write(temp.path().join("note.txt"), "old\n").unwrap();
    let runtime = ToolRuntime::new(temp.path()).unwrap();

    let provider = ScriptedProvider::new([
        "<inductor_tool_call>{\"name\":\"write_file\",\"input\":{\"path\":\"note.txt\",\"content\":\"new\\n\"}}</inductor_tool_call>",
        "done",
    ]);
    let auth = test_auth();
    let approver = RecordingApprover::new(PermissionDecision::Deny);
    let mut allow = AllowStore::new();
    let mut state = SessionState::new(SessionId::new());
    let mut config = HarnessConfig::new("test-model");
    config.approval_policy = ApprovalPolicy::Mutating;

    let events = collect_events(run_turn(
        &provider,
        &auth,
        &runtime,
        &approver,
        &mut allow,
        &mut state,
        "replace note".to_string(),
        config,
        CancellationToken::new(),
        provider_core::empty_permission_responses(),
        provider_core::empty_question_responses(),
        provider_core::empty_question_requests(),
    ))
    .await;

    let preview = events
        .iter()
        .find_map(|event| match event {
            SessionEvent::PermissionRequest { input_json, .. } => Some(input_json),
            _ => None,
        })
        .expect("permission request should be emitted");

    assert_eq!(preview["filepath"], "note.txt");
    assert!(preview["diff"].as_str().unwrap().contains("-old"));
    assert!(preview["diff"].as_str().unwrap().contains("+new"));
}

#[tokio::test]
async fn edit_file_emits_patch_event_after_execution() {
    let temp = TempDir::new("edit-patch-event");
    fs::write(temp.path().join("note.txt"), "old\n").unwrap();
    let runtime = ToolRuntime::new(temp.path()).unwrap();

    let provider = ScriptedProvider::new([
        "<inductor_tool_call>{\"name\":\"edit_file\",\"input\":{\"path\":\"note.txt\",\"old\":\"old\",\"new\":\"new\"}}</inductor_tool_call>",
        "done",
    ]);
    let auth = test_auth();
    let mut allow = AllowStore::new();
    let mut state = SessionState::new(SessionId::new());

    let events = collect_events(run_turn(
        &provider,
        &auth,
        &runtime,
        &AutoApprove,
        &mut allow,
        &mut state,
        "edit note".to_string(),
        HarnessConfig::new("test-model"),
        CancellationToken::new(),
        provider_core::empty_permission_responses(),
        provider_core::empty_question_responses(),
        provider_core::empty_question_requests(),
    ))
    .await;

    let patch = events
        .iter()
        .find_map(|event| match event {
            SessionEvent::Patch { files, .. } => files.first(),
            _ => None,
        })
        .expect("patch event should be emitted");

    assert_eq!(patch.path, "note.txt");
    assert!(patch.diff.as_deref().unwrap().contains("-old"));
    assert!(patch.diff.as_deref().unwrap().contains("+new"));

    let diagnostics = events
        .iter()
        .find_map(|event| match event {
            SessionEvent::Diagnostics { files, .. } => files.first(),
            _ => None,
        })
        .expect("diagnostics event should be emitted");

    assert_eq!(diagnostics.path, "note.txt");
    assert!(diagnostics.exists);
    assert_eq!(diagnostics.lines, Some(1));
}

#[test]
fn unified_line_diff_keeps_unchanged_lines_as_context() {
    let diff = super::unified_line_diff(
        "src/main.rs",
        "fn main() {\n    println!(\"old\");\n}\n",
        "fn main() {\n    println!(\"old\");\n    println!(\"new\");\n}\n",
    );

    assert_eq!(diff.additions, 1);
    assert_eq!(diff.deletions, 0);
    assert!(diff.text.contains(" fn main() {"));
    assert!(diff.text.contains("     println!(\"old\");"));
    assert!(diff.text.contains("+    println!(\"new\");"));
    assert!(!diff.text.contains("-fn main() {"));
    assert!(!diff.text.contains("+fn main() {"));
}

#[tokio::test]
async fn always_policy_pauses_even_for_benign_calls() {
    let temp = TempDir::new("approval-always");
    fs::write(temp.path().join("a.txt"), "x").unwrap();
    let runtime = ToolRuntime::new(temp.path()).unwrap();

    let provider = ScriptedProvider::new([
        "<inductor_tool_call>{\"name\":\"read_file\",\"input\":{\"path\":\"a.txt\"}}</inductor_tool_call>",
        "ok",
    ]);
    let auth = test_auth();
    let approver = RecordingApprover::new(PermissionDecision::Allow);
    let mut allow = AllowStore::new();
    let mut state = SessionState::new(SessionId::new());
    let mut config = HarnessConfig::new("test-model");
    config.approval_policy = ApprovalPolicy::Always;

    let events = collect_events(run_turn(
        &provider,
        &auth,
        &runtime,
        &approver,
        &mut allow,
        &mut state,
        "read a.txt".to_string(),
        config,
        CancellationToken::new(),
        provider_core::empty_permission_responses(),
        provider_core::empty_question_responses(),
        provider_core::empty_question_requests(),
    ))
    .await;

    // Even a benign read paused, but approval let it run.
    assert_eq!(approver.seen.lock().unwrap().len(), 1);
    assert!(
        events
            .iter()
            .any(|e| matches!(e, SessionEvent::ToolCallResult { .. }))
    );
}

#[tokio::test]
async fn allow_always_skips_future_prompts_for_same_program() {
    let temp = TempDir::new("approval-allowalways");
    let runtime = ToolRuntime::new(temp.path()).unwrap();

    // Two risky-looking sudo calls; only the first should prompt.
    let provider = ScriptedProvider::new([
        "<inductor_tool_call>{\"name\":\"bash\",\"input\":{\"command\":\"sudo echo one\"}}</inductor_tool_call>",
        "<inductor_tool_call>{\"name\":\"bash\",\"input\":{\"command\":\"sudo echo two\"}}</inductor_tool_call>",
        "all done",
    ]);
    let auth = test_auth();
    let approver = RecordingApprover::new(PermissionDecision::AllowAlways);
    let mut allow = AllowStore::new();
    let mut state = SessionState::new(SessionId::new());

    let _ = collect_events(run_turn(
        &provider,
        &auth,
        &runtime,
        &approver,
        &mut allow,
        &mut state,
        "run sudo twice".to_string(),
        HarnessConfig::new("test-model"),
        CancellationToken::new(),
        provider_core::empty_permission_responses(),
        provider_core::empty_question_responses(),
        provider_core::empty_question_requests(),
    ))
    .await;

    // Only the first sudo prompted; "allow always" recorded a bash-prefix rule.
    assert_eq!(approver.seen.lock().unwrap().len(), 1);
    assert!(allow.rules().iter().any(|r| r.value == "sudo"));
}

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("inductor-harness-{label}-{nanos}"));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
