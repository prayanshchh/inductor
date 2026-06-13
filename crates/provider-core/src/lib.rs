use futures_core::Stream;
use harness_core::{
    ModelInfo, PermissionResponse, ProviderCapabilities, SessionEvent, ToolCallId, TurnRequest,
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

    async fn stream_turn(
        &self,
        auth: &ProviderAuth,
        req: TurnRequest,
        cancel: CancellationToken,
        permissions: PermissionResponses,
        tool_responses: ToolResponses,
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
