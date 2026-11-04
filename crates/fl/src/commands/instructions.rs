//! `fl instructions` — Flotilla-authored guides embedded in the binary.
//!
//! This command never opens the daemon socket: the text ships with the
//! client so an agent on a broken connection can still read the contract.
use crate::cli::InstructionTopic;
use crate::error::CliError;
use crate::output::Reporter;

/// Every topic, its display title and its embedded markdown.
const TOPICS: [(&str, &str, &str); 5] = [
    (
        "orchestration",
        "Orchestration contract, coordination state, teams and hooks",
        include_str!("../../instructions/orchestration.md"),
    ),
    (
        "layout",
        "Views, tabs, panes, split ratios and composed operations",
        include_str!("../../instructions/layout.md"),
    ),
    (
        "worktree",
        "Worktree state, branches, review threads and scripts",
        include_str!("../../instructions/worktree.md"),
    ),
    (
        "review",
        "App-managed review threads and their trust rules",
        include_str!("../../instructions/review.md"),
    ),
    (
        "browser",
        "Browser tabs and the client-side automation contract",
        include_str!("../../instructions/browser.md"),
    ),
];

pub fn run(reporter: &Reporter, topic: Option<InstructionTopic>) -> Result<(), CliError> {
    let Some(topic) = topic else {
        if reporter.json {
            return reporter.value(&serde_json::json!({
                "topics": TOPICS
                    .iter()
                    .map(|(name, title, _)| serde_json::json!({ "topic": name, "title": title }))
                    .collect::<Vec<_>>(),
            }));
        }
        reporter.text("Instruction guides (fl instructions <topic>):");
        for (name, title, _) in TOPICS {
            reporter.text(format!("  {name:<14} {title}"));
        }
        return Ok(());
    };
    let name = topic.as_str();
    let (_, title, markdown) = TOPICS
        .iter()
        .find(|(candidate, _, _)| *candidate == name)
        .expect("every InstructionTopic has a guide");
    if reporter.json {
        return reporter.value(&serde_json::json!({
            "topic": name,
            "title": title,
            "markdown": markdown,
        }));
    }
    reporter.text(markdown.trim_end());
    Ok(())
}
