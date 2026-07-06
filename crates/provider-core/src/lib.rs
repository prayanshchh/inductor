use futures_core::Stream;
use harness_core::{
    AgentQuestion, ModelInfo, PermissionResponse, ProviderCapabilities, QuestionAnswer,
    QuestionResponse, SessionEvent, ToolCallId, TurnRequest,
};
use secrecy::{ExposeSecret, SecretString};
use std::{fmt, pin::Pin};
use tokio_util::sync::CancellationToken;

/// Channel a provider reads to receive the user's answers to the
/// [`SessionEvent::PermissionRequest`]s it emits. Providers that never pause for
/// permission (e.g. Codex today) simply ignore it.
pub type PermissionResponses = tokio::sync::mpsc::UnboundedReceiver<PermissionResponse>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderToolResponse {
    pub tool_call_id: ToolCallId,
    pub output: String,
    pub is_error: bool,
}

/// Channel a provider reads to receive the user's answers to question prompts.
pub type QuestionResponses = tokio::sync::mpsc::UnboundedReceiver<QuestionResponse>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderQuestionRequest {
    pub tool_call_id: ToolCallId,
    pub questions: Vec<AgentQuestion>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderQuestionResult {
    pub answers: Vec<QuestionAnswer>,
    pub output: String,
}

/// Channel a provider writes to ask the host UI for user answers to questions.
pub type QuestionRequests = tokio::sync::mpsc::UnboundedSender<ProviderQuestionRequest>;

/// Channel a provider reads to receive local tool results from the harness.
/// Providers that do not keep an in-flight native tool call open ignore it.
pub type ToolResponses = tokio::sync::mpsc::UnboundedReceiver<ProviderToolResponse>;

/// An already-closed permission channel, for provider turns that never pause for
/// permission (the sender is dropped immediately).
pub fn empty_permission_responses() -> PermissionResponses {
    tokio::sync::mpsc::unbounded_channel().1
}

pub fn empty_tool_responses() -> ToolResponses {
    tokio::sync::mpsc::unbounded_channel().1
}

pub fn empty_question_responses() -> QuestionResponses {
    tokio::sync::mpsc::unbounded_channel().1
}

pub fn empty_question_requests() -> QuestionRequests {
    tokio::sync::mpsc::unbounded_channel().0
}

pub async fn ask_questions(
    requests: &QuestionRequests,
    responses: &mut QuestionResponses,
    tool_call_id: ToolCallId,
    questions: Vec<AgentQuestion>,
) -> ProviderQuestionResult {
    let _ = requests.send(ProviderQuestionRequest {
        tool_call_id,
        questions,
    });
    while let Some(response) = responses.recv().await {
        if response.tool_call_id != tool_call_id {
            continue;
        }
        let output = response
            .answers
            .iter()
            .enumerate()
            .map(|(index, answer)| {
                format!(
                    "Q{}: {}\nA{}: {}",
                    index + 1,
                    answer.question,
                    index + 1,
                    answer.answer
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        return ProviderQuestionResult {
            answers: response.answers,
            output,
        };
    }
    ProviderQuestionResult {
        answers: Vec::new(),
        output:
            "No question answers received; ask the user again if this decision is still required."
                .to_string(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderAuthKind {
    ApiKey,
    BearerToken,
    SessionToken,
    Unknown,
}

#[derive(Clone)]
pub struct ProviderAuth {
    kind: ProviderAuthKind,
    secret: SecretString,
}

impl ProviderAuth {
    pub fn new(kind: ProviderAuthKind, secret: SecretString) -> Self {
        Self { kind, secret }
    }

    pub fn kind(&self) -> ProviderAuthKind {
        self.kind
    }

    pub fn expose_secret(&self) -> &str {
        self.secret.expose_secret()
    }
}

impl fmt::Debug for ProviderAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ProviderAuth")
            .field("kind", &self.kind)
            .field("secret", &"<redacted>")
            .finish()
    }
}

#[async_trait::async_trait]
pub trait ProviderPlugin: Send + Sync {
    fn id(&self) -> &'static str;

    fn capabilities(&self) -> ProviderCapabilities;

    async fn list_models(&self, auth: &ProviderAuth) -> anyhow::Result<Vec<ModelInfo>>;

    #[allow(clippy::too_many_arguments)]
    async fn stream_turn(
        &self,
        auth: &ProviderAuth,
        req: TurnRequest,
        cancel: CancellationToken,
        permissions: PermissionResponses,
        tool_responses: ToolResponses,
        question_responses: QuestionResponses,
        question_requests: QuestionRequests,
    ) -> anyhow::Result<Pin<Box<dyn Stream<Item = anyhow::Result<SessionEvent>> + Send>>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_auth_debug_redacts_secret() {
        let auth = ProviderAuth::new(
            ProviderAuthKind::SessionToken,
            SecretString::from("secret-provider-token".to_string()),
        );

        let debug = format!("{auth:?}");

        assert!(debug.contains("ProviderAuth"));
        assert!(debug.contains("SessionToken"));
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("secret-provider-token"));
    }
}
