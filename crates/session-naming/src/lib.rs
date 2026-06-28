//! Automatic session naming using smaller, cheaper models.

use anyhow::Result;
use async_trait::async_trait;
use auth::{AuthDetector, ProviderKind, RuntimeCredentialLoader};
use futures_util::StreamExt;
use harness_core::{SessionId, TurnRequest};
use provider_claude::ClaudeProvider;
use provider_codex::CodexProvider;
use provider_copilot::CopilotProvider;
use provider_core::{ProviderAuth, ProviderAuthKind, ProviderPlugin};
use secrecy::SecretString;
use tokio_util::sync::CancellationToken;

/// Configuration for session naming
#[derive(Debug, Clone)]
pub struct SessionNamingConfig {
    /// The provider to use for naming (should be cheap/fast)
    pub provider: ProviderKind,
    /// The model to use for naming (should be cheap, like Haiku)
    pub model: String,
    /// Whether to enable session naming
    pub enabled: bool,
    /// Optional cwd for providers that derive workspace context from cwd.
    pub cwd: Option<std::path::PathBuf>,
}

impl Default for SessionNamingConfig {
    fn default() -> Self {
        Self {
            provider: ProviderKind::Claude,
            model: "haiku".to_string(), // Use cheaper Claude Haiku for naming
            enabled: true,
            cwd: None,
        }
    }
}

/// Trait for generating session names
#[async_trait]
pub trait SessionNamer: Send + Sync {
    async fn generate_name(&self, prompts: &[String]) -> Result<String>;
}

/// Session namer that uses a language model to generate names
pub struct ModelBasedNamer {
    config: SessionNamingConfig,
}

impl ModelBasedNamer {
    pub fn new(config: SessionNamingConfig) -> Self {
        Self { config }
    }

    async fn get_provider_and_auth(&self) -> Result<(Box<dyn ProviderPlugin>, ProviderAuth)> {
        let detector = AuthDetector::from_env()?;
        let credentials = detector.detect_all();
        let reference = credentials
            .iter()
            .find(|credential| credential.provider == self.config.provider)
            .ok_or_else(|| {
                anyhow::anyhow!("no detected credential for {:?}", self.config.provider)
            })?;

        let (provider, auth): (Box<dyn ProviderPlugin>, ProviderAuth) = match self.config.provider {
            ProviderKind::Claude => {
                let provider: Box<dyn ProviderPlugin> = if let Some(cwd) = &self.config.cwd {
                    Box::new(ClaudeProvider::with_cwd(cwd.clone()))
                } else {
                    Box::new(ClaudeProvider::new()?)
                };
                let auth = ProviderAuth::new(
                    ProviderAuthKind::SessionToken,
                    SecretString::from(String::new()),
                );
                (provider, auth)
            }
            ProviderKind::Codex => {
                let provider = Box::new(CodexProvider::new()?);
                let auth = RuntimeCredentialLoader::load(reference)?.into_provider_auth();
                (provider, auth)
            }
            ProviderKind::Copilot => {
                let provider = Box::new(CopilotProvider::new()?);
                let auth = RuntimeCredentialLoader::load(reference)?.into_provider_auth();
                (provider, auth)
            }
        };

        Ok((provider, auth))
    }
}

#[async_trait]
impl SessionNamer for ModelBasedNamer {
    async fn generate_name(&self, prompts: &[String]) -> Result<String> {
        if !self.config.enabled || prompts.is_empty() {
            return Ok("New Session".to_string());
        }

        // Take up to 2 prompts and limit their length to avoid token limits
        let prompt_content = prompts
            .iter()
            .take(2)
            .map(|p| {
                // Truncate very long prompts to stay within token limits
                if p.len() > 2000 {
                    format!("{}...", &p[..2000])
                } else {
                    p.clone()
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n");

        let naming_prompt = naming_prompt(&prompt_content);

        let (provider, auth) = self.get_provider_and_auth().await?;

        let session_id = SessionId::new();
        let request = TurnRequest {
            session_id,
            model: self.config.model.clone(),
            prompt: naming_prompt,
            system_prompt: None,
            messages: Vec::new(),
            tool_names: Vec::new(),
            metadata: serde_json::Value::Null,
            images: Vec::new(),
        };

        let cancel = CancellationToken::new();
        let (_perm_tx, perm_rx) = tokio::sync::mpsc::unbounded_channel();
        let tool_rx = provider_core::empty_tool_responses();

        let mut stream = provider
            .stream_turn(&auth, request, cancel, perm_rx, tool_rx, provider_core::empty_question_responses(), provider_core::empty_question_requests())
            .await?;
        let mut response_text = String::new();

        while let Some(event) = stream.next().await {
            let event = event?;
            if let harness_core::SessionEvent::TextDelta { text, .. } = event {
                response_text.push_str(&text);
            } else if let harness_core::SessionEvent::Result { .. } = event {
                break;
            }
        }

        let name = limit_words(&clean_title(&response_text), 3);

        // Fallback if the name is too long or empty
        if name.is_empty() || name.chars().count() > 50 {
            Ok("New Session".to_string())
        } else {
            Ok(name)
        }
    }
}

fn naming_prompt(prompt_content: &str) -> String {
    format!(
        "You are a title generator. You output ONLY a thread title. Nothing else.\n\n\
<task>\n\
Generate a brief title that would help the user find this conversation later.\n\
Your output must be a single line of AT MOST 3 words, no more than 50 characters, with no explanations or quotes.\n\
</task>\n\n\
<rules>\n\
- Use at most 3 words. Prefer 2-3 punchy words.\n\
- Use the same language as the user request.\n\
- Focus on the main topic or change the user needs.\n\
- Never include tool names.\n\
- Keep exact technical terms, numbers, filenames, and HTTP codes.\n\
- Do not answer questions or mention that you are generating a title.\n\
- Always output something meaningful, even if the input is minimal.\n\
</rules>\n\n\
User request(s):\n{prompt_content}"
    )
}

fn clean_title(response: &str) -> String {
    let line = response
        .trim()
        .lines()
        .next()
        .unwrap_or("New Session")
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim_end_matches('.')
        .trim_matches('"')
        .trim_matches('\'')
        .trim();

    line.to_string()
}

/// Keep at most `max_words` whitespace-separated words, dropping any trailing
/// punctuation the model may have appended to the last kept word.
fn limit_words(name: &str, max_words: usize) -> String {
    name.split_whitespace()
        .take(max_words)
        .collect::<Vec<_>>()
        .join(" ")
        .trim_end_matches(|c: char| c == ',' || c == '.' || c == ':' || c == ';')
        .to_string()
}

/// Generate a session name based on the first few user prompts
pub async fn generate_session_name(
    prompts: &[String],
    config: Option<SessionNamingConfig>,
) -> Result<String> {
    let config = config.unwrap_or_default();
    let namer = ModelBasedNamer::new(config);
    namer.generate_name(prompts).await
}

/// Generate a session/worktree/branch name from arbitrary context using the
/// same provider/model as the active user session.
pub async fn generate_context_name(
    context: &str,
    config: Option<SessionNamingConfig>,
) -> Result<String> {
    let config = config.unwrap_or_default();
    let namer = ModelBasedNamer::new(config);
    namer.generate_name(&[context.to_string()]).await
}

/// Generate a concise pull-request description with the selected provider/model.
///
/// The caller supplies the commit/PR title plus a git diff summary. The model is
/// asked for only the PR body so the result can be passed directly to
/// `gh pr create --body`.
pub async fn generate_pull_request_description(
    title: &str,
    diff_summary: &str,
    config: Option<SessionNamingConfig>,
) -> Result<String> {
    let config = config.unwrap_or_default();
    if !config.enabled || diff_summary.trim().is_empty() {
        return Ok(fallback_pr_description(title, diff_summary));
    }

    let namer = ModelBasedNamer::new(config.clone());
    let (provider, auth) = namer.get_provider_and_auth().await?;
    let request = TurnRequest {
        session_id: SessionId::new(),
        model: config.model,
        prompt: pr_description_prompt(title, diff_summary),
        system_prompt: None,
        messages: Vec::new(),
        tool_names: Vec::new(),
        metadata: serde_json::Value::Null,
        images: Vec::new(),
    };

    let cancel = CancellationToken::new();
    let (_perm_tx, perm_rx) = tokio::sync::mpsc::unbounded_channel();
    let tool_rx = provider_core::empty_tool_responses();
    let mut stream = provider
        .stream_turn(&auth, request, cancel, perm_rx, tool_rx, provider_core::empty_question_responses(), provider_core::empty_question_requests())
        .await?;
    let mut response_text = String::new();

    while let Some(event) = stream.next().await {
        let event = event?;
        if let harness_core::SessionEvent::TextDelta { text, .. } = event {
            response_text.push_str(&text);
        } else if let harness_core::SessionEvent::Result { .. } = event {
            break;
        }
    }

    let body = clean_pr_description(&response_text);
    if body.is_empty() {
        Ok(fallback_pr_description(title, diff_summary))
    } else {
        Ok(body)
    }
}

fn pr_description_prompt(title: &str, diff_summary: &str) -> String {
    let diff_summary = truncate_chars(diff_summary, 12_000);
    format!(
        "You are writing a GitHub pull request description. Output ONLY the PR body.\n\n\
Rules:\n\
- Be concise and accurate.\n\
- Describe what changed, not implementation speculation.\n\
- Use this exact structure:\n\
Summary:\n\
- <1-3 short bullets>\n\n\
Testing:\n\
- Not run (not requested)\n\n\
- Do not include a title, code fences, or extra commentary.\n\n\
PR title / commit message:\n{title}\n\n\
Git diff summary:\n{diff_summary}"
    )
}

fn clean_pr_description(response: &str) -> String {
    let mut body = response
        .trim()
        .trim_matches('`')
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .trim()
        .to_string();
    if body.starts_with("```") {
        body = body
            .lines()
            .filter(|line| !line.trim_start().starts_with("```"))
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string();
    }
    truncate_chars(&body, 4_000).trim().to_string()
}

fn fallback_pr_description(title: &str, diff_summary: &str) -> String {
    let mut changed = diff_summary
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.starts_with("diff --git ") {
                line.split_whitespace()
                    .nth(3)
                    .map(|path| path.trim_start_matches("b/"))
            } else {
                None
            }
        })
        .take(5)
        .collect::<Vec<_>>();
    changed.dedup();

    let summary = if changed.is_empty() {
        format!("- {title}")
    } else {
        format!("- {title}\n- Updates {}", changed.join(", "))
    };
    format!("Summary:\n{summary}\n\nTesting:\n- Not run (not requested)")
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut out = text.chars().take(max_chars).collect::<String>();
    if text.chars().count() > max_chars {
        out.push_str("\n… [truncated]");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_empty_prompts() {
        let config = SessionNamingConfig::default();
        let namer = ModelBasedNamer::new(config);
        let name = namer.generate_name(&[]).await.unwrap();
        assert_eq!(name, "New Session");
    }

    #[tokio::test]
    async fn test_disabled_config() {
        let config = SessionNamingConfig {
            enabled: false,
            ..Default::default()
        };
        let namer = ModelBasedNamer::new(config);
        let name = namer
            .generate_name(&["Test prompt".to_string()])
            .await
            .unwrap();
        assert_eq!(name, "New Session");
    }

    #[test]
    fn limit_words_caps_at_three_words() {
        assert_eq!(
            limit_words("Fix login refresh bug now", 3),
            "Fix login refresh"
        );
        assert_eq!(limit_words("Parser bug", 3), "Parser bug");
        assert_eq!(limit_words("Add  retry,", 3), "Add retry");
        assert_eq!(limit_words("", 3), "");
    }

    #[test]
    fn clean_title_removes_quotes_and_period() {
        assert_eq!(clean_title("\"Parser bug fix.\"\nextra"), "Parser bug fix");
    }

    #[test]
    fn naming_prompt_keeps_strict_rules() {
        let prompt = naming_prompt("fix auth refresh");

        assert!(prompt.contains("ONLY a thread title"));
        assert!(prompt.contains("fix auth refresh"));
    }
}
