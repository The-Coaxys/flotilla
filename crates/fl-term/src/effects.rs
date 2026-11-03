//! Typed effects are durable data; parser callbacks never cross a checkpoint.
use alacritty_terminal::event::{Event, WindowSize};
use alacritty_terminal::term::ClipboardType;
use base64::Engine as _;
use serde::{Deserialize, Serialize};

use crate::{EngineError, Size, palette};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardTarget {
    Clipboard,
    Selection,
}

impl From<ClipboardType> for ClipboardTarget {
    fn from(value: ClipboardType) -> Self {
        match value {
            ClipboardType::Clipboard => Self::Clipboard,
            ClipboardType::Selection => Self::Selection,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Effect {
    /// Stable within a child incarnation, including after daemon recovery.
    pub sequence: u64,
    pub kind: EffectKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum EffectKind {
    Reply(Vec<u8>),
    Title(Option<String>),
    CurrentDirectory(String),
    ShellIntegration {
        action: u8,
        exit_status: Option<i32>,
    },
    Bell,
    ClipboardStore {
        target: ClipboardTarget,
        text: String,
    },
    ClipboardRead {
        target: ClipboardTarget,
        reply_prefix: String,
        reply_suffix: String,
    },
    Exit {
        code: Option<i32>,
    },
}

impl EffectKind {
    /// Called only after host consent. Persist the concrete bytes before writing
    /// them through the prochost's deduplicated input boundary.
    pub fn clipboard_reply(&self, text: &str) -> Option<Vec<u8>> {
        let Self::ClipboardRead {
            reply_prefix,
            reply_suffix,
            ..
        } = self
        else {
            return None;
        };
        let mut reply = String::with_capacity(
            reply_prefix.len() + text.len().div_ceil(3) * 4 + reply_suffix.len(),
        );
        reply.push_str(reply_prefix);
        base64::engine::general_purpose::STANDARD.encode_string(text, &mut reply);
        reply.push_str(reply_suffix);
        Some(reply.into_bytes())
    }

    pub(crate) fn external(&self) -> bool {
        matches!(
            self,
            Self::Reply(_) | Self::Bell | Self::ClipboardStore { .. } | Self::ClipboardRead { .. }
        )
    }
}

pub(crate) fn translate(
    event: Event,
    size: Size,
    colors: &[u32],
) -> Result<Option<EffectKind>, EngineError> {
    let effect = match event {
        Event::Title(title) => EffectKind::Title(Some(title)),
        Event::ResetTitle => EffectKind::Title(None),
        Event::CurrentDirectory(uri) => EffectKind::CurrentDirectory(uri),
        Event::ShellIntegration {
            action,
            exit_status,
        } => EffectKind::ShellIntegration {
            action,
            exit_status,
        },
        Event::PtyWrite(text) => EffectKind::Reply(text.into_bytes()),
        Event::Bell => EffectKind::Bell,
        Event::Exit => EffectKind::Exit { code: None },
        Event::ChildExit(status) => EffectKind::Exit {
            code: status.code(),
        },
        Event::ClipboardStore(target, text) => EffectKind::ClipboardStore {
            target: target.into(),
            text,
        },
        Event::ClipboardLoad(target, formatter) => {
            let empty_reply = formatter("");
            let separator = empty_reply.rfind(';').ok_or_else(|| {
                EngineError::InvalidEffect("clipboard reply has no separator".into())
            })? + 1;
            EffectKind::ClipboardRead {
                target: target.into(),
                reply_prefix: empty_reply[..separator].to_string(),
                reply_suffix: empty_reply[separator..].to_string(),
            }
        }
        Event::ColorRequestResolved(index, current, formatter) => {
            let color = current
                .map(palette::pack)
                .or_else(|| colors.get(index).copied())
                .ok_or_else(|| EngineError::InvalidEffect(format!("palette index {index}")))?;
            EffectKind::Reply(formatter(palette::unpack(color)).into_bytes())
        }
        Event::TextAreaSizeRequest(formatter) => EffectKind::Reply(
            formatter(WindowSize {
                num_lines: size.rows,
                num_cols: size.columns,
                cell_width: size.cell_width,
                cell_height: size.cell_height,
            })
            .into_bytes(),
        ),
        Event::MouseCursorDirty | Event::CursorBlinkingChange | Event::Wakeup => return Ok(None),
    };
    Ok(Some(effect))
}
