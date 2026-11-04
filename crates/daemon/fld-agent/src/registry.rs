use crate::*;
use std::{collections::BTreeMap, path::PathBuf};

/// Built-ins plus complete user overrides keyed by stable provider ID.
#[derive(Debug, Clone)]
pub struct ProviderRegistry {
    entries: BTreeMap<String, ProviderSpec>,
}
impl Default for ProviderRegistry {
    fn default() -> Self {
        let mut entries = BTreeMap::new();
        for (id, name, adapter, resume) in [
            (
                "claude",
                "Claude Code",
                AdapterKind::ClaudeStream,
                ResumeStrategy::ClaudeSession,
            ),
            (
                "codex",
                "Codex",
                AdapterKind::CodexAppServer,
                ResumeStrategy::CodexThread,
            ),
        ] {
            entries.insert(
                id.into(),
                ProviderSpec {
                    id: id.into(),
                    name: name.into(),
                    command: PathBuf::from(id),
                    args: Vec::new(),
                    env: BTreeMap::new(),
                    adapter,
                    resume,
                    hook_support: true,
                },
            );
        }
        Self { entries }
    }
}
impl ProviderRegistry {
    pub fn override_provider(&mut self, provider: ProviderSpec) -> Result<()> {
        validate_provider(&provider)?;
        self.entries.insert(provider.id.clone(), provider);
        Ok(())
    }
    pub fn providers(&self) -> impl Iterator<Item = &ProviderSpec> {
        self.entries.values()
    }
    pub fn get(&self, id: &str) -> Option<&ProviderSpec> {
        self.entries.get(id)
    }
    pub async fn discover(&self) -> Vec<ProviderDiscovery> {
        let mut results = Vec::with_capacity(self.entries.len());
        for provider in self.entries.values() {
            results.push(discover(provider.clone()).await);
        }
        results
    }
}

pub(crate) fn validate_provider(p: &ProviderSpec) -> Result<()> {
    if p.id.is_empty() || p.command.as_os_str().is_empty() {
        return Err(AdapterError::InvalidConfig(
            "empty provider identity or command".into(),
        ));
    }
    if !matches!(
        (p.adapter, &p.resume),
        (AdapterKind::ClaudeStream, ResumeStrategy::ClaudeSession)
            | (AdapterKind::CodexAppServer, ResumeStrategy::CodexThread)
    ) {
        return Err(AdapterError::InvalidConfig(
            "resume strategy does not match adapter".into(),
        ));
    }
    const RESERVED: &[&str] = &[
        "--",
        "-p",
        "-r",
        "-c",
        "--print",
        "--continue",
        "--resume",
        "--session-id",
        "--fork-session",
        "--input-format",
        "--output-format",
        "--include-partial-messages",
        "--verbose",
        "--permission-mode",
        "--permission-prompts",
        "--permission-prompt-tool",
        "--dangerously-skip-permissions",
        "--allow-dangerously-skip-permissions",
        "--allowedTools",
        "--allowed-tools",
        "--no-session-persistence",
        "--listen",
        "--stdio",
        "--config",
        "--settings",
    ];
    if p.args.iter().any(|arg| {
        RESERVED.iter().any(|flag| {
            arg == flag
                || arg
                    .strip_prefix(flag)
                    .is_some_and(|suffix| suffix.starts_with('='))
        })
    }) {
        return Err(AdapterError::InvalidConfig(
            "provider arguments override adapter-owned protocol or permission flags".into(),
        ));
    }
    Ok(())
}

async fn discover(provider: ProviderSpec) -> ProviderDiscovery {
    let mut result = ProviderDiscovery {
        provider,
        version: None,
        capabilities: Capabilities {
            chat: false,
            resume: false,
            steer: false,
            approvals: false,
            questions: false,
            partial_messages: false,
        },
        unavailable_reason: None,
    };
    let attempt: Result<()> = async {
        validate_provider(&result.provider)?;
        result.version = Some(
            process::capture(&result.provider, &["--version"])
                .await?
                .trim()
                .to_owned(),
        );
        let args: &[&str] = match result.provider.adapter {
            AdapterKind::ClaudeStream => &["--help"],
            AdapterKind::CodexAppServer => &["app-server", "--help"],
        };
        let help = process::capture(&result.provider, args).await?;
        let required: &[&str] = match result.provider.adapter {
            AdapterKind::ClaudeStream => &[
                "--input-format",
                "--output-format",
                "--include-partial-messages",
                "--resume",
                "--session-id",
                "manual",
            ],
            AdapterKind::CodexAppServer => &["generate-json-schema", "stdio"],
        };
        for flag in required {
            if !help.contains(flag) {
                return Err(AdapterError::Unsupported(format!(
                    "installed CLI help lacks {flag}"
                )));
            }
        }
        if result.provider.adapter == AdapterKind::CodexAppServer {
            process::validate_codex_schema(&result.provider).await?;
        }
        process::probe(&result.provider).await?;
        Ok(())
    }
    .await;
    match attempt {
        Ok(()) => {
            result.capabilities = Capabilities {
                chat: true,
                resume: true,
                steer: true,
                approvals: true,
                questions: true,
                partial_messages: true,
            }
        }
        Err(error) => result.unavailable_reason = Some(error.to_string()),
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn builtins_are_data_and_overrides_keep_identity() {
        let mut registry = ProviderRegistry::default();
        let mut p = registry.get("claude").unwrap().clone();
        p.command = "/opt/claude".into();
        p.env.insert("PROFILE".into(), "work".into());
        registry.override_provider(p.clone()).unwrap();
        assert_eq!(registry.get("claude"), Some(&p));
        assert_eq!(registry.providers().count(), 2);
    }
    #[test]
    fn protocol_and_permission_flags_cannot_be_shadowed() {
        let registry = ProviderRegistry::default();
        for arg in [
            "--permission-mode=bypassPermissions",
            "--resume",
            "--listen=ws://0.0.0.0:1",
            "--dangerously-skip-permissions",
        ] {
            let mut p = registry.get("claude").unwrap().clone();
            p.args.push(arg.into());
            assert!(validate_provider(&p).is_err());
        }
    }
}
