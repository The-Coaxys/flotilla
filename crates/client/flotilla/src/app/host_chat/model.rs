//! Pure chat reductions shared by the chat surface, the decision cards, the
//! inbox and the composer. Nothing here touches GPUI: the transcript, markdown
//! blocks, unified diffs, answer validation, inbox order and the send/steer
//! decision are plain functions so they can be unit tested without a window.
use std::collections::{BTreeMap, HashMap, HashSet};

use fl_model::chat::{
    ApprovalDecision, ChatEvent, ChatSession, EventRecord, InboxItem, InboxState, Question,
    QuestionResponse, SessionState, TextUpdate, ToolCall, ToolKind, ToolStatus, TurnOutcome, Usage,
};

/// Rows parsed from one unified diff block; the daemon already bounds the
/// text, this keeps a pathological patch from shaping millions of rows.
pub const MAX_DIFF_ROWS: usize = 4000;

/// One rendered transcript row. Durable sequence is kept so search hits can
/// scroll to the event that produced the row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptRow {
    pub seq: u64,
    pub at_ms: u64,
    pub kind: RowKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowKind {
    User {
        text: String,
    },
    Assistant {
        id: String,
        text: String,
        /// A `Final` update replaced the accumulated deltas.
        complete: bool,
    },
    Thinking {
        id: String,
        text: String,
    },
    Tool(ToolRow),
    FileEdit {
        path: String,
        diff: String,
        truncated: bool,
    },
    Approval {
        id: String,
        tool: String,
        details: String,
        truncated: bool,
        options: Vec<ApprovalDecision>,
        /// A `RequestResolved` event exists for this request.
        resolved: bool,
    },
    Question {
        id: String,
        questions: Vec<Question>,
        schema_json: Option<String>,
        resolved: bool,
    },
    TurnEnd {
        duration_ms: Option<u64>,
        turns: u64,
        usage: Usage,
        model: Option<String>,
        outcome: TurnOutcome,
    },
    Structured {
        json: String,
    },
    Error {
        code: String,
        message: String,
        fatal: bool,
    },
    ProcessExited {
        code: Option<i32>,
        expected: bool,
    },
    /// A provider hook reported a state fact (never a turn outcome).
    Hook {
        kind: fl_model::coordination::HookFactKind,
        detail: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolRow {
    pub id: String,
    pub kind: ToolKind,
    pub title: String,
    pub input: String,
    pub input_truncated: bool,
    pub status: ToolStatus,
    pub output: Option<String>,
    pub diff: Option<String>,
    /// Subagent events grouped under the tool call that spawned them.
    pub children: Vec<TranscriptRow>,
}

/// Append a streaming update without duplicating a whole cumulative snapshot.
/// Providers send either a fresh delta or the text so far; both converge on the
/// same bubble text.
pub fn merge_stream(existing: &str, incoming: &str) -> String {
    if incoming.is_empty() {
        return existing.to_owned();
    }
    if existing.is_empty() || incoming == existing || incoming.starts_with(existing) {
        return incoming.to_owned();
    }
    let mut merged = String::with_capacity(existing.len() + incoming.len());
    merged.push_str(existing);
    merged.push_str(incoming);
    merged
}

/// Fold durable events into display rows. Events arrive gap-free in sequence
/// order, so a row is updated in place by matching its provider id.
pub fn build_transcript(events: &[EventRecord]) -> Vec<TranscriptRow> {
    build_rows(events, &collect_resolved_ids(events))
}

/// Every request id that any event in this transcript has resolved. Cards use
/// this instead of a local "submitted" flag so a redraw or reconnect cannot
/// resurrect an approval that the daemon already answered.
pub fn collect_resolved_ids(events: &[EventRecord]) -> HashSet<String> {
    let mut resolved = HashSet::new();
    for record in events {
        collect_resolved(&record.event, &mut resolved);
    }
    resolved
}

fn collect_resolved(event: &ChatEvent, out: &mut HashSet<String>) {
    match event {
        ChatEvent::RequestResolved { id } => {
            out.insert(id.clone());
        }
        ChatEvent::Subagent { events, .. } => {
            for event in events {
                collect_resolved(event, out);
            }
        }
        _ => {}
    }
}

fn build_rows(events: &[EventRecord], resolved: &HashSet<String>) -> Vec<TranscriptRow> {
    let mut rows: Vec<TranscriptRow> = Vec::new();
    for record in events {
        let seq = record.sequence;
        let at_ms = record.timestamp_ms;
        match &record.event {
            ChatEvent::UserMessage { text, .. } => rows.push(TranscriptRow {
                seq,
                at_ms,
                kind: RowKind::User { text: text.clone() },
            }),
            ChatEvent::AssistantText { id, update, text } => {
                let index = last_index(
                    &rows,
                    |kind| matches!(kind, RowKind::Assistant { id: known, .. } if known == id),
                );
                match index {
                    Some(index) => {
                        if let RowKind::Assistant {
                            text: known,
                            complete,
                            ..
                        } = &mut rows[index].kind
                        {
                            match update {
                                TextUpdate::Delta => *known = merge_stream(known, text),
                                TextUpdate::Final => {
                                    *known = text.clone();
                                    *complete = true;
                                }
                            }
                        }
                    }
                    None => rows.push(TranscriptRow {
                        seq,
                        at_ms,
                        kind: RowKind::Assistant {
                            id: id.clone(),
                            text: text.clone(),
                            complete: *update == TextUpdate::Final,
                        },
                    }),
                }
            }
            ChatEvent::Thinking { id, delta } => {
                let index = last_index(
                    &rows,
                    |kind| matches!(kind, RowKind::Thinking { id: known, .. } if known == id),
                );
                match index {
                    Some(index) => {
                        if let RowKind::Thinking { text, .. } = &mut rows[index].kind {
                            *text = merge_stream(text, delta);
                        }
                    }
                    None => rows.push(TranscriptRow {
                        seq,
                        at_ms,
                        kind: RowKind::Thinking {
                            id: id.clone(),
                            text: delta.clone(),
                        },
                    }),
                }
            }
            ChatEvent::ToolCall(call) => {
                let index = last_index(
                    &rows,
                    |kind| matches!(kind, RowKind::Tool(tool) if tool.id == call.id),
                );
                match index {
                    Some(index) => {
                        if let RowKind::Tool(tool) = &mut rows[index].kind {
                            apply_call(tool, call);
                        }
                    }
                    None => rows.push(TranscriptRow {
                        seq,
                        at_ms,
                        kind: RowKind::Tool(tool_row(call)),
                    }),
                }
            }
            ChatEvent::ToolOutput { id, delta } => {
                let index = last_index(
                    &rows,
                    |kind| matches!(kind, RowKind::Tool(tool) if &tool.id == id),
                );
                if let Some(index) = index
                    && let RowKind::Tool(tool) = &mut rows[index].kind
                {
                    let existing = tool.output.get_or_insert_with(String::new);
                    *existing = merge_stream(existing, &delta.text);
                }
            }
            ChatEvent::FileEdit { path, unified_diff } => rows.push(TranscriptRow {
                seq,
                at_ms,
                kind: RowKind::FileEdit {
                    path: path.display().to_string(),
                    diff: unified_diff.text.clone(),
                    truncated: unified_diff.truncated,
                },
            }),
            ChatEvent::ApprovalRequest {
                id,
                tool,
                details,
                options,
            } => {
                let index = last_index(
                    &rows,
                    |kind| matches!(kind, RowKind::Approval { id: known, .. } if known == id),
                );
                match index {
                    Some(index) => {
                        if let RowKind::Approval {
                            tool: known_tool,
                            details: known_details,
                            truncated,
                            options: known_options,
                            ..
                        } = &mut rows[index].kind
                        {
                            *known_tool = tool.clone();
                            *known_details = details.text.clone();
                            *truncated = details.truncated;
                            *known_options = options.clone();
                        }
                    }
                    None => rows.push(TranscriptRow {
                        seq,
                        at_ms,
                        kind: RowKind::Approval {
                            id: id.clone(),
                            tool: tool.clone(),
                            details: details.text.clone(),
                            truncated: details.truncated,
                            options: options.clone(),
                            resolved: resolved.contains(id),
                        },
                    }),
                }
            }
            ChatEvent::QuestionRequest {
                id,
                questions,
                schema_json,
            } => {
                let index = last_index(
                    &rows,
                    |kind| matches!(kind, RowKind::Question { id: known, .. } if known == id),
                );
                match index {
                    Some(index) => {
                        if let RowKind::Question {
                            questions: known_questions,
                            schema_json: known_schema,
                            ..
                        } = &mut rows[index].kind
                        {
                            *known_questions = questions.clone();
                            *known_schema = schema_json.clone();
                        }
                    }
                    None => rows.push(TranscriptRow {
                        seq,
                        at_ms,
                        kind: RowKind::Question {
                            id: id.clone(),
                            questions: questions.clone(),
                            schema_json: schema_json.clone(),
                            resolved: resolved.contains(id),
                        },
                    }),
                }
            }
            ChatEvent::Subagent {
                parent_tool_id,
                events,
            } => {
                let children = build_nested(events, resolved, seq, at_ms);
                if children.is_empty() {
                    continue;
                }
                let index = last_index(
                    &rows,
                    |kind| matches!(kind, RowKind::Tool(tool) if &tool.id == parent_tool_id),
                );
                match index {
                    Some(index) => {
                        if let RowKind::Tool(tool) = &mut rows[index].kind {
                            tool.children.extend(children);
                        }
                    }
                    None => rows.push(TranscriptRow {
                        seq,
                        at_ms,
                        kind: RowKind::Tool(ToolRow {
                            id: parent_tool_id.clone(),
                            kind: ToolKind::Other,
                            title: "Subagent".into(),
                            input: String::new(),
                            input_truncated: false,
                            status: ToolStatus::Completed,
                            output: None,
                            diff: None,
                            children,
                        }),
                    }),
                }
            }
            ChatEvent::TurnEnd {
                duration_ms,
                turns,
                usage,
                model,
                outcome,
            } => rows.push(TranscriptRow {
                seq,
                at_ms,
                kind: RowKind::TurnEnd {
                    duration_ms: *duration_ms,
                    turns: *turns,
                    usage: usage.clone(),
                    model: model.clone(),
                    outcome: outcome.clone(),
                },
            }),
            ChatEvent::StructuredOutput { json } => rows.push(TranscriptRow {
                seq,
                at_ms,
                kind: RowKind::Structured { json: json.clone() },
            }),
            ChatEvent::Error {
                code,
                message,
                fatal,
            } => rows.push(TranscriptRow {
                seq,
                at_ms,
                kind: RowKind::Error {
                    code: code.clone(),
                    message: message.clone(),
                    fatal: *fatal,
                },
            }),
            ChatEvent::ProcessExited { code, expected } => rows.push(TranscriptRow {
                seq,
                at_ms,
                kind: RowKind::ProcessExited {
                    code: *code,
                    expected: *expected,
                },
            }),
            ChatEvent::HookFact(fact) => rows.push(TranscriptRow {
                seq,
                at_ms,
                kind: RowKind::Hook {
                    kind: fact.kind,
                    detail: fact.detail.clone(),
                },
            }),
            ChatEvent::RequestResolved { .. }
            | ChatEvent::SessionReady(_)
            | ChatEvent::StateChanged(_)
            | ChatEvent::Diagnostic { .. } => {}
        }
    }
    rows
}

/// Subagent events carry no sequence of their own; they inherit the parent
/// record's position so one durable record still maps to one transcript area.
fn build_nested(
    events: &[ChatEvent],
    resolved: &HashSet<String>,
    seq: u64,
    at_ms: u64,
) -> Vec<TranscriptRow> {
    let records: Vec<EventRecord> = events
        .iter()
        .map(|event| EventRecord {
            schema_version: 1,
            session_id: uuid::Uuid::nil(),
            incarnation: uuid::Uuid::nil(),
            sequence: seq,
            timestamp_ms: at_ms,
            native_session_id: None,
            turn_id: None,
            event: event.clone(),
        })
        .collect();
    build_rows(&records, resolved)
}

/// Index of the last row whose kind matches. Returning an index (not a
/// mutable borrow) keeps `rows` free for updates and pushes in the same arm.
fn last_index(rows: &[TranscriptRow], matches: impl Fn(&RowKind) -> bool) -> Option<usize> {
    rows.iter().rposition(|row| matches(&row.kind))
}

fn tool_row(call: &ToolCall) -> ToolRow {
    ToolRow {
        id: call.id.clone(),
        kind: call.kind.clone(),
        title: call.title.clone(),
        input: call.input_summary.text.clone(),
        input_truncated: call.input_summary.truncated,
        status: call.status.clone(),
        output: call.output.as_ref().map(|text| text.text.clone()),
        diff: call.diff.as_ref().map(|text| text.text.clone()),
        children: Vec::new(),
    }
}

fn apply_call(tool: &mut ToolRow, call: &ToolCall) {
    tool.kind = call.kind.clone();
    tool.title = call.title.clone();
    tool.input = call.input_summary.text.clone();
    tool.input_truncated = call.input_summary.truncated;
    tool.status = call.status.clone();
    if let Some(output) = &call.output {
        // A later tool update carries the output so far; merge it the same way
        // as streamed output deltas so neither can duplicate the other.
        let existing = tool.output.get_or_insert_with(String::new);
        *existing = merge_stream(existing, &output.text);
    }
    if let Some(diff) = &call.diff {
        tool.diff = Some(diff.text.clone());
    }
}

// --- markdown --------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkdownBlock {
    Heading {
        level: u8,
        text: String,
    },
    Paragraph(String),
    List {
        ordered: bool,
        items: Vec<String>,
    },
    Code {
        language: Option<String>,
        text: String,
    },
}

/// The subset of markdown assistant messages actually use here: ATX headings,
/// paragraphs, bullet/numbered lists and fenced code. Inline code is handled by
/// [`inline_spans`].
pub fn parse_markdown(source: &str) -> Vec<MarkdownBlock> {
    let mut blocks = Vec::new();
    let mut paragraph = String::new();
    let mut list: Option<(bool, Vec<String>)> = None;
    let mut lines = source.lines();

    fn flush_paragraph(paragraph: &mut String, blocks: &mut Vec<MarkdownBlock>) {
        let text = paragraph.trim();
        if !text.is_empty() {
            blocks.push(MarkdownBlock::Paragraph(text.to_owned()));
        }
        paragraph.clear();
    }
    fn flush_list(list: &mut Option<(bool, Vec<String>)>, blocks: &mut Vec<MarkdownBlock>) {
        if let Some((ordered, items)) = list.take()
            && !items.is_empty()
        {
            blocks.push(MarkdownBlock::List { ordered, items });
        }
    }

    while let Some(line) = lines.next() {
        let trimmed = line.trim_end();
        if let Some(info) = trimmed.trim_start().strip_prefix("```") {
            flush_paragraph(&mut paragraph, &mut blocks);
            flush_list(&mut list, &mut blocks);
            let language = info.trim();
            let mut code = String::new();
            for body in lines.by_ref() {
                if body.trim_start().starts_with("```") {
                    break;
                }
                code.push_str(body);
                code.push('\n');
            }
            blocks.push(MarkdownBlock::Code {
                language: (!language.is_empty()).then(|| language.to_owned()),
                text: code.trim_end_matches('\n').to_owned(),
            });
            continue;
        }
        if trimmed.trim().is_empty() {
            flush_paragraph(&mut paragraph, &mut blocks);
            flush_list(&mut list, &mut blocks);
            continue;
        }
        if let Some((level, text)) = heading(trimmed) {
            flush_paragraph(&mut paragraph, &mut blocks);
            flush_list(&mut list, &mut blocks);
            blocks.push(MarkdownBlock::Heading {
                level,
                text: text.to_owned(),
            });
            continue;
        }
        if let Some((ordered, item)) = list_item(trimmed) {
            flush_paragraph(&mut paragraph, &mut blocks);
            let continues = matches!(&list, Some((known, _)) if *known == ordered);
            if continues {
                if let Some((_, items)) = &mut list {
                    items.push(item.to_owned());
                }
            } else {
                flush_list(&mut list, &mut blocks);
                list = Some((ordered, vec![item.to_owned()]));
            }
            continue;
        }
        flush_list(&mut list, &mut blocks);
        if !paragraph.is_empty() {
            paragraph.push(' ');
        }
        paragraph.push_str(trimmed.trim());
    }
    flush_paragraph(&mut paragraph, &mut blocks);
    flush_list(&mut list, &mut blocks);
    blocks
}

fn heading(line: &str) -> Option<(u8, &str)> {
    let hashes = line.bytes().take_while(|byte| *byte == b'#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &line[hashes..];
    if rest.is_empty() {
        return Some((hashes as u8, ""));
    }
    let rest = rest.strip_prefix(' ')?;
    Some((hashes as u8, rest.trim()))
}

fn list_item(line: &str) -> Option<(bool, &str)> {
    let trimmed = line.trim_start();
    if let Some(item) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
        .or_else(|| trimmed.strip_prefix("+ "))
    {
        return Some((false, item.trim()));
    }
    let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0 {
        let rest = &trimmed[digits..];
        if let Some(item) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return Some((true, item.trim()));
        }
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineSpan {
    pub text: String,
    pub code: bool,
}

/// Split a line into plain and `code` spans. An unmatched backtick is literal.
pub fn inline_spans(text: &str) -> Vec<InlineSpan> {
    let mut spans = Vec::new();
    let mut plain = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('`') {
        plain.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        match after.find('`') {
            Some(end) => {
                if !plain.is_empty() {
                    spans.push(InlineSpan {
                        text: std::mem::take(&mut plain),
                        code: false,
                    });
                }
                spans.push(InlineSpan {
                    text: after[..end].to_owned(),
                    code: true,
                });
                rest = &after[end + 1..];
            }
            None => {
                plain.push('`');
                rest = after;
            }
        }
    }
    plain.push_str(rest);
    if !plain.is_empty() {
        spans.push(InlineSpan {
            text: plain,
            code: false,
        });
    }
    spans
}

// --- unified diffs ---------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    /// `diff --git`, `index`, `---`, `+++`
    File,
    /// `@@ … @@`
    Hunk,
    Added,
    Removed,
    Context,
    /// `\ No newline at end of file`
    Note,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffRow {
    pub kind: DiffKind,
    pub old: Option<u64>,
    pub new: Option<u64>,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedDiff {
    pub rows: Vec<DiffRow>,
    pub truncated: bool,
}

/// Parse a unified diff (as providers emit it) into numbered rows.
pub fn parse_unified_diff(source: &str) -> ParsedDiff {
    #[derive(Default)]
    struct Cursor {
        old: u64,
        new: u64,
    }
    let mut parsed = ParsedDiff::default();
    let mut cursor: Option<Cursor> = None;
    for line in source.lines() {
        if parsed.rows.len() >= MAX_DIFF_ROWS {
            parsed.truncated = true;
            break;
        }
        if let Some((old, new)) = hunk_header(line) {
            let cursor = cursor.get_or_insert_with(Cursor::default);
            cursor.old = old;
            cursor.new = new;
            parsed.rows.push(DiffRow {
                kind: DiffKind::Hunk,
                old: None,
                new: None,
                text: line.trim_end().to_owned(),
            });
            continue;
        }
        if line.starts_with("diff --git")
            || line.starts_with("index ")
            || line.starts_with("--- ")
            || line.starts_with("+++ ")
        {
            parsed.rows.push(DiffRow {
                kind: DiffKind::File,
                old: None,
                new: None,
                text: line.trim_end().to_owned(),
            });
            continue;
        }
        if line.starts_with("\\ ") {
            parsed.rows.push(DiffRow {
                kind: DiffKind::Note,
                old: None,
                new: None,
                text: line.trim_end().to_owned(),
            });
            continue;
        }
        let Some(cursor) = cursor.as_mut() else {
            continue;
        };
        let (kind, old, new) = match line.as_bytes().first() {
            Some(b'+') => {
                let number = cursor.new;
                cursor.new += 1;
                (DiffKind::Added, None, Some(number))
            }
            Some(b'-') => {
                let number = cursor.old;
                cursor.old += 1;
                (DiffKind::Removed, Some(number), None)
            }
            Some(b' ') | None => {
                let (old, new) = (cursor.old, cursor.new);
                cursor.old += 1;
                cursor.new += 1;
                (DiffKind::Context, Some(old), Some(new))
            }
            _ => continue,
        };
        parsed.rows.push(DiffRow {
            kind,
            old,
            new,
            text: line.get(1..).unwrap_or_default().trim_end().to_owned(),
        });
    }
    parsed
}

fn hunk_header(line: &str) -> Option<(u64, u64)> {
    let rest = line.strip_prefix("@@ ")?;
    let mut parts = rest.split(' ');
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let start = |range: &str| -> Option<u64> {
        let first = range.split(',').next()?;
        first.parse().ok()
    };
    Some((start(old)?, start(new)?))
}

// --- composer and decisions ------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitAction {
    Send,
    Steer,
}

/// Which request the composer submits for the session's current state. `None`
/// means the composer accepts no submission until the state changes.
pub fn composer_action(status: &SessionState) -> Option<SubmitAction> {
    match status {
        SessionState::Idle => Some(SubmitAction::Send),
        SessionState::Running => Some(SubmitAction::Steer),
        _ => None,
    }
}

/// A running (or starting/blocked) turn can be interrupted; idle and ended
/// sessions cannot.
pub fn can_stop(status: &SessionState) -> bool {
    matches!(
        status,
        SessionState::Starting | SessionState::Running | SessionState::AwaitingInput
    )
}

pub fn is_running(status: &SessionState) -> bool {
    matches!(status, SessionState::Running)
}

/// A decision request is actionable only when the daemon is live, no
/// resolution event has arrived and this client has no submission in flight.
pub fn decision_ready(live: bool, resolved: bool, inflight: bool) -> bool {
    live && !resolved && !inflight
}

pub fn approval_label(decision: ApprovalDecision) -> &'static str {
    match decision {
        ApprovalDecision::AllowOnce => "Allow once",
        ApprovalDecision::Deny => "Deny",
        ApprovalDecision::Cancel => "Cancel",
    }
}

pub fn tool_icon(kind: &ToolKind) -> &'static str {
    match kind {
        ToolKind::Read => "▤",
        ToolKind::Edit => "✎",
        ToolKind::Exec => "▶",
        ToolKind::Search => "⌕",
        ToolKind::Web => "◇",
        ToolKind::Mcp => "⚙",
        ToolKind::Other => "•",
    }
}

pub fn tool_kind_label(kind: &ToolKind) -> &'static str {
    match kind {
        ToolKind::Read => "Read",
        ToolKind::Edit => "Edit",
        ToolKind::Exec => "Exec",
        ToolKind::Search => "Search",
        ToolKind::Web => "Web",
        ToolKind::Mcp => "MCP",
        ToolKind::Other => "Tool",
    }
}

pub fn tool_status_label(status: &ToolStatus) -> &'static str {
    match status {
        ToolStatus::Running => "running",
        ToolStatus::Completed => "completed",
        ToolStatus::Failed => "failed",
        ToolStatus::Denied => "denied",
    }
}

pub fn status_label(status: &SessionState) -> String {
    match status {
        SessionState::Starting => "starting".into(),
        SessionState::Idle => "idle".into(),
        SessionState::Running => "running".into(),
        SessionState::AwaitingInput => "awaiting input".into(),
        SessionState::Closed => "closed".into(),
        SessionState::Failed { message } => format!("failed: {message}"),
    }
}

pub fn outcome_label(outcome: &TurnOutcome) -> &'static str {
    match outcome {
        TurnOutcome::Completed => "completed",
        TurnOutcome::Cancelled => "cancelled",
        TurnOutcome::Failed => "failed",
    }
}

/// `HH:MM:SS` in UTC for a durable timestamp.
pub fn format_clock(ms: u64) -> String {
    let secs = ms / 1000 % 86_400;
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

pub fn format_duration(ms: u64) -> String {
    let secs = ms / 1000;
    if secs < 60 {
        format!("{}.{:01}s", secs, (ms % 1000) / 100)
    } else if secs < 3600 {
        format!("{}m {:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h {:02}m", secs / 3600, secs % 3600 / 60)
    }
}

// --- answers ---------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AnswerDraft {
    /// Selected option labels per question id.
    pub choices: BTreeMap<String, Vec<String>>,
    /// Typed text per question id (for free-text questions).
    pub free: BTreeMap<String, String>,
    /// Human-supplied JSON for an MCP form.
    pub structured: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnswerError {
    Missing(String),
    TooManyChoices(String),
    NotOffered { question: String, answer: String },
    EmptyStructured,
    InvalidStructured(String),
}

impl std::fmt::Display for AnswerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AnswerError::Missing(question) => write!(f, "answer the question “{question}”"),
            AnswerError::TooManyChoices(question) => {
                write!(f, "choose exactly one option for “{question}”")
            }
            AnswerError::NotOffered { question, answer } => {
                write!(f, "“{answer}” is not an offered option for “{question}”")
            }
            AnswerError::EmptyStructured => f.write_str("enter the structured JSON answer"),
            AnswerError::InvalidStructured(message) => {
                write!(f, "structured JSON is invalid: {message}")
            }
        }
    }
}

/// Turn the local draft into the daemon's typed response. Every question must
/// be answered exactly once; offered options are validated here as well as by
/// the adapter, so a refusal is not the first time the user learns.
pub fn build_question_response(
    questions: &[Question],
    schema_json: Option<&str>,
    draft: &AnswerDraft,
) -> Result<QuestionResponse, AnswerError> {
    if schema_json.is_some() {
        let structured = draft.structured.trim();
        if structured.is_empty() {
            return Err(AnswerError::EmptyStructured);
        }
        serde_json::from_str::<serde_json::Value>(structured)
            .map_err(|error| AnswerError::InvalidStructured(error.to_string()))?;
        return Ok(QuestionResponse {
            answers: BTreeMap::new(),
            structured_json: Some(structured.to_owned()),
        });
    }
    let mut answers = BTreeMap::new();
    for question in questions {
        let choices = draft
            .choices
            .get(&question.id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let typed = draft
            .free
            .get(&question.id)
            .map(|text| text.trim())
            .unwrap_or_default();
        let answer: Vec<String> = if !choices.is_empty() {
            if !question.multi && choices.len() != 1 {
                return Err(AnswerError::TooManyChoices(question.id.clone()));
            }
            for choice in choices {
                if !question.free_text && !question.options.iter().any(|o| o.label == *choice) {
                    return Err(AnswerError::NotOffered {
                        question: question.id.clone(),
                        answer: choice.clone(),
                    });
                }
            }
            choices.to_vec()
        } else if !typed.is_empty() {
            if !question.free_text {
                return Err(AnswerError::Missing(question.id.clone()));
            }
            vec![typed.to_owned()]
        } else {
            return Err(AnswerError::Missing(question.id.clone()));
        };
        answers.insert(question.id.clone(), answer);
    }
    Ok(QuestionResponse {
        answers,
        structured_json: None,
    })
}

// --- inbox and notifications ----------------------------------------------

/// Unread items first, then newest. Ties are broken by id so the order is
/// stable across snapshots.
pub fn ordered_inbox(items: &[InboxItem]) -> Vec<&InboxItem> {
    let mut ordered: Vec<&InboxItem> = items
        .iter()
        .filter(|item| item.state != InboxState::Dismissed)
        .collect();
    ordered.sort_by(|a, b| {
        let a_unread = a.state == InboxState::Unread;
        let b_unread = b.state == InboxState::Unread;
        b_unread
            .cmp(&a_unread)
            .then_with(|| b.created_ms.cmp(&a.created_ms))
            .then_with(|| a.id.cmp(&b.id))
    });
    ordered
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Approval,
    Question,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toast {
    pub session: uuid::Uuid,
    pub seq: u64,
    pub kind: ToastKind,
    pub title: String,
}

/// Which durable events deserve an in-app toast.
pub fn toast_for(event: &ChatEvent) -> Option<(ToastKind, &'static str)> {
    match event {
        ChatEvent::ApprovalRequest { .. } => Some((ToastKind::Approval, "Approval needed")),
        ChatEvent::QuestionRequest { .. } => Some((ToastKind::Question, "Question waiting")),
        ChatEvent::TurnEnd { outcome, .. } => Some(match outcome {
            TurnOutcome::Completed => (ToastKind::Completed, "Turn completed"),
            TurnOutcome::Cancelled => (ToastKind::Failed, "Turn cancelled"),
            TurnOutcome::Failed => (ToastKind::Failed, "Turn failed"),
        }),
        ChatEvent::Error { fatal: true, .. } => Some((ToastKind::Failed, "Session error")),
        _ => None,
    }
}

/// Events newer than the last seen sequence, in session/sequence order.
pub fn new_toasts(
    seen: &HashMap<uuid::Uuid, u64>,
    events: &HashMap<uuid::Uuid, Vec<EventRecord>>,
) -> Vec<Toast> {
    let mut sessions: Vec<&uuid::Uuid> = events.keys().collect();
    sessions.sort();
    let mut toasts = Vec::new();
    for session in sessions {
        let last = seen.get(session).copied().unwrap_or(0);
        for record in events.get(session).into_iter().flatten() {
            if record.sequence <= last {
                continue;
            }
            if let Some((kind, title)) = toast_for(&record.event) {
                toasts.push(Toast {
                    session: *session,
                    seq: record.sequence,
                    kind,
                    title: title.into(),
                });
            }
        }
    }
    toasts
}

/// Highest durable sequence the mirror holds for every session.
pub fn seen_sequences(events: &HashMap<uuid::Uuid, Vec<EventRecord>>) -> HashMap<uuid::Uuid, u64> {
    events
        .iter()
        .map(|(session, records)| {
            (
                *session,
                records.last().map(|record| record.sequence).unwrap_or(0),
            )
        })
        .collect()
}

/// Sessions of one worktree, newest first, for the Chat panel.
pub fn sessions_newest_first<'a>(sessions: &[&'a ChatSession]) -> Vec<&'a ChatSession> {
    let mut sorted = sessions.to_vec();
    sorted.sort_by(|a, b| {
        b.created_ms
            .cmp(&a.created_ms)
            .then_with(|| a.id.cmp(&b.id))
    });
    sorted
}

#[cfg(test)]
mod tests {
    use super::*;
    use fl_model::WorktreeId;
    use fl_model::chat::{
        ApprovalDecision, BoundedText, ChatEvent, EventRecord, InboxKind, PermissionMode, Question,
        QuestionOption, ResumeTarget, StatusSource, TextUpdate, ToolStatus,
    };

    fn record(sequence: u64, event: ChatEvent) -> EventRecord {
        EventRecord {
            schema_version: 1,
            session_id: uuid::Uuid::nil(),
            incarnation: uuid::Uuid::nil(),
            sequence,
            timestamp_ms: 1_000 * sequence,
            native_session_id: None,
            turn_id: None,
            event,
        }
    }

    fn question(id: &str, options: &[&str], multi: bool, free_text: bool) -> Question {
        Question {
            id: id.into(),
            text: format!("{id}?"),
            options: options
                .iter()
                .map(|label| QuestionOption {
                    label: (*label).into(),
                    description: String::new(),
                })
                .collect(),
            multi,
            free_text,
            secret: false,
        }
    }

    fn inbox(id: u128, state: InboxState, created_ms: u64) -> InboxItem {
        InboxItem {
            id: uuid::Uuid::from_u128(id),
            session: uuid::Uuid::nil(),
            worktree: WorktreeId::from_bytes([1; 16]),
            kind: InboxKind::Approval,
            state,
            title: format!("item {id}"),
            created_ms,
        }
    }

    #[test]
    fn hook_facts_render_as_status_rows_not_turns() {
        use fl_model::coordination::{HookFact, HookFactKind};
        let rows = build_transcript(&[
            record(
                1,
                ChatEvent::HookFact(HookFact {
                    kind: HookFactKind::TurnStarted,
                    detail: "prompt submitted".into(),
                }),
            ),
            record(
                2,
                ChatEvent::HookFact(HookFact {
                    kind: HookFactKind::TurnEnded,
                    detail: String::new(),
                }),
            ),
        ]);
        assert_eq!(rows.len(), 2);
        assert!(matches!(
            &rows[0].kind,
            RowKind::Hook { kind: HookFactKind::TurnStarted, detail } if detail == "prompt submitted"
        ));
        assert!(matches!(
            &rows[1].kind,
            RowKind::Hook {
                kind: HookFactKind::TurnEnded,
                ..
            }
        ));
        assert!(
            !rows
                .iter()
                .any(|row| matches!(row.kind, RowKind::TurnEnd { .. }))
        );
    }

    #[test]
    fn streaming_deltas_merge_into_one_bubble() {
        let events = vec![
            record(
                1,
                ChatEvent::AssistantText {
                    id: "m1".into(),
                    update: TextUpdate::Delta,
                    text: "Hello".into(),
                },
            ),
            record(
                2,
                ChatEvent::AssistantText {
                    id: "m1".into(),
                    update: TextUpdate::Delta,
                    text: ", world".into(),
                },
            ),
            // A cumulative snapshot must replace, not duplicate.
            record(
                3,
                ChatEvent::AssistantText {
                    id: "m1".into(),
                    update: TextUpdate::Delta,
                    text: "Hello, world!".into(),
                },
            ),
            record(
                4,
                ChatEvent::AssistantText {
                    id: "m2".into(),
                    update: TextUpdate::Delta,
                    text: "Second block".into(),
                },
            ),
            record(
                5,
                ChatEvent::AssistantText {
                    id: "m1".into(),
                    update: TextUpdate::Final,
                    text: "Hello, world.".into(),
                },
            ),
        ];
        let rows = build_transcript(&events);
        assert_eq!(rows.len(), 2);
        match &rows[0].kind {
            RowKind::Assistant { id, text, complete } => {
                assert_eq!(id, "m1");
                assert_eq!(text, "Hello, world.");
                assert!(complete);
            }
            other => panic!("unexpected first row: {other:?}"),
        }
        assert_eq!(rows[1].seq, 4);
    }

    #[test]
    fn tool_output_deltas_merge_and_repeat_calls_update_in_place() {
        let call = |status: ToolStatus, output: Option<&str>| ToolCall {
            id: "t1".into(),
            kind: ToolKind::Exec,
            title: "cargo test".into(),
            input_summary: BoundedText {
                text: "cargo test".into(),
                truncated: false,
            },
            status,
            output: output.map(|text| BoundedText {
                text: text.into(),
                truncated: false,
            }),
            diff: None,
        };
        let events = vec![
            record(1, ChatEvent::ToolCall(call(ToolStatus::Running, None))),
            record(
                2,
                ChatEvent::ToolOutput {
                    id: "t1".into(),
                    delta: BoundedText {
                        text: "running ".into(),
                        truncated: false,
                    },
                },
            ),
            record(
                3,
                ChatEvent::ToolOutput {
                    id: "t1".into(),
                    delta: BoundedText {
                        text: "2 tests".into(),
                        truncated: false,
                    },
                },
            ),
            record(
                4,
                ChatEvent::ToolCall(call(ToolStatus::Completed, Some("running 2 tests"))),
            ),
        ];
        let rows = build_transcript(&events);
        assert_eq!(rows.len(), 1);
        match &rows[0].kind {
            RowKind::Tool(tool) => {
                assert_eq!(tool.status, ToolStatus::Completed);
                assert_eq!(tool.output.as_deref(), Some("running 2 tests"));
            }
            other => panic!("unexpected row: {other:?}"),
        }
    }

    #[test]
    fn resolution_events_resolve_cards_and_subagents_group_under_parent() {
        let events = vec![
            record(
                1,
                ChatEvent::ToolCall(ToolCall {
                    id: "t1".into(),
                    kind: ToolKind::Mcp,
                    title: "delegate".into(),
                    input_summary: BoundedText {
                        text: "spawn".into(),
                        truncated: false,
                    },
                    status: ToolStatus::Running,
                    output: None,
                    diff: None,
                }),
            ),
            record(
                2,
                ChatEvent::Subagent {
                    parent_tool_id: "t1".into(),
                    events: vec![ChatEvent::AssistantText {
                        id: "child".into(),
                        update: TextUpdate::Final,
                        text: "child output".into(),
                    }],
                },
            ),
            record(
                3,
                ChatEvent::ApprovalRequest {
                    id: "a1".into(),
                    tool: "bash".into(),
                    details: BoundedText {
                        text: "rm -rf".into(),
                        truncated: false,
                    },
                    options: vec![ApprovalDecision::AllowOnce, ApprovalDecision::Deny],
                },
            ),
            record(4, ChatEvent::RequestResolved { id: "a1".into() }),
        ];
        let rows = build_transcript(&events);
        match &rows[0].kind {
            RowKind::Tool(tool) => {
                assert_eq!(tool.children.len(), 1);
                assert!(matches!(tool.children[0].kind, RowKind::Assistant { .. }));
            }
            other => panic!("unexpected row: {other:?}"),
        }
        match &rows[1].kind {
            RowKind::Approval { resolved, .. } => assert!(resolved),
            other => panic!("unexpected row: {other:?}"),
        }
    }

    #[test]
    fn markdown_blocks_cover_headings_lists_and_fences() {
        let source = "# Title\n\nfirst line\nsecond line\n\n- one\n- two\n\n1. first\n2. second\n\n```rust\nlet x = 1;\nlet y = 2;\n```\n\nTail with `code` inside.";
        let blocks = parse_markdown(source);
        assert_eq!(
            blocks,
            vec![
                MarkdownBlock::Heading {
                    level: 1,
                    text: "Title".into()
                },
                MarkdownBlock::Paragraph("first line second line".into()),
                MarkdownBlock::List {
                    ordered: false,
                    items: vec!["one".into(), "two".into()]
                },
                MarkdownBlock::List {
                    ordered: true,
                    items: vec!["first".into(), "second".into()]
                },
                MarkdownBlock::Code {
                    language: Some("rust".into()),
                    text: "let x = 1;\nlet y = 2;".into()
                },
                MarkdownBlock::Paragraph("Tail with `code` inside.".into()),
            ]
        );
        assert_eq!(
            inline_spans("Tail with `code` inside."),
            vec![
                InlineSpan {
                    text: "Tail with ".into(),
                    code: false
                },
                InlineSpan {
                    text: "code".into(),
                    code: true
                },
                InlineSpan {
                    text: " inside.".into(),
                    code: false
                },
            ]
        );
    }

    #[test]
    fn unified_diff_numbers_lines_and_marks_truncation() {
        let source =
            "diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,2 +1,3 @@\n context\n-old\n+new\n+extra\n";
        let parsed = parse_unified_diff(source);
        assert!(!parsed.truncated);
        assert_eq!(parsed.rows[0].kind, DiffKind::File);
        let rows: Vec<(&DiffKind, Option<u64>, Option<u64>, &str)> = parsed
            .rows
            .iter()
            .map(|row| (&row.kind, row.old, row.new, row.text.as_str()))
            .collect();
        assert!(rows.contains(&(&DiffKind::Context, Some(1), Some(1), "context")));
        assert!(rows.contains(&(&DiffKind::Removed, Some(2), None, "old")));
        assert!(rows.contains(&(&DiffKind::Added, None, Some(2), "new")));
        assert!(rows.contains(&(&DiffKind::Added, None, Some(3), "extra")));
    }

    #[test]
    fn composer_chooses_send_or_steer_from_real_status() {
        assert_eq!(
            composer_action(&SessionState::Idle),
            Some(SubmitAction::Send)
        );
        assert_eq!(
            composer_action(&SessionState::Running),
            Some(SubmitAction::Steer)
        );
        assert_eq!(composer_action(&SessionState::AwaitingInput), None);
        assert_eq!(composer_action(&SessionState::Closed), None);
        assert_eq!(
            composer_action(&SessionState::Failed {
                message: "x".into()
            }),
            None
        );
        assert!(can_stop(&SessionState::Running));
        assert!(!can_stop(&SessionState::Idle));
    }

    #[test]
    fn decision_gating_blocks_inflight_resolved_and_offline_submissions() {
        assert!(decision_ready(true, false, false));
        assert!(!decision_ready(true, true, false));
        assert!(!decision_ready(true, false, true));
        assert!(!decision_ready(false, false, false));
    }

    #[test]
    fn answers_require_every_question_once_and_validate_options() {
        let questions = vec![
            question("q1", &["One", "Two"], false, false),
            question("q2", &["A", "B"], true, false),
            question("q3", &[], false, true),
        ];
        let mut draft = AnswerDraft::default();
        draft.choices.insert("q1".into(), vec!["One".into()]);
        draft
            .choices
            .insert("q2".into(), vec!["A".into(), "B".into()]);
        draft.free.insert("q3".into(), "typed".into());
        let response = build_question_response(&questions, None, &draft).unwrap();
        assert_eq!(response.answers["q1"], vec!["One"]);
        assert_eq!(response.answers["q2"], vec!["A", "B"]);
        assert_eq!(response.answers["q3"], vec!["typed"]);
        assert!(response.structured_json.is_none());

        let mut missing = draft.clone();
        missing.free.remove("q3");
        assert_eq!(
            build_question_response(&questions, None, &missing),
            Err(AnswerError::Missing("q3".into()))
        );

        let mut wrong = draft.clone();
        wrong
            .choices
            .insert("q1".into(), vec!["One".into(), "Two".into()]);
        assert_eq!(
            build_question_response(&questions, None, &wrong),
            Err(AnswerError::TooManyChoices("q1".into()))
        );

        let mut not_offered = draft.clone();
        not_offered
            .choices
            .insert("q1".into(), vec!["Three".into()]);
        assert!(matches!(
            build_question_response(&questions, None, &not_offered),
            Err(AnswerError::NotOffered { .. })
        ));
    }

    #[test]
    fn structured_forms_require_valid_json_and_no_text_answers() {
        let questions = vec![question("q1", &[], false, true)];
        let mut draft = AnswerDraft::default();
        assert_eq!(
            build_question_response(&questions, Some("{}"), &draft),
            Err(AnswerError::EmptyStructured)
        );
        draft.structured = "{not json".into();
        assert!(matches!(
            build_question_response(&questions, Some("{}"), &draft),
            Err(AnswerError::InvalidStructured(_))
        ));
        draft.structured = "{\"answer\":42}".into();
        draft.free.insert("q1".into(), "ignored".into());
        let response = build_question_response(&questions, Some("{}"), &draft).unwrap();
        assert!(response.answers.is_empty());
        assert_eq!(response.structured_json.as_deref(), Some("{\"answer\":42}"));
    }

    #[test]
    fn inbox_is_unread_first_then_newest() {
        let items = vec![
            inbox(1, InboxState::Read, 1_000),
            inbox(2, InboxState::Unread, 500),
            inbox(3, InboxState::Unread, 900),
            inbox(4, InboxState::Dismissed, 2_000),
            inbox(5, InboxState::Read, 3_000),
        ];
        let ordered = ordered_inbox(&items);
        let ids: Vec<u128> = ordered.iter().map(|item| item.id.as_u128()).collect();
        assert_eq!(ids, vec![3, 2, 5, 1]);
    }

    #[test]
    fn toasts_are_derived_only_from_events_newer_than_seen() {
        let session = uuid::Uuid::from_u128(9);
        let mut events = HashMap::new();
        events.insert(
            session,
            vec![
                record(
                    1,
                    ChatEvent::ApprovalRequest {
                        id: "a".into(),
                        tool: "bash".into(),
                        details: BoundedText {
                            text: "x".into(),
                            truncated: false,
                        },
                        options: vec![ApprovalDecision::AllowOnce],
                    },
                ),
                record(
                    2,
                    ChatEvent::AssistantText {
                        id: "m".into(),
                        update: TextUpdate::Delta,
                        text: "hi".into(),
                    },
                ),
                record(
                    3,
                    ChatEvent::TurnEnd {
                        duration_ms: Some(10),
                        turns: 1,
                        usage: Usage::default(),
                        model: None,
                        outcome: TurnOutcome::Completed,
                    },
                ),
            ],
        );
        let mut seen = HashMap::new();
        seen.insert(session, 0);
        let toasts = new_toasts(&seen, &events);
        assert_eq!(toasts.len(), 2);
        assert_eq!(toasts[0].kind, ToastKind::Approval);
        assert_eq!(toasts[1].kind, ToastKind::Completed);

        seen.insert(session, 3);
        assert!(new_toasts(&seen, &events).is_empty());
        assert_eq!(seen_sequences(&events)[&session], 3);
    }

    #[test]
    fn sessions_are_listed_newest_first() {
        let session = |id: u128, created_ms: u64| ChatSession {
            id: uuid::Uuid::from_u128(id),
            worktree: WorktreeId::from_bytes([1; 16]),
            provider: "claude".into(),
            label: format!("chat {id}"),
            status: SessionState::Idle,
            status_source: StatusSource::Structured,
            model: None,
            thinking: None,
            permission_mode: PermissionMode::Ask,
            resume: None::<ResumeTarget>,
            seq: 0,
            created_ms,
            updated_ms: created_ms,
        };
        let a = session(1, 10);
        let b = session(2, 20);
        let ordered = sessions_newest_first(&[&a, &b]);
        assert_eq!(ordered[0].id, b.id);
    }
}
