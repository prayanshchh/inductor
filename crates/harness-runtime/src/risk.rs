//! Phase 6: risk classification and allow rules.
//!
//! The classifier inspects a parsed tool call and returns the set of
//! [`RiskFlag`]s that apply. The harness uses this, together with the active
//! [`ApprovalPolicy`], to decide whether a tool call must pause for approval.

use std::collections::{HashMap, HashSet};

use harness_core::{
    AllowRule, AllowRuleKind, ApprovalPolicy, PermissionDecision, PermissionRequestId, RiskFlag,
};
use regex::Regex;
use serde_json::Value;

use crate::ParsedToolCall;

/// Classify a tool call and return any risk flags that apply.
pub fn classify(call: &ParsedToolCall) -> Vec<RiskFlag> {
    let mut flags = Vec::new();

    match call.name.as_str() {
        "bash" => {
            if let Some(command) = call.input.get("command").and_then(Value::as_str) {
                classify_bash(command, &mut flags);
            }
        }
        "read_file" | "glob" | "grep" | "read_memory" => {
            if let Some(path) = call.input.get("path").and_then(Value::as_str) {
                classify_read_path(path, &mut flags);
            }
        }
        "list_dir" => {
            if let Some(path) = call.input.get("path").and_then(Value::as_str) {
                classify_read_path(path, &mut flags);
            }
        }
        "write_file" | "write_memory" | "edit_file" | "multi_edit" => {
            if let Some(path) = call.input.get("path").and_then(Value::as_str) {
                classify_write_path(path, &mut flags);
            }
        }
        "apply_patch" | "apply_patch_freeform" => {
            if let Some(patch) = call.input.get("patch").and_then(Value::as_str) {
                for path in patch_paths(patch) {
                    classify_write_path(&path, &mut flags);
                }
            }
        }
        "apply_patch_structured" => {
            if let Some(operations) = call.input.get("operations").and_then(Value::as_array) {
                for operation in operations {
                    for field in ["path", "from", "to"] {
                        if let Some(path) = operation.get(field).and_then(Value::as_str) {
                            classify_write_path(path, &mut flags);
                        }
                    }
                }
            }
        }
        "web_fetch" => push_unique(&mut flags, RiskFlag::NetworkAccess),
        _ => {}
    }

    flags
}

fn classify_bash(command: &str, flags: &mut Vec<RiskFlag>) {
    let lower = command.to_lowercase();

    // `rm -rf` / `rm -fr` and friends.
    if contains_word(&lower, "rm") && has_recursive_force_flags(&lower) {
        push_unique(flags, RiskFlag::RecursiveRemove);
    }
    if contains_word(&lower, "sudo") {
        push_unique(flags, RiskFlag::Sudo);
    }
    if lower.contains("git push") && (lower.contains("--force") || lower.contains("-f")) {
        push_unique(flags, RiskFlag::GitForcePush);
    }
    if lower.contains("npm publish")
        || lower.contains("cargo publish")
        || lower.contains("pip upload")
        || lower.contains("twine upload")
    {
        push_unique(flags, RiskFlag::PackagePublish);
    }
    if lower.contains("cargo install")
        || lower.contains("npm install -g")
        || lower.contains("npm i -g")
        || lower.contains("pip install")
    {
        push_unique(flags, RiskFlag::WriteOutsideWorkspace);
    }
    // Touching the git internals directly.
    if lower.contains(".git/") {
        push_unique(flags, RiskFlag::GitDirectory);
    }
    // Crude network-tool detection (the sandbox also denies network).
    if contains_word(&lower, "curl")
        || contains_word(&lower, "wget")
        || contains_word(&lower, "nc")
        || contains_word(&lower, "ssh")
        || contains_word(&lower, "scp")
    {
        push_unique(flags, RiskFlag::NetworkAccess);
    }
}

fn is_outside_workspace(path: &str) -> bool {
    path.starts_with('/') || path.contains("..")
}

fn classify_read_path(path: &str, flags: &mut Vec<RiskFlag>) {
    if is_outside_workspace(path) {
        push_unique(flags, RiskFlag::ReadOutsideWorkspace);
    }
}

fn classify_write_path(path: &str, flags: &mut Vec<RiskFlag>) {
    if is_outside_workspace(path) {
        push_unique(flags, RiskFlag::WriteOutsideWorkspace);
    }
    if path.contains(".git/") || path == ".git" {
        push_unique(flags, RiskFlag::GitDirectory);
    }

    let file_name = path.rsplit('/').next().unwrap_or(path);
    if file_name == ".env" || file_name.starts_with(".env.") {
        push_unique(flags, RiskFlag::EnvFile);
    } else if file_name.starts_with('.') && !file_name.is_empty() {
        push_unique(flags, RiskFlag::Dotfile);
    }
}

fn has_recursive_force_flags(command: &str) -> bool {
    // Match `-rf`, `-fr`, `-r ... -f`, or long forms.
    let combined = command.contains("-rf") || command.contains("-fr");
    let long = command.contains("--recursive") && command.contains("--force");
    let split = (command.contains("-r") || command.contains("--recursive"))
        && (command.contains("-f") || command.contains("--force"));
    combined || long || split
}

/// Whole-word containment so `format` does not match `rm`. Splits on shell
/// separators and whitespace, then compares tokens exactly.
fn contains_word(haystack: &str, word: &str) -> bool {
    haystack
        .split(|c: char| c.is_whitespace() || matches!(c, ';' | '|' | '&' | '(' | ')' | '<' | '>'))
        .any(|token| token == word)
}

fn push_unique(flags: &mut Vec<RiskFlag>, flag: RiskFlag) {
    if !flags.contains(&flag) {
        flags.push(flag);
    }
}

/// A set of allow rules that pre-approve matching tool calls for a session.
#[derive(Debug, Default)]
pub struct AllowStore {
    rules: Vec<AllowRule>,
}

impl AllowStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_rules(rules: Vec<AllowRule>) -> Self {
        Self { rules }
    }

    pub fn add(&mut self, rule: AllowRule) {
        if !self.rules.contains(&rule) {
            self.rules.push(rule);
        }
    }

    pub fn rules(&self) -> &[AllowRule] {
        &self.rules
    }

    /// Whether any stored rule pre-approves this call.
    pub fn is_allowed(&self, call: &ParsedToolCall) -> bool {
        self.rules.iter().any(|rule| rule_matches(rule, call))
    }

    /// Derive a rule that would pre-approve this exact call in the future,
    /// used when the user picks "allow always".
    pub fn rule_for(call: &ParsedToolCall) -> AllowRule {
        match call.name.as_str() {
            "bash" => {
                let command = call
                    .input
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                // Use the first token (the program) as a conservative prefix.
                let prefix = command.split_whitespace().next().unwrap_or(command);
                AllowRule::new(AllowRuleKind::BashPrefix, prefix)
            }
            other => AllowRule::new(AllowRuleKind::ToolName, other),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPermission {
    pub request_id: PermissionRequestId,
    pub tool_name: String,
    pub input: Value,
    pub risk_flags: Vec<RiskFlag>,
    pub reason: String,
    fingerprint: String,
}

#[derive(Debug)]
pub struct PermissionService<'a> {
    allow: &'a mut AllowStore,
    pending: HashMap<PermissionRequestId, PendingPermission>,
    denied: HashSet<String>,
}

impl<'a> PermissionService<'a> {
    pub fn new(allow: &'a mut AllowStore) -> Self {
        Self {
            allow,
            pending: HashMap::new(),
            denied: HashSet::new(),
        }
    }

    pub fn rules(&self) -> &[AllowRule] {
        self.allow.rules()
    }

    pub fn is_allowed(&self, call: &ParsedToolCall) -> bool {
        self.allow.is_allowed(call)
    }

    pub fn is_denied_for_session(&self, call: &ParsedToolCall) -> bool {
        self.denied.contains(&call_fingerprint(call))
    }

    pub fn should_request(
        &self,
        policy: ApprovalPolicy,
        risk_flags: &[RiskFlag],
        call: &ParsedToolCall,
    ) -> bool {
        if self.is_allowed(call) {
            return false;
        }
        match policy {
            ApprovalPolicy::Never => false,
            ApprovalPolicy::Always => true,
            ApprovalPolicy::OnRequest => !risk_flags.is_empty(),
            ApprovalPolicy::Mutating => !risk_flags.is_empty() || is_mutating_tool_name(&call.name),
            ApprovalPolicy::OnFailure => false,
        }
    }

    pub fn begin_request(
        &mut self,
        call: &ParsedToolCall,
        input: Value,
        risk_flags: Vec<RiskFlag>,
        reason: String,
    ) -> PendingPermission {
        let pending = PendingPermission {
            request_id: PermissionRequestId::new(),
            tool_name: call.name.clone(),
            input,
            risk_flags,
            reason,
            fingerprint: call_fingerprint(call),
        };
        self.pending.insert(pending.request_id, pending.clone());
        pending
    }

    pub fn resolve(
        &mut self,
        request_id: PermissionRequestId,
        decision: PermissionDecision,
        call: &ParsedToolCall,
    ) {
        let fingerprint = self
            .pending
            .remove(&request_id)
            .map(|pending| pending.fingerprint)
            .unwrap_or_else(|| call_fingerprint(call));
        match decision {
            PermissionDecision::AllowAlways => self.allow.add(AllowStore::rule_for(call)),
            PermissionDecision::Deny => {
                self.denied.insert(fingerprint);
            }
            PermissionDecision::Allow => {}
        }
    }
}

pub fn is_mutating_tool_name(name: &str) -> bool {
    matches!(
        name,
        "write_file"
            | "write_memory"
            | "edit_file"
            | "multi_edit"
            | "apply_patch"
            | "apply_patch_freeform"
            | "apply_patch_structured"
            | "todo_write"
            | "bash_kill"
            | "bash"
    )
}

fn rule_matches(rule: &AllowRule, call: &ParsedToolCall) -> bool {
    match rule.kind {
        AllowRuleKind::ToolName => wildcard_match(&rule.value, &call.name),
        AllowRuleKind::BashPrefix => {
            call.name == "bash"
                && call
                    .input
                    .get("command")
                    .and_then(Value::as_str)
                    .map(|cmd| cmd.trim_start().starts_with(&rule.value))
                    .unwrap_or(false)
        }
        AllowRuleKind::BashRegex => {
            call.name == "bash"
                && Regex::new(&rule.value)
                    .ok()
                    .zip(call.input.get("command").and_then(Value::as_str))
                    .map(|(re, cmd)| re.is_match(cmd))
                    .unwrap_or(false)
        }
        AllowRuleKind::PathWrite => {
            matches!(
                call.name.as_str(),
                "write_file"
                    | "write_memory"
                    | "edit_file"
                    | "multi_edit"
                    | "apply_patch"
                    | "apply_patch_freeform"
                    | "apply_patch_structured"
            ) && call
                .input
                .get("path")
                .and_then(Value::as_str)
                .map(|path| wildcard_match(&rule.value, path) || path.starts_with(&rule.value))
                .unwrap_or(false)
        }
    }
}

fn patch_paths(patch: &str) -> Vec<String> {
    patch
        .lines()
        .filter_map(|line| {
            line.strip_prefix("*** Add File: ")
                .or_else(|| line.strip_prefix("*** Update File: "))
                .or_else(|| line.strip_prefix("*** Delete File: "))
                .or_else(|| line.strip_prefix("--- a/"))
                .or_else(|| line.strip_prefix("+++ b/"))
                .map(str::trim)
                .filter(|path| !path.is_empty() && *path != "/dev/null")
                .map(str::to_string)
        })
        .collect()
}

fn call_fingerprint(call: &ParsedToolCall) -> String {
    format!("{}:{}", call.name, call.input)
}

fn wildcard_match(pattern: &str, text: &str) -> bool {
    fn inner(pattern: &[u8], text: &[u8]) -> bool {
        if pattern.is_empty() {
            return text.is_empty();
        }
        if pattern[0] == b'*' {
            return inner(&pattern[1..], text) || (!text.is_empty() && inner(pattern, &text[1..]));
        }
        if !text.is_empty() && (pattern[0] == b'?' || pattern[0] == text[0]) {
            return inner(&pattern[1..], &text[1..]);
        }
        false
    }

    inner(pattern.as_bytes(), text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bash(command: &str) -> ParsedToolCall {
        ParsedToolCall {
            name: "bash".to_string(),
            input: json!({ "command": command }),
        }
    }

    fn write(path: &str) -> ParsedToolCall {
        ParsedToolCall {
            name: "write_file".to_string(),
            input: json!({ "path": path, "content": "x" }),
        }
    }

    #[test]
    fn flags_rm_rf() {
        assert!(classify(&bash("rm -rf build")).contains(&RiskFlag::RecursiveRemove));
        assert!(classify(&bash("rm -fr build")).contains(&RiskFlag::RecursiveRemove));
    }

    #[test]
    fn flags_sudo_and_force_push() {
        assert!(classify(&bash("sudo make install")).contains(&RiskFlag::Sudo));
        assert!(classify(&bash("git push --force origin main")).contains(&RiskFlag::GitForcePush));
    }

    #[test]
    fn flags_package_publish() {
        assert!(classify(&bash("cargo publish")).contains(&RiskFlag::PackagePublish));
    }

    #[test]
    fn flags_global_install_as_outside_write() {
        assert!(
            classify(&bash(
                "cargo install --path crates/agent --bin inductor --force"
            ))
            .contains(&RiskFlag::WriteOutsideWorkspace)
        );
    }

    #[test]
    fn benign_command_has_no_flags() {
        assert!(classify(&bash("ls -la && pwd")).is_empty());
    }

    #[test]
    fn flags_env_and_dotfiles_and_escapes() {
        assert!(classify(&write(".env")).contains(&RiskFlag::EnvFile));
        assert!(classify(&write(".bashrc")).contains(&RiskFlag::Dotfile));
        assert!(classify(&write("/etc/hosts")).contains(&RiskFlag::WriteOutsideWorkspace));
        assert!(classify(&write("../escape.txt")).contains(&RiskFlag::WriteOutsideWorkspace));
    }

    #[test]
    fn benign_write_has_no_flags() {
        assert!(classify(&write("src/main.rs")).is_empty());
    }

    #[test]
    fn allow_store_matches_bash_prefix() {
        let mut store = AllowStore::new();
        store.add(AllowRule::new(AllowRuleKind::BashPrefix, "cargo"));

        assert!(store.is_allowed(&bash("cargo test")));
        assert!(!store.is_allowed(&bash("rm -rf /")));
    }

    #[test]
    fn allow_store_matches_tool_name_and_regex() {
        let store = AllowStore::with_rules(vec![
            AllowRule::new(AllowRuleKind::ToolName, "read_file"),
            AllowRule::new(AllowRuleKind::BashRegex, r"^echo\s"),
        ]);

        assert!(store.is_allowed(&ParsedToolCall {
            name: "read_file".to_string(),
            input: json!({ "path": "a.txt" }),
        }));
        assert!(store.is_allowed(&bash("echo hi")));
        assert!(!store.is_allowed(&bash("ls")));
    }

    #[test]
    fn rule_for_bash_uses_program_prefix() {
        let rule = AllowStore::rule_for(&bash("cargo test --all"));
        assert_eq!(rule.kind, AllowRuleKind::BashPrefix);
        assert_eq!(rule.value, "cargo");
    }
}
