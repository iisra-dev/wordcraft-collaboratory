//! What a paste brings into a shared document (C7.2, C7.3).
//!
//! Paste, Keep Text Only and Merge Formatting resolve their content the way the editor does
//! (`wordcraft_engine::cmd::paste::paste_content`: WordCraft's own copy, another program's HTML
//! or RTF, or plain text), and the result is reduced to what shared documents support: paragraphs
//! of text runs with the basic character formatting of [`BasicFormat`]. Everything else is
//! refused with an explanation — tables, pictures and other embedded objects, or a paste too big
//! for one revision — and the user can paste it as text instead. Nothing here is ever turned
//! into arbitrary editor operations: [`crate::capture::capture_paste`] makes the edit from it.

use serde_json::Value;
use wordcraft_collab_model::format::wire_name;
use wordcraft_collab_protocol::operations::BasicFormat;
use wordcraft_doc::edit::Fragment;
use wordcraft_doc::para::OBJ;
use wordcraft_doc::props::TextColor;
use wordcraft_doc::{Block, StyleSheet};
use wordcraft_engine::Session;
use wordcraft_engine::cmd::paste::{Pasted, paste_content};

use crate::capture::CaptureError;

/// Largest text one paste may carry, in bytes: with its formatting and JSON it must fit the
/// server's message limit (`MAX_MESSAGE_BYTES`, 1 MiB) as a single revision.
pub const MAX_PASTE_TEXT_BYTES: usize = 256 * 1024;

/// Paste commands shared documents run as collaborative edits.
pub const PASTE_COMMANDS: &[&str] = &["edit.paste", "edit.pasteText", "edit.pasteMerge"];

pub const TOO_BIG: &str = "pasting this much at once (paste it in smaller parts)";
pub const OBJECTS: &str = "pasting pictures, fields or other objects (use Paste › Keep Text Only)";
pub const TABLES: &str = "pasting tables or text boxes (use Paste › Keep Text Only)";

/// One run of pasted text and the formatting to give it (`None`: keep the formatting the text
/// takes where it lands, as typing does).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PastedRun {
    pub text: String,
    pub format: Option<BasicFormat>,
}

/// Pasted paragraphs: the first continues the paragraph at the caret, each later one starts a
/// new paragraph. Never empty (an empty paste is an error).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Payload {
    pub paragraphs: Vec<Vec<PastedRun>>,
}

/// What `command` (one of [`PASTE_COMMANDS`]) with `params` would paste into `session`.
/// `session` is only used to read and adopt the clipboard content; its document is unchanged.
pub fn payload(session: &mut Session, command: &str, params: &Value) -> Result<Payload, CaptureError> {
    let empty = || CaptureError::BadParams { command: command.into(), reason: "the clipboard is empty" };
    let payload = match command {
        "edit.pasteText" | "edit.pasteMerge" => {
            let text = match params.get("text").and_then(Value::as_str) {
                Some(t) => t.to_owned(),
                None => match paste_content(session, params).map_err(|_| empty())? {
                    Pasted::Text(t) => t,
                    Pasted::Fragment(f) => f.plain_text(),
                },
            };
            plain(&text)?
        }
        "edit.paste" => match paste_content(session, params).map_err(|_| empty())? {
            Pasted::Text(t) => plain(&t)?,
            Pasted::Fragment(f) => formatted(&f, &session.doc.styles)?,
        },
        _ => return Err(CaptureError::Unsupported("this command")),
    };
    let bytes: usize = payload.paragraphs.iter().flatten().map(|r| r.text.len()).sum();
    if bytes > MAX_PASTE_TEXT_BYTES {
        return Err(CaptureError::Unsupported(TOO_BIG));
    }
    if payload.paragraphs.iter().all(|p| p.iter().all(|r| r.text.is_empty())) && payload.paragraphs.len() < 2 {
        return Err(empty());
    }
    Ok(payload)
}

/// Plain text: lines become paragraphs (`\r\n`, `\r` and `\n` all end one), other control
/// characters but tab are dropped, and the text keeps the formatting where it lands.
pub fn plain(text: &str) -> Result<Payload, CaptureError> {
    if text.len() > MAX_PASTE_TEXT_BYTES.saturating_mul(2) {
        return Err(CaptureError::Unsupported(TOO_BIG));
    }
    if text.contains(OBJ) {
        return Err(CaptureError::Unsupported(OBJECTS));
    }
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let paragraphs = text
        .split('\n')
        .map(|line| {
            let clean: String = line.chars().filter(|c| *c == '\t' || !c.is_control()).collect();
            vec![PastedRun { text: clean, format: None }]
        })
        .collect();
    Ok(Payload { paragraphs })
}

/// A formatted copy: its paragraphs and runs, each with its basic formatting resolved (direct
/// formatting and character styles), so it looks the same in the shared document.
fn formatted(fragment: &Fragment, styles: &StyleSheet) -> Result<Payload, CaptureError> {
    if !fragment.parts.is_empty() {
        return Err(CaptureError::Unsupported(TABLES));
    }
    let mut paragraphs = Vec::new();
    for block in &fragment.blocks {
        let Block::Para(p) = block else { return Err(CaptureError::Unsupported(TABLES)) };
        if p.text.contains(OBJ) {
            return Err(CaptureError::Unsupported(OBJECTS));
        }
        let mut runs = Vec::new();
        for (range, props) in p.run_ranges() {
            let Some(text) = p.text.get(range) else { continue };
            if text.is_empty() {
                continue;
            }
            // Manual line breaks stay; other control characters cannot be shared.
            let text: String = text.chars().filter(|c| *c == '\t' || *c == '\n' || !c.is_control()).collect();
            let r = styles.resolve_char(p.props.style.as_deref(), props);
            let format = BasicFormat {
                bold: Some(r.bold),
                italic: Some(r.italic),
                underline: wire_name(&r.underline),
                font: Some(r.font.clone()).filter(|f| !f.trim().is_empty() && f.len() <= wordcraft_collab_model::format::MAX_FONT_NAME_BYTES),
                size_half_points: Some((r.size * 2.0).round().clamp(2.0, 3276.0) as u16),
                color: Some(match r.color {
                    TextColor::Auto => "auto".into(),
                    TextColor::Rgb(c) => format!("{:02X}{:02X}{:02X}", c.0, c.1, c.2),
                }),
                highlight: wire_name(&props.highlight.unwrap_or_default()),
            };
            runs.push(PastedRun { text, format: Some(format) });
        }
        paragraphs.push(runs);
    }
    if paragraphs.is_empty() {
        return Err(CaptureError::Unsupported(TABLES));
    }
    Ok(Payload { paragraphs })
}
