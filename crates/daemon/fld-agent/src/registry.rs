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
        for (id, name, adapter, resume, hook_support, command) in [
            (
                "claude",
                "Claude Code",
                AdapterKind::ClaudeStream,
                ResumeStrategy::ClaudeSession,
                true,
                "claude",
            ),
            (
                "codex",
                "Codex",
                AdapterKind::CodexAppServer,
                ResumeStrategy::CodexThread,
                true,
                "codex",
            ),
            (
                "cursor",
                "Cursor",
                AdapterKind::CursorStreamJson,
                ResumeStrategy::CursorChat,
                false,
                "cursor-agent",
            ),
            (
                "pi",
                "Pi",
                AdapterKind::PiRpc,
                ResumeStrategy::PiSession,
                true,
                "pi",
            ),
            (
                "omp",
                "Oh My Pi",
                AdapterKind::PiRpc,
                ResumeStrategy::PiSession,
                true,
                "omp",
            ),
        ] {
            entries.insert(
                id.into(),
                ProviderSpec {
                    id: id.into(),
                    name: name.into(),
                    command: PathBuf::from(command),
                    args: Vec::new(),
                    env: BTreeMap::new(),
                    adapter,
                    resume,
                    hook_support,
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
            | (AdapterKind::Acp, ResumeStrategy::AcpSession)
            | (AdapterKind::CursorStreamJson, ResumeStrategy::CursorChat)
            | (AdapterKind::PiRpc, ResumeStrategy::PiSession)
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
        "--mode",
        "--stream-partial-output",
        "--session",
        "--session-dir",
        "--no-session",
        "--model",
        "--force",
        "-f",
        "--yolo",
        "--auto-review",
        "--sandbox",
        "--approve-mcps",
        "--trust",
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
    let kind = result.provider.adapter;
    let attempt: Result<()> = async {
        validate_provider(&result.provider)?;
        match kind {
            AdapterKind::ClaudeStream | AdapterKind::CodexAppServer => {
                discover_core(&mut result).await
            }
            AdapterKind::Acp => {
                let provider = result.provider.clone();
                crate::acp::discover(&provider, &mut result).await
            }
            AdapterKind::CursorStreamJson => {
                let provider = result.provider.clone();
                crate::cursor::discover(&provider, &mut result).await
            }
            AdapterKind::PiRpc => {
                let provider = result.provider.clone();
                crate::pi::discover(&provider, &mut result).await
            }
        }
    }
    .await;
    if let Err(error) = attempt {
        result.unavailable_reason = Some(error.to_string());
    }
    result
}

async fn discover_core(result: &mut ProviderDiscovery) -> Result<()> {
    result.version = Some(
        process::capture(&result.provider, &["--version"])
            .await?
            .trim()
            .to_owned(),
    );
    let args: &[&str] = match result.provider.adapter {
        AdapterKind::ClaudeStream => &["--help"],
        AdapterKind::CodexAppServer => &["app-server", "--help"],
        _ => {
            return Err(AdapterError::Unsupported(
                "legacy discovery called for a non-core adapter".into(),
            ));
        }
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
        _ => &[],
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
    result.capabilities = Capabilities {
        chat: true,
        resume: true,
        steer: true,
        approvals: true,
        questions: true,
        partial_messages: true,
    };
    Ok(())
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
        assert_eq!(registry.providers().count(), 5);
    }
    #[test]
    fn builtins_cover_cursor_pi_and_omp_with_matching_strategies() {
        let registry = ProviderRegistry::default();
        for (id, adapter, resume, command) in [
            (
                "cursor",
                AdapterKind::CursorStreamJson,
                ResumeStrategy::CursorChat,
                "cursor-agent",
            ),
            ("pi", AdapterKind::PiRpc, ResumeStrategy::PiSession, "pi"),
            ("omp", AdapterKind::PiRpc, ResumeStrategy::PiSession, "omp"),
        ] {
            let provider = registry.get(id).unwrap();
            assert_eq!(provider.adapter, adapter);
            assert_eq!(provider.resume, resume);
            assert_eq!(provider.command, PathBuf::from(command));
            assert!(provider.args.is_empty());
        }
        // ACP is always a configured provider, never a guessed binary name.
        assert!(registry.get("opencode").is_none());
        assert!(registry.get("hermes").is_none());
    }
    #[test]
    fn acp_providers_validate_and_keep_user_arguments() {
        let mut registry = ProviderRegistry::default();
        let provider = ProviderSpec {
            id: "opencode".into(),
            name: "OpenCode".into(),
            command: "opencode".into(),
            args: vec!["acp".into()],
            env: Default::default(),
            adapter: AdapterKind::Acp,
            resume: ResumeStrategy::AcpSession,
            hook_support: false,
        };
        registry.override_provider(provider.clone()).unwrap();
        assert_eq!(registry.get("opencode"), Some(&provider));
        let mut wrong = provider;
        wrong.resume = ResumeStrategy::PiSession;
        assert!(validate_provider(&wrong).is_err());
    }
    #[test]
    fn new_adapter_protocol_flags_cannot_be_shadowed() {
        let registry = ProviderRegistry::default();
        for (id, arg) in [
            ("cursor", "--output-format=text"),
            ("cursor", "--force"),
            ("cursor", "-p"),
            ("cursor", "--resume"),
            ("pi", "--mode"),
            ("pi", "--session"),
            ("omp", "--no-session"),
            ("omp", "--model=x"),
        ] {
            let mut provider = registry.get(id).unwrap().clone();
            provider.args.push(arg.into());
            assert!(
                validate_provider(&provider).is_err(),
                "{id} accepted reserved {arg}"
            );
        }
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
