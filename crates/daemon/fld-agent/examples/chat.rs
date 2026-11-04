//! Interactive, opt-in real-provider smoke driver. No approval is ever automatic.
use fld_agent::{
    Adapter, ApprovalDecision, PermissionMode, ProviderRegistry, QuestionResponse, SessionConfig,
};
use std::collections::BTreeMap;
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let provider = args
        .next()
        .ok_or("usage: chat <claude|codex> <absolute trusted cwd> [model] [output-schema-json]")?;
    let cwd = args.next().ok_or("missing cwd")?.into();
    let registry = ProviderRegistry::default();
    for discovery in registry.discover().await {
        eprintln!(
            "{}: {:?} {:?}",
            discovery.provider.id, discovery.version, discovery.unavailable_reason
        );
    }
    let spec = registry.get(&provider).ok_or("unknown provider")?.clone();
    let mut adapter = Adapter::launch(
        spec,
        SessionConfig {
            cwd,
            model: args.next(),
            permission_mode: PermissionMode::Ask,
            instructions: None,
            session_id: None,
            provider_config_json: None,
            output_schema_json: args.next(),
            resume: None,
        },
    )
    .await?;
    eprintln!("Session: {:?}", adapter.info);
    eprintln!(
        "Prompt lines send while idle. /steer <text>, /cancel, /approve <id>, /deny <id>, /answer <id> <answers-json>, /quit. No default approval."
    );
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        tokio::select! {
            event = adapter.events.recv() => match event {
                Some(event) => println!("{}", serde_json::to_string(&event)?),
                None => { eprintln!("Adapter ended: {:?}", adapter.state()); break; }
            },
            line = lines.next_line() => {
                let Some(line) = line? else { adapter.shutdown().await?; break; };
                let result = if line == "/quit" { adapter.shutdown().await?; break; }
                else if line == "/cancel" { adapter.cancel().await }
                else if let Some(text) = line.strip_prefix("/steer ") { adapter.steer(text).await }
                else if let Some(id) = line.strip_prefix("/approve ") { adapter.approve(id, ApprovalDecision::AllowOnce).await }
                else if let Some(id) = line.strip_prefix("/deny ") { adapter.approve(id, ApprovalDecision::Deny).await }
                else if let Some(answer) = line.strip_prefix("/answer ") {
                    let (id,json) = answer.split_once(' ').ok_or("/answer requires id and JSON map of question IDs to answer arrays")?;
                    let answers: BTreeMap<String, Vec<String>> = serde_json::from_str(json)?;
                    adapter.respond_question(id,QuestionResponse { answers, structured_json: None }).await
                } else { adapter.send(line).await };
                if let Err(error) = result { eprintln!("Request failed: {error}"); }
            }
        }
    }
    Ok(())
}
