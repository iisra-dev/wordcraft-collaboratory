//! Basic formatting commands on shared documents (C7.3).
//!
//! Bold, Italic, Underline, Font, Font Size, Font Color and Text Highlight Color are captured as a
//! [`BasicFormat`] patch on the selection ([`crate::capture::capture_format`]), never run locally:
//! the patch is computed here from the command's parameters and, for the toggles, from the
//! selection's current formatting (on unless the whole selection already has it, like the
//! editor). Other formatting commands stay blocked until they have tested semantics.

use serde_json::Value;
use wordcraft_collab_model::format::wire_name;
use wordcraft_collab_protocol::operations::BasicFormat;
use wordcraft_doc::props::{Highlight, TextColor, Underline};
use wordcraft_doc::resolve::ResolvedChar;
use wordcraft_doc::{Path, Pos, StoryRef};
use wordcraft_engine::Session;

use crate::capture::CaptureError;

/// Formatting commands shared documents support.
pub const FORMAT_COMMANDS: &[&str] = &[
    "format.bold",
    "format.italic",
    "format.underline",
    "format.doubleUnderline",
    "format.wordUnderline",
    "format.font",
    "format.size",
    "format.color",
    "format.highlight",
];

pub fn is_format_command(command: &str) -> bool {
    FORMAT_COMMANDS.contains(&command)
}

/// The patch `command` with `params` applies to `session`'s selection.
pub fn patch(session: &Session, command: &str, params: &Value) -> Result<BasicFormat, CaptureError> {
    let bad = |reason: &'static str| CaptureError::BadParams { command: command.into(), reason };
    let value = params.get("value").and_then(Value::as_bool);
    let toggle = |has: &dyn Fn(&ResolvedChar) -> bool| value.unwrap_or_else(|| !all_selected(session, has));
    let mut f = BasicFormat::default();
    match command {
        "format.bold" => f.bold = Some(toggle(&|r| r.bold)),
        "format.italic" => f.italic = Some(toggle(&|r| r.italic)),
        "format.underline" | "format.doubleUnderline" | "format.wordUnderline" => {
            let style = match command {
                "format.doubleUnderline" => Some(Underline::Double),
                "format.wordUnderline" => Some(Underline::Words),
                _ => params.get("style").and_then(Value::as_str).map(Underline::from_ooxml),
            };
            let on = match (value, style) {
                (Some(on), _) => on,
                // An explicit style sets it; with none, the button toggles single underline.
                (None, Some(_)) => true,
                (None, None) => !all_selected(session, &|r| r.underline != Underline::None),
            };
            let u = if on { style.filter(|s| *s != Underline::None).unwrap_or(Underline::Single) } else { Underline::None };
            f.underline = wire_name(&u);
        }
        "format.font" => {
            let name = params.get("name").and_then(Value::as_str).ok_or(bad("needs \"name\""))?;
            f.font = Some(name.to_owned());
        }
        "format.size" => {
            let size = params.get("size").and_then(Value::as_f64).ok_or(bad("needs \"size\""))?;
            if !size.is_finite() || !(1.0..=1638.0).contains(&size) {
                return Err(bad("size must be from 1 to 1638 points"));
            }
            f.size_half_points = Some((size * 2.0).round() as u16);
        }
        "format.color" => {
            let c = params.get("color").and_then(Value::as_str).ok_or(bad("needs \"color\""))?;
            f.color = Some(match TextColor::parse(c).ok_or(bad("color must be RRGGBB or auto"))? {
                TextColor::Auto => "auto".into(),
                TextColor::Rgb(c) => format!("{:02X}{:02X}{:02X}", c.0, c.1, c.2),
            });
        }
        "format.highlight" => {
            let h = match params.get("color").and_then(Value::as_str) {
                Some(name) => serde_json::from_value::<Highlight>(Value::String(name.into())).map_err(|_| bad("unknown highlight colour"))?,
                None if all_selected(session, &|r| r.highlight.is_some()) => Highlight::None,
                None => Highlight::Yellow,
            };
            f.highlight = wire_name(&h);
        }
        _ => return Err(CaptureError::Unsupported("this command")),
    }
    Ok(f)
}

/// Whether every character of the selection (top-level body paragraphs) has `has`.
fn all_selected(session: &Session, has: &dyn Fn(&ResolvedChar) -> bool) -> bool {
    let (a, b) = order(&session.sel.anchor, &session.sel.focus);
    let (Some(&i), Some(&j)) = (a.path.0.first(), b.path.0.first()) else { return false };
    let mut any = false;
    for k in i..=j {
        let Some(p) = session.doc.para(StoryRef::Body, &Path::top(k as usize)) else { return false };
        let from = if k == i { a.off } else { 0 };
        let to = if k == j { b.off } else { p.len() };
        for (range, props) in p.run_ranges() {
            if range.end <= from || range.start >= to || range.is_empty() {
                continue;
            }
            any = true;
            if !has(&session.doc.styles.resolve_char(p.props.style.as_deref(), props)) {
                return false;
            }
        }
    }
    any
}

fn order<'a>(a: &'a Pos, b: &'a Pos) -> (&'a Pos, &'a Pos) {
    if (&a.path.0, a.off) <= (&b.path.0, b.off) { (a, b) } else { (b, a) }
}
