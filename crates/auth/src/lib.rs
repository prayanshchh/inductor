use std::{
    error::Error,
    fmt, fs,
    path::{Path, PathBuf},
    process::Command,
};

use harness_core::ProviderId;
use provider_core::{ProviderAuth, ProviderAuthKind};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const CLAUDE_CODE_KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
const GITHUB_COPILOT_TOKEN_ENV: &str = "GITHUB_COPILOT_TOKEN";
const GITHUB_COPILOT_OAUTH_CLIENT_ID: &str = "Iv1.b507a08c87ecfe98";

#[derive(Debug, Clone)]
pub struct AuthDetector {
    home_dir: PathBuf,
    codex_home: Option<PathBuf>,
}

impl AuthDetector {
    pub fn from_env() -> Result<Self, AuthError> {
        let home_dir = std::env::var_os("HOME")
            .map(PathBuf::from)
            .ok_or(AuthError::HomeNotSet)?;
        let codex_home = std::env::var_os("CODEX_HOME").map(PathBuf::from);

        Ok(Self {
            home_dir,
            codex_home,
        })
    }

    pub fn new(home_dir: PathBuf, codex_home: Option<PathBuf>) -> Self {
        Self {
            home_dir,
            codex_home,
        }
    }

    pub fn detect_all(&self) -> Vec<DetectedCredential> {
        let mut detected = Vec::new();

        if let Some(credential) = self.detect_codex() {
            detected.push(credential);
        }

        if let Some(credential) = self.detect_claude() {
            detected.push(credential);
        }

        if let Some(credential) = self.detect_copilot() {
            detected.push(credential);
        }

        detected
    }

    pub fn detect_codex(&self) -> Option<DetectedCredential> {
        let auth_path = self.codex_auth_path();
        let identity_hint = codex_identity_hint(&auth_path)?;

        Some(DetectedCredential {
            provider: ProviderKind::Codex,
            provider_id: ProviderId("codex".to_string()),
            source: CredentialSource::File { path: auth_path },
            identity_hint,
        })
    }

    pub fn detect_claude(&self) -> Option<DetectedCredential> {
        if !keychain_item_exists(CLAUDE_CODE_KEYCHAIN_SERVICE) {
            return None;
        }

        Some(DetectedCredential {
            provider: ProviderKind::Claude,
            provider_id: ProviderId("claude".to_string()),
            source: CredentialSource::MacosKeychain {
                service: CLAUDE_CODE_KEYCHAIN_SERVICE.to_string(),
            },
            identity_hint: None,
        })
    }

    pub fn detect_copilot(&self) -> Option<DetectedCredential> {
        if std::env::var_os(GITHUB_COPILOT_TOKEN_ENV).is_some() {
            return Some(DetectedCredential {
                provider: ProviderKind::Copilot,
                provider_id: ProviderId("copilot".to_string()),
                source: CredentialSource::Environment {
                    variable: GITHUB_COPILOT_TOKEN_ENV.to_string(),
                },
                identity_hint: None,
            });
        }

        let auth_path = self.copilot_auth_path();
        let identity_hint = copilot_identity_hint(&auth_path)?;

        Some(DetectedCredential {
            provider: ProviderKind::Copilot,
            provider_id: ProviderId("copilot".to_string()),
            source: CredentialSource::File { path: auth_path },
            identity_hint,
        })
    }

    fn codex_auth_path(&self) -> PathBuf {
        self.codex_home
            .clone()
            .unwrap_or_else(|| self.home_dir.join(".codex"))
            .join("auth.json")
    }

    pub fn copilot_auth_path(&self) -> PathBuf {
        self.home_dir
            .join(".config")
            .join("github-copilot")
            .join("apps.json")
    }

    pub fn home_dir(&self) -> &Path {
        &self.home_dir
    }
}

#[derive(Clone)]
pub struct RuntimeCredential {
    provider: ProviderKind,
    secret: SecretString,
}

impl RuntimeCredential {
    pub fn provider(&self) -> ProviderKind {
        self.provider
    }

    pub fn expose_secret(&self) -> &str {
        self.secret.expose_secret()
    }

    pub fn into_provider_auth(self) -> ProviderAuth {
        ProviderAuth::new(self.provider.provider_auth_kind(), self.secret)
    }
}

impl fmt::Debug for RuntimeCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RuntimeCredential")
            .field("provider", &self.provider)
            .field("secret", &"<redacted>")
            .finish()
    }
}

pub struct RuntimeCredentialLoader;

impl RuntimeCredentialLoader {
    pub fn load(reference: &DetectedCredential) -> Result<RuntimeCredential, CredentialLoadError> {
        let secret = match (&reference.provider, &reference.source) {
            (ProviderKind::Codex, CredentialSource::File { path }) => load_codex_secret(path)?,
            (ProviderKind::Claude, CredentialSource::MacosKeychain { service }) => {
                load_claude_keychain_access_token(service)?
            }
            (ProviderKind::Copilot, CredentialSource::Environment { variable }) => {
                load_env_secret(ProviderKind::Copilot, variable)?
            }
            (ProviderKind::Copilot, CredentialSource::File { path }) => {
                load_copilot_oauth_token(path)?
            }
            (provider, source) => {
                return Err(CredentialLoadError::ProviderSourceMismatch {
                    provider: *provider,
                    source: source.clone(),
                });
            }
        };

        Ok(RuntimeCredential {
            provider: reference.provider,
            secret,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Claude,
    Codex,
    Copilot,
}

impl ProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Copilot => "copilot",
        }
    }

    pub fn provider_auth_kind(self) -> ProviderAuthKind {
        match self {
            Self::Claude | Self::Codex | Self::Copilot => ProviderAuthKind::SessionToken,
        }
    }
}

impl fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CredentialSource {
    MacosKeychain { service: String },
    File { path: PathBuf },
    Environment { variable: String },
}

impl CredentialSource {
    pub fn display_safe(&self, home_dir: &Path) -> String {
        match self {
            Self::MacosKeychain { service } => format!("macos_keychain:{service}"),
            Self::File { path } => format!("file:{}", display_path_safe(path, home_dir)),
            Self::Environment { variable } => format!("env:{variable}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DetectedCredential {
    pub provider: ProviderKind,
    pub provider_id: ProviderId,
    pub source: CredentialSource,
    pub identity_hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthError {
    HomeNotSet,
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::HomeNotSet => f.write_str("HOME is not set"),
        }
    }
}

impl std::error::Error for AuthError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialLoadError {
    MissingFile(PathBuf),
    InvalidJson(PathBuf),
    InvalidKeychainJson {
        service: String,
    },
    MissingSecret {
        provider: ProviderKind,
        source: CredentialSource,
    },
    KeychainLookupFailed {
        service: String,
        stderr: String,
    },
    ProviderSourceMismatch {
        provider: ProviderKind,
        source: CredentialSource,
    },
}

impl fmt::Display for CredentialLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingFile(path) => {
                write!(f, "credential source file is missing: {}", path.display())
            }
            Self::InvalidJson(path) => {
                write!(
                    f,
                    "credential source file is not valid JSON: {}",
                    path.display()
                )
            }
            Self::InvalidKeychainJson { service } => write!(
                f,
                "macOS Keychain item for service {service} is not valid Claude Code credential JSON"
            ),
            Self::MissingSecret { provider, source } => write!(
                f,
                "credential source for {provider} does not contain a supported secret field: {source:?}"
            ),
            Self::KeychainLookupFailed { service, stderr } => write!(
                f,
                "failed to read macOS Keychain item for service {service}: {}",
                stderr.trim()
            ),
            Self::ProviderSourceMismatch { provider, source } => {
                write!(
                    f,
                    "unsupported credential source for {provider}: {source:?}"
                )
            }
        }
    }
}

impl Error for CredentialLoadError {}

fn codex_identity_hint(path: &Path) -> Option<Option<String>> {
    let raw = fs::read_to_string(path).ok()?;
    let json: Value = serde_json::from_str(&raw).ok()?;

    if !json.is_object() {
        return None;
    }

    Some(find_identity_hint(&json))
}

fn copilot_identity_hint(path: &Path) -> Option<Option<String>> {
    let raw = fs::read_to_string(path).ok()?;
    let json: Value = serde_json::from_str(&raw).ok()?;
    let entry = copilot_app_entry(&json)?;
    Some(find_identity_hint(entry))
}

fn load_codex_secret(path: &Path) -> Result<SecretString, CredentialLoadError> {
    let json = read_json_file(path)?;
    let secret = find_secret_string(
        &json,
        &[
            "api_key",
            "openai_api_key",
            "access_token",
            "id_token",
            "refresh_token",
        ],
    )
    .ok_or_else(|| CredentialLoadError::MissingSecret {
        provider: ProviderKind::Codex,
        source: CredentialSource::File {
            path: path.to_path_buf(),
        },
    })?;

    Ok(SecretString::from(secret))
}

fn load_copilot_oauth_token(path: &Path) -> Result<SecretString, CredentialLoadError> {
    let json = read_json_file(path)?;
    let entry = copilot_app_entry(&json).ok_or_else(|| CredentialLoadError::MissingSecret {
        provider: ProviderKind::Copilot,
        source: CredentialSource::File {
            path: path.to_path_buf(),
        },
    })?;
    let secret = entry
        .get("oauth_token")
        .or_else(|| entry.get("access_token"))
        .or_else(|| entry.get("token"))
        .and_then(Value::as_str)
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| CredentialLoadError::MissingSecret {
            provider: ProviderKind::Copilot,
            source: CredentialSource::File {
                path: path.to_path_buf(),
            },
        })?;

    Ok(SecretString::from(secret.to_string()))
}

fn copilot_app_entry(json: &Value) -> Option<&Value> {
    let key = format!("github.com:{GITHUB_COPILOT_OAUTH_CLIENT_ID}");
    json.get(&key).or_else(|| {
        json.as_object().and_then(|object| {
            object
                .iter()
                .find(|(key, _)| key.ends_with(GITHUB_COPILOT_OAUTH_CLIENT_ID))
                .map(|(_, value)| value)
        })
    })
}

fn load_env_secret(
    provider: ProviderKind,
    variable: &str,
) -> Result<SecretString, CredentialLoadError> {
    let secret = std::env::var(variable)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| CredentialLoadError::MissingSecret {
            provider,
            source: CredentialSource::Environment {
                variable: variable.to_string(),
            },
        })?;
    Ok(SecretString::from(secret))
}

fn read_json_file(path: &Path) -> Result<Value, CredentialLoadError> {
    let raw = fs::read_to_string(path)
        .map_err(|_| CredentialLoadError::MissingFile(path.to_path_buf()))?;
    serde_json::from_str(&raw).map_err(|_| CredentialLoadError::InvalidJson(path.to_path_buf()))
}

fn find_identity_hint(json: &Value) -> Option<String> {
    for key in ["email", "user_email", "username", "user"] {
        if let Some(value) = json.get(key).and_then(Value::as_str) {
            return Some(value.to_string());
        }
    }

    for object_key in ["account", "profile", "user"] {
        if let Some(object) = json.get(object_key) {
            if let Some(value) = find_identity_hint(object) {
                return Some(value);
            }
        }
    }

    None
}

fn find_secret_string(json: &Value, keys: &[&str]) -> Option<String> {
    match json {
        Value::Object(object) => {
            for key in keys {
                if let Some(value) = object.get(*key).and_then(Value::as_str) {
                    if !value.trim().is_empty() {
                        return Some(value.to_string());
                    }
                }
            }

            for value in object.values() {
                if let Some(secret) = find_secret_string(value, keys) {
                    return Some(secret);
                }
            }

            None
        }
        Value::Array(values) => values
            .iter()
            .find_map(|value| find_secret_string(value, keys)),
        _ => None,
    }
}

fn keychain_item_exists(service: &str) -> bool {
    if !cfg!(target_os = "macos") {
        return false;
    }

    Command::new("security")
        .args(["find-generic-password", "-s", service])
        .output()
        .map(|output| output.status.success())
        .unwrap_or(false)
}

fn load_claude_keychain_access_token(service: &str) -> Result<SecretString, CredentialLoadError> {
    let payload = read_keychain_secret_payload(service)?;
    claude_access_token_from_keychain_payload(service, &payload)
}

fn read_keychain_secret_payload(service: &str) -> Result<String, CredentialLoadError> {
    let output = Command::new("security")
        .args(["find-generic-password", "-s", service, "-w"])
        .output()
        .map_err(|err| CredentialLoadError::KeychainLookupFailed {
            service: service.to_string(),
            stderr: err.to_string(),
        })?;

    if !output.status.success() {
        return Err(CredentialLoadError::KeychainLookupFailed {
            service: service.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    let payload = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if payload.is_empty() {
        return Err(CredentialLoadError::MissingSecret {
            provider: ProviderKind::Claude,
            source: CredentialSource::MacosKeychain {
                service: service.to_string(),
            },
        });
    }

    Ok(payload)
}

fn claude_access_token_from_keychain_payload(
    service: &str,
    payload: &str,
) -> Result<SecretString, CredentialLoadError> {
    let json: Value =
        serde_json::from_str(payload).map_err(|_| CredentialLoadError::InvalidKeychainJson {
            service: service.to_string(),
        })?;

    let access_token = json
        .pointer("/claudeAiOauth/accessToken")
        .and_then(Value::as_str)
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| CredentialLoadError::MissingSecret {
            provider: ProviderKind::Claude,
            source: CredentialSource::MacosKeychain {
                service: service.to_string(),
            },
        })?;

    Ok(SecretString::from(access_token.to_string()))
}

fn display_path_safe(path: &Path, home_dir: &Path) -> String {
    if let Ok(relative) = path.strip_prefix(home_dir) {
        return format!("~/{}", relative.display());
    }

    path.display().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn detects_codex_auth_from_default_home_path() {
        let temp = TempDir::new("codex-default");
        let codex_dir = temp.path().join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        fs::write(
            codex_dir.join("auth.json"),
            r#"{"email":"dev@example.com","access_token":"secret"}"#,
        )
        .unwrap();

        let detector = AuthDetector::new(temp.path().to_path_buf(), None);
        let credential = detector.detect_codex().unwrap();

        assert_eq!(credential.provider, ProviderKind::Codex);
        assert_eq!(credential.provider_id.0, "codex");
        assert_eq!(
            credential.identity_hint,
            Some("dev@example.com".to_string())
        );
        assert_eq!(
            credential.source.display_safe(detector.home_dir()),
            "file:~/.codex/auth.json"
        );
    }

    #[test]
    fn detects_codex_auth_from_codex_home_override() {
        let temp = TempDir::new("codex-home");
        let codex_home = temp.path().join("custom-codex");
        fs::create_dir_all(&codex_home).unwrap();
        fs::write(
            codex_home.join("auth.json"),
            r#"{"user":{"email":"me@test.dev"}}"#,
        )
        .unwrap();

        let detector = AuthDetector::new(temp.path().to_path_buf(), Some(codex_home.clone()));
        let credential = detector.detect_codex().unwrap();

        assert_eq!(credential.identity_hint, Some("me@test.dev".to_string()));
        assert_eq!(
            credential.source,
            CredentialSource::File {
                path: codex_home.join("auth.json")
            }
        );
    }

    #[test]
    fn missing_codex_auth_returns_none() {
        let temp = TempDir::new("codex-missing");
        let detector = AuthDetector::new(temp.path().to_path_buf(), None);

        assert!(detector.detect_codex().is_none());
    }

    #[test]
    fn invalid_codex_auth_returns_none() {
        let temp = TempDir::new("codex-invalid");
        let codex_dir = temp.path().join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        fs::write(codex_dir.join("auth.json"), "not json").unwrap();

        let detector = AuthDetector::new(temp.path().to_path_buf(), None);

        assert!(detector.detect_codex().is_none());
    }

    #[test]
    fn loads_codex_secret_only_at_runtime() {
        let temp = TempDir::new("codex-runtime");
        let codex_dir = temp.path().join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        fs::write(
            codex_dir.join("auth.json"),
            r#"{"email":"dev@example.com","access_token":"secret-token-value"}"#,
        )
        .unwrap();

        let detector = AuthDetector::new(temp.path().to_path_buf(), None);
        let reference = detector.detect_codex().unwrap();
        let runtime = RuntimeCredentialLoader::load(&reference).unwrap();

        assert_eq!(runtime.provider(), ProviderKind::Codex);
        assert_eq!(runtime.expose_secret(), "secret-token-value");
    }

    #[test]
    fn runtime_credential_debug_redacts_secret() {
        let runtime = RuntimeCredential {
            provider: ProviderKind::Codex,
            secret: SecretString::from("super-secret-token".to_string()),
        };

        let debug = format!("{runtime:?}");

        assert!(debug.contains("RuntimeCredential"));
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("super-secret-token"));
    }

    #[test]
    fn runtime_credential_converts_to_redacted_provider_auth() {
        let runtime = RuntimeCredential {
            provider: ProviderKind::Claude,
            secret: SecretString::from("claude-session-secret".to_string()),
        };

        let provider_auth = runtime.into_provider_auth();
        let debug = format!("{provider_auth:?}");

        assert_eq!(provider_auth.kind(), ProviderAuthKind::SessionToken);
        assert_eq!(provider_auth.expose_secret(), "claude-session-secret");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("claude-session-secret"));
    }

    #[test]
    fn claude_keychain_payload_exposes_only_access_token() {
        let payload = r#"{
            "claudeAiOauth": {
                "accessToken": "claude-access-token",
                "refreshToken": "claude-refresh-token",
                "expiresAt": 1770000000000
            },
            "organizationUuid": "org_123"
        }"#;

        let secret =
            claude_access_token_from_keychain_payload(CLAUDE_CODE_KEYCHAIN_SERVICE, payload)
                .unwrap();

        assert_eq!(secret.expose_secret(), "claude-access-token");
        assert_ne!(secret.expose_secret(), "claude-refresh-token");
    }

    #[test]
    fn claude_keychain_payload_errors_when_access_token_is_missing() {
        let payload = r#"{"claudeAiOauth":{"refreshToken":"refresh-only"}}"#;
        let error =
            claude_access_token_from_keychain_payload(CLAUDE_CODE_KEYCHAIN_SERVICE, payload)
                .unwrap_err();

        assert!(matches!(
            error,
            CredentialLoadError::MissingSecret {
                provider: ProviderKind::Claude,
                ..
            }
        ));
    }

    #[test]
    fn claude_keychain_payload_errors_when_json_is_invalid() {
        let error =
            claude_access_token_from_keychain_payload(CLAUDE_CODE_KEYCHAIN_SERVICE, "not-json")
                .unwrap_err();

        assert!(matches!(
            error,
            CredentialLoadError::InvalidKeychainJson { .. }
        ));
    }

    #[test]
    fn codex_runtime_loader_finds_nested_secret_fields() {
        let temp = TempDir::new("codex-nested-secret");
        let codex_dir = temp.path().join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        fs::write(
            codex_dir.join("auth.json"),
            r#"{"tokens":{"refresh_token":"nested-secret"}}"#,
        )
        .unwrap();

        let detector = AuthDetector::new(temp.path().to_path_buf(), None);
        let reference = detector.detect_codex().unwrap();
        let runtime = RuntimeCredentialLoader::load(&reference).unwrap();

        assert_eq!(runtime.expose_secret(), "nested-secret");
    }

    #[test]
    fn loads_copilot_oauth_token_from_editor_cache() {
        let temp = TempDir::new("copilot-cache");
        let config_dir = temp.path().join(".config/github-copilot");
        fs::create_dir_all(&config_dir).unwrap();
        let apps_path = config_dir.join("apps.json");
        fs::write(
            &apps_path,
            r#"{
                "github.com:Iv1.b507a08c87ecfe98": {
                    "oauth_token": "copilot-oauth-token",
                    "user": "dev@example.com"
                }
            }"#,
        )
        .unwrap();

        let secret = load_copilot_oauth_token(&apps_path).unwrap();
        let detector = AuthDetector::new(temp.path().to_path_buf(), None);
        let credential = detector.detect_copilot().unwrap();

        assert_eq!(secret.expose_secret(), "copilot-oauth-token");
        assert_eq!(credential.provider, ProviderKind::Copilot);
        assert_eq!(
            credential.identity_hint,
            Some("dev@example.com".to_string())
        );
    }

    #[test]
    fn copilot_cache_errors_when_token_is_missing() {
        let temp = TempDir::new("copilot-missing-token");
        let apps_path = temp.path().join("apps.json");
        fs::write(
            &apps_path,
            r#"{"github.com:Iv1.b507a08c87ecfe98":{"user":"dev@example.com"}}"#,
        )
        .unwrap();

        let error = load_copilot_oauth_token(&apps_path).unwrap_err();

        assert!(matches!(
            error,
            CredentialLoadError::MissingSecret {
                provider: ProviderKind::Copilot,
                ..
            }
        ));
    }

    #[test]
    fn codex_runtime_loader_errors_when_secret_field_is_missing() {
        let temp = TempDir::new("codex-no-secret");
        let codex_dir = temp.path().join(".codex");
        fs::create_dir_all(&codex_dir).unwrap();
        fs::write(
            codex_dir.join("auth.json"),
            r#"{"email":"dev@example.com"}"#,
        )
        .unwrap();

        let detector = AuthDetector::new(temp.path().to_path_buf(), None);
        let reference = detector.detect_codex().unwrap();
        let error = RuntimeCredentialLoader::load(&reference).unwrap_err();

        assert!(matches!(
            error,
            CredentialLoadError::MissingSecret {
                provider: ProviderKind::Codex,
                ..
            }
        ));
    }

    #[test]
    fn safe_file_display_uses_tilde_for_home_path() {
        let home = PathBuf::from("/Users/tester");
        let source = CredentialSource::File {
            path: home.join(".codex/auth.json"),
        };

        assert_eq!(source.display_safe(&home), "file:~/.codex/auth.json");
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
            let path = std::env::temp_dir().join(format!("inductor-auth-{label}-{nanos}"));
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
