//! Phase 6: macOS `sandbox-exec` profile builder.
//!
//! Builds a Seatbelt (`.sb`) policy that denies by default, allows broad
//! filesystem reads, but only allows writes under a set of explicit roots
//! (the workspace and the system tempdir). Optionally denies network access.
//!
//! On non-macOS targets this is a no-op: [`SandboxPolicy::wrap_shell_command`]
//! returns the plain command so callers behave identically everywhere. Linux
//! sandboxing (landlock / bwrap) is a later phase per the plan.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How `bash`-style commands should be confined.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SandboxPolicy {
    /// Run commands without any confinement.
    Disabled,
    /// Confine writes to `write_roots` and optionally deny network.
    Restricted {
        write_roots: Vec<PathBuf>,
        deny_network: bool,
    },
}

impl SandboxPolicy {
    /// The default workspace policy: writes limited to the workspace and the
    /// system tempdir, network denied. On non-macOS this still records the
    /// intent but has no enforcement effect.
    pub fn workspace_default(workspace_root: impl AsRef<Path>) -> Self {
        let mut write_roots = vec![workspace_root.as_ref().to_path_buf()];
        // Many tools need a scratch area; allow the canonical tempdir.
        if let Ok(tmp) = std::env::temp_dir().canonicalize() {
            write_roots.push(tmp);
        }
        Self::Restricted {
            write_roots,
            deny_network: true,
        }
    }

    /// Whether this policy actually enforces anything on the current platform.
    pub fn is_enforced(&self) -> bool {
        cfg!(target_os = "macos") && matches!(self, Self::Restricted { .. })
    }

    /// Wrap a shell command so it runs confined.
    ///
    /// Returns `(program, args)` ready for `Command::new(program).args(args)`.
    /// On macOS with a `Restricted` policy this is
    /// `sandbox-exec -p <profile> sh -lc <command>`; otherwise it is the plain
    /// `sh -lc <command>`.
    pub fn wrap_shell_command(&self, command: &str) -> (String, Vec<String>) {
        match self {
            Self::Restricted {
                write_roots,
                deny_network,
            } if cfg!(target_os = "macos") => {
                let profile = build_seatbelt_profile(write_roots, *deny_network);
                (
                    "sandbox-exec".to_string(),
                    vec![
                        "-p".to_string(),
                        profile,
                        "sh".to_string(),
                        "-lc".to_string(),
                        command.to_string(),
                    ],
                )
            }
            _ => (
                "sh".to_string(),
                vec!["-lc".to_string(), command.to_string()],
            ),
        }
    }
}

/// Build a Seatbelt profile string for `sandbox-exec -p`.
///
/// Deny-by-default, allow process exec + broad reads, allow writes only under
/// the given roots (plus the standard char devices), and deny network when
/// requested.
pub fn build_seatbelt_profile(write_roots: &[PathBuf], deny_network: bool) -> String {
    let mut profile = String::new();
    profile.push_str("(version 1)\n");
    profile.push_str("(deny default)\n");
    // Allow programs to launch and basic introspection they need to run.
    profile.push_str("(allow process-exec)\n");
    profile.push_str("(allow process-fork)\n");
    profile.push_str("(allow signal (target self))\n");
    profile.push_str("(allow sysctl-read)\n");
    profile.push_str("(allow mach-lookup)\n");
    // Reads are broadly permitted; the confinement is on writes.
    profile.push_str("(allow file-read*)\n");

    if deny_network {
        profile.push_str("(deny network*)\n");
    } else {
        profile.push_str("(allow network*)\n");
    }

    // Writes: only under the explicit roots, plus the standard char devices
    // that ordinary commands expect to write to.
    profile.push_str("(allow file-write*\n");
    profile.push_str("    (literal \"/dev/null\")\n");
    profile.push_str("    (literal \"/dev/stdout\")\n");
    profile.push_str("    (literal \"/dev/stderr\")\n");
    profile.push_str("    (literal \"/dev/tty\")\n");
    for root in write_roots {
        profile.push_str(&format!("    (subpath {})\n", quote_sb_path(root)));
    }
    profile.push_str(")\n");

    profile
}

/// Quote a path as a Seatbelt string literal, escaping backslashes and quotes.
fn quote_sb_path(path: &Path) -> String {
    let raw = path.to_string_lossy();
    let escaped = raw.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_denies_by_default_and_scopes_writes() {
        let roots = vec![PathBuf::from("/tmp/workspace")];
        let profile = build_seatbelt_profile(&roots, true);

        assert!(profile.contains("(deny default)"));
        assert!(profile.contains("(allow file-read*)"));
        assert!(profile.contains("(deny network*)"));
        assert!(profile.contains("(subpath \"/tmp/workspace\")"));
    }

    #[test]
    fn profile_can_allow_network() {
        let profile = build_seatbelt_profile(&[PathBuf::from("/ws")], false);
        assert!(profile.contains("(allow network*)"));
        assert!(!profile.contains("(deny network*)"));
    }

    #[test]
    fn disabled_policy_returns_plain_shell() {
        let (program, args) = SandboxPolicy::Disabled.wrap_shell_command("echo hi");
        assert_eq!(program, "sh");
        assert_eq!(args, vec!["-lc", "echo hi"]);
    }

    #[test]
    fn restricted_policy_wraps_on_macos() {
        let policy = SandboxPolicy::Restricted {
            write_roots: vec![PathBuf::from("/ws")],
            deny_network: true,
        };
        let (program, args) = policy.wrap_shell_command("echo hi");

        if cfg!(target_os = "macos") {
            assert_eq!(program, "sandbox-exec");
            assert_eq!(args[0], "-p");
            assert!(args[1].contains("(deny default)"));
            assert_eq!(args[args.len() - 1], "echo hi");
        } else {
            assert_eq!(program, "sh");
        }
    }

    #[test]
    fn path_quoting_escapes_special_characters() {
        let quoted = quote_sb_path(Path::new("/tmp/a\"b"));
        assert_eq!(quoted, "\"/tmp/a\\\"b\"");
    }
}
