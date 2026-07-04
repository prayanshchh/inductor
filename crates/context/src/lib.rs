use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelEffort {
    None,
    Minimal,
    Low,
    Medium,
    High,
    #[serde(rename = "xhigh", alias = "x_high", alias = "x-high")]
    XHigh,
    Max,
}

impl Default for ModelEffort {
    fn default() -> Self {
        Self::Medium
    }
}

impl ModelEffort {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderFamily {
    Claude,
    Codex,
    Copilot,
    Generic,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderEffort {
    pub provider: ProviderFamily,
    pub model_effort: ModelEffort,
    pub parameter_name: Option<String>,
    pub parameter_value: Option<String>,
    pub prompt_hint: Option<String>,
}

pub fn translate_effort(provider: ProviderFamily, effort: ModelEffort) -> ProviderEffort {
    match provider {
        ProviderFamily::Codex => ProviderEffort {
            provider,
            model_effort: effort,
            parameter_name: Some("reasoning_effort".to_string()),
            parameter_value: Some(effort.as_str().to_string()),
            prompt_hint: None,
        },
        ProviderFamily::Copilot => ProviderEffort {
            provider,
            model_effort: effort,
            parameter_name: None,
            parameter_value: None,
            prompt_hint: Some(format!("Reasoning effort: {}", effort.as_str())),
        },
        ProviderFamily::Claude => ProviderEffort {
            provider,
            model_effort: effort,
            parameter_name: None,
            parameter_value: None,
            prompt_hint: Some(format!(
                "Use {} reasoning effort for this turn.",
                effort.as_str()
            )),
        },
        ProviderFamily::Generic => ProviderEffort {
            provider,
            model_effort: effort,
            parameter_name: None,
            parameter_value: None,
            prompt_hint: Some(format!("Reasoning effort: {}", effort.as_str())),
        },
    }
}

pub trait TokenCounter: Send + Sync {
    fn count_tokens(&self, text: &str) -> usize;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct ApproxTokenCounter;

impl TokenCounter for ApproxTokenCounter {
    fn count_tokens(&self, text: &str) -> usize {
        let chars = text.chars().count();
        let words = text.split_whitespace().count();
        chars.div_ceil(4).max(words).max(1)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextLimits {
    pub soft_tokens: usize,
    pub hard_tokens: usize,
    pub tool_result_inline_bytes: usize,
}

impl Default for ContextLimits {
    fn default() -> Self {
        Self {
            soft_tokens: 16_000,
            hard_tokens: 24_000,
            tool_result_inline_bytes: 4 * 1024,
        }
    }
}

impl ContextLimits {
    pub fn new(soft_tokens: usize, hard_tokens: usize, tool_result_inline_bytes: usize) -> Self {
        Self {
            soft_tokens,
            hard_tokens: hard_tokens.max(soft_tokens),
            tool_result_inline_bytes,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextMessage {
    pub role: String,
    pub content: String,
}

impl ContextMessage {
    pub fn new(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedContext {
    pub prompt: String,
    #[serde(default)]
    pub messages: Vec<ContextMessage>,
    pub token_count: usize,
    pub compacted: bool,
    pub original_token_count: usize,
    pub summary: Option<String>,
}

pub fn prepare_context(
    system_preamble: &str,
    messages: &[ContextMessage],
    limits: &ContextLimits,
    counter: &dyn TokenCounter,
) -> Result<PreparedContext, ContextError> {
    let prompt = render_prompt(system_preamble, messages);
    let original_token_count = counter.count_tokens(&prompt);
    if original_token_count <= limits.soft_tokens {
        return Ok(PreparedContext {
            prompt,
            messages: messages.to_vec(),
            token_count: original_token_count,
            compacted: false,
            original_token_count,
            summary: None,
        });
    }

    let compacted_messages = compact_messages(messages);
    let summary = compacted_messages
        .first()
        .map(|message| message.content.clone());
    let prompt = render_prompt(system_preamble, &compacted_messages);
    let token_count = counter.count_tokens(&prompt);
    if token_count > limits.hard_tokens {
        return Err(ContextError::HardLimitExceeded {
            tokens: token_count,
            hard_limit: limits.hard_tokens,
        });
    }

    Ok(PreparedContext {
        prompt,
        messages: compacted_messages,
        token_count,
        compacted: true,
        original_token_count,
        summary,
    })
}

pub fn render_prompt(system_preamble: &str, messages: &[ContextMessage]) -> String {
    let mut prompt = String::from(system_preamble);
    prompt.push_str("\n\n--- Conversation ---\n");
    for message in messages {
        prompt.push_str(&message.role);
        prompt.push_str(":\n");
        prompt.push_str(&message.content);
        prompt.push_str("\n\n");
    }
    prompt.push_str("Assistant:\n");
    prompt
}

pub fn compact_messages(messages: &[ContextMessage]) -> Vec<ContextMessage> {
    if messages.len() <= 4 {
        return messages.to_vec();
    }

    let split_at = messages.len().saturating_sub(4);
    let mut compacted = Vec::with_capacity(5);
    compacted.push(ContextMessage::new(
        "System",
        summarize_messages(&messages[..split_at]),
    ));
    compacted.extend_from_slice(&messages[split_at..]);
    compacted
}

pub fn summarize_messages(messages: &[ContextMessage]) -> String {
    let mut summary = format!("Compacted {} earlier message(s).", messages.len());
    for message in messages.iter().take(12) {
        summary.push_str("\n- ");
        summary.push_str(&message.role);
        summary.push_str(": ");
        summary.push_str(&single_line_preview(&message.content, 180));
    }
    if messages.len() > 12 {
        summary.push_str(&format!("\n- ... {} more message(s)", messages.len() - 12));
    }
    summary
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRef {
    pub id: String,
    pub path: PathBuf,
    pub bytes: usize,
}

#[derive(Debug, Clone)]
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn store(&self, bytes: &[u8]) -> Result<BlobRef, ContextError> {
        fs::create_dir_all(&self.root).map_err(|source| ContextError::BlobIo {
            path: self.root.clone(),
            source,
        })?;
        let id = sha256_hex(bytes);
        let path = self.root.join(&id);
        fs::write(&path, bytes).map_err(|source| ContextError::BlobIo {
            path: path.clone(),
            source,
        })?;
        Ok(BlobRef {
            id,
            path,
            bytes: bytes.len(),
        })
    }

    pub fn read(&self, reference: &BlobRef) -> Result<Vec<u8>, ContextError> {
        fs::read(&reference.path).map_err(|source| ContextError::BlobIo {
            path: reference.path.clone(),
            source,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StubbedToolOutput {
    pub inline_output: String,
    pub blob: Option<BlobRef>,
}

pub fn stub_tool_output(
    output: &str,
    limit_bytes: usize,
    store: Option<&BlobStore>,
) -> Result<StubbedToolOutput, ContextError> {
    if output.len() <= limit_bytes {
        return Ok(StubbedToolOutput {
            inline_output: output.to_string(),
            blob: None,
        });
    }

    let blob = match store {
        Some(store) => Some(store.store(output.as_bytes())?),
        None => None,
    };
    let inline = truncate_utf8(output, limit_bytes);
    let suffix = match &blob {
        Some(blob) => format!(
            "\n\n[Inductor truncated this tool output from {} bytes to {} bytes. Full output stored in blob {} at {}. To inspect more without rerunning the tool, call read_blob with blob_id \"{}\".]",
            output.len(),
            inline.len(),
            blob.id,
            blob.path.display(),
            blob.id
        ),
        None => format!(
            "\n\n[Inductor truncated this tool output from {} bytes to {} bytes. Full output was not stored; rerun with a narrower query or configure a blob root.]",
            output.len(),
            inline.len()
        ),
    };

    Ok(StubbedToolOutput {
        inline_output: format!("{inline}{suffix}"),
        blob,
    })
}

fn truncate_utf8(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

fn single_line_preview(text: &str, limit: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_utf8(&text, limit)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        output.push_str(&format!("{byte:02x}"));
    }
    output
}

#[derive(Debug)]
pub enum ContextError {
    HardLimitExceeded { tokens: usize, hard_limit: usize },
    BlobIo { path: PathBuf, source: io::Error },
}

impl fmt::Display for ContextError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HardLimitExceeded { tokens, hard_limit } => write!(
                f,
                "context exceeds hard limit: {tokens} tokens > {hard_limit} tokens"
            ),
            Self::BlobIo { path, source } => write!(f, "{}: {source}", path.display()),
        }
    }
}

impl std::error::Error for ContextError {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn approximate_counter_counts_at_least_words() {
        let counter = ApproxTokenCounter;
        assert!(counter.count_tokens("one two three four") >= 4);
    }

    #[test]
    fn below_soft_limit_does_not_compact() {
        let counter = ApproxTokenCounter;
        let messages = vec![ContextMessage::new("User", "short")];
        let prepared = prepare_context(
            "system",
            &messages,
            &ContextLimits::new(100, 200, 100),
            &counter,
        )
        .unwrap();

        assert!(!prepared.compacted);
        assert!(prepared.prompt.contains("User:\nshort"));
    }

    #[test]
    fn above_soft_limit_compacts_old_messages() {
        let counter = ApproxTokenCounter;
        let messages = (0..10)
            .map(|index| ContextMessage::new("User", format!("message {index} {}", "x".repeat(80))))
            .collect::<Vec<_>>();

        let prepared = prepare_context(
            "system",
            &messages,
            &ContextLimits::new(80, 400, 100),
            &counter,
        )
        .unwrap();

        assert!(prepared.compacted);
        assert!(prepared.prompt.contains("Compacted 6 earlier message"));
        assert!(prepared.prompt.contains("message 9"));
    }

    #[test]
    fn above_hard_limit_errors_after_compaction() {
        let counter = ApproxTokenCounter;
        let messages = (0..10)
            .map(|index| {
                ContextMessage::new("User", format!("message {index} {}", "x".repeat(500)))
            })
            .collect::<Vec<_>>();

        let error = prepare_context(
            "system",
            &messages,
            &ContextLimits::new(10, 20, 100),
            &counter,
        )
        .unwrap_err();

        assert!(matches!(error, ContextError::HardLimitExceeded { .. }));
    }

    #[test]
    fn stubs_large_tool_output_and_stores_blob() {
        let temp = TempDir::new("blob");
        let store = BlobStore::new(temp.path());
        let output = "abc".repeat(100);

        let stubbed = stub_tool_output(&output, 20, Some(&store)).unwrap();

        assert!(stubbed.inline_output.starts_with("abcabc"));
        assert!(stubbed.inline_output.contains("Inductor truncated"));
        let blob = stubbed.blob.unwrap();
        assert_eq!(store.read(&blob).unwrap(), output.as_bytes());
    }

    #[test]
    fn effort_translation_maps_codex_to_reasoning_effort() {
        let mapping = translate_effort(ProviderFamily::Codex, ModelEffort::High);

        assert_eq!(mapping.parameter_name.as_deref(), Some("reasoning_effort"));
        assert_eq!(mapping.parameter_value.as_deref(), Some("high"));
        assert!(mapping.prompt_hint.is_none());
    }

    #[test]
    fn model_effort_serializes_xhigh_in_provider_format() {
        assert_eq!(
            serde_json::to_value(ModelEffort::XHigh).unwrap(),
            serde_json::json!("xhigh")
        );
        assert_eq!(
            serde_json::from_value::<ModelEffort>(serde_json::json!("x_high")).unwrap(),
            ModelEffort::XHigh
        );
    }

    #[test]
    fn effort_translation_maps_claude_to_prompt_hint() {
        let mapping = translate_effort(ProviderFamily::Claude, ModelEffort::Low);

        assert!(mapping.parameter_name.is_none());
        assert!(mapping.prompt_hint.unwrap().contains("low"));
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
            let path = std::env::temp_dir().join(format!("inductor-context-{label}-{nanos}"));
            fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}
