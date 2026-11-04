//! Basic character formatting on shared documents (C7.3): the wire [`BasicFormat`] patch,
//! validated into document properties before anything is touched.
//!
//! Only the supported properties can enter through the network — no style ids, links, fields or
//! arbitrary `CharProps` — and every value is checked: underline and highlight names are
//! WordCraft's own (camelCase, as `format.underline` / `format.highlight` take them), colours are
//! `RRGGBB` or `auto`, fonts are short printable names and sizes half-points from 1 to 1638 pt.

use wordcraft_collab_protocol::operations::BasicFormat;
use wordcraft_doc::props::{CharProps, Highlight, Rgb, TextColor, Underline};

use crate::ModelError;

/// Longest font name a patch may carry, in bytes.
pub const MAX_FONT_NAME_BYTES: usize = 128;

/// The checked effect of a [`BasicFormat`] patch.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FormatPatch {
    bold: Option<bool>,
    italic: Option<bool>,
    underline: Option<Underline>,
    font: Option<String>,
    size: Option<f32>,
    color: Option<TextColor>,
    highlight: Option<Highlight>,
}

const INVALID: ModelError = ModelError::InvalidFormat;

impl FormatPatch {
    /// Validate a wire patch. An empty patch is refused: it would commit a revision that does
    /// nothing.
    pub fn parse(f: &BasicFormat) -> Result<Self, ModelError> {
        if f.is_empty() {
            return Err(INVALID);
        }
        let underline = f.underline.as_deref().map(named::<Underline>).transpose()?;
        let highlight = f.highlight.as_deref().map(named::<Highlight>).transpose()?;
        let font = match &f.font {
            Some(font) if font.trim().is_empty() || font.len() > MAX_FONT_NAME_BYTES || font.chars().any(char::is_control) => {
                return Err(INVALID);
            }
            other => other.clone(),
        };
        let size = match f.size_half_points {
            Some(h) if !(2..=3276).contains(&h) => return Err(INVALID),
            Some(h) => Some(f32::from(h) / 2.0),
            None => None,
        };
        let color = match f.color.as_deref() {
            None => None,
            Some("auto") => Some(TextColor::Auto),
            Some(hex) if hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()) => Some(TextColor::Rgb(Rgb::parse(hex).ok_or(INVALID)?)),
            Some(_) => return Err(INVALID),
        };
        Ok(Self { bold: f.bold, italic: f.italic, underline, font, size, color, highlight })
    }

    /// Patch `p`: only the properties the patch names change (with their complex-script twins,
    /// as the editor's Bold/Italic/Font/Size do); everything else is kept.
    pub fn apply_to(&self, p: &mut CharProps) {
        if let Some(b) = self.bold {
            p.bold = Some(b);
            p.bold_cs = Some(b);
        }
        if let Some(i) = self.italic {
            p.italic = Some(i);
            p.italic_cs = Some(i);
        }
        if let Some(u) = self.underline {
            p.underline = Some(u);
        }
        if let Some(font) = &self.font {
            p.font = Some(font.clone());
            p.font_cs = Some(font.clone());
            p.font_ea = Some(font.clone());
        }
        if let Some(size) = self.size {
            p.size = Some(size);
            p.size_cs = Some(size);
        }
        if let Some(color) = self.color {
            p.color = Some(color);
        }
        if let Some(h) = self.highlight {
            p.highlight = Some(h);
        }
    }
}

/// The wire name of an underline or highlight (its serde name).
pub fn wire_name<T: serde::Serialize>(value: &T) -> Option<String> {
    serde_json::to_value(value).ok()?.as_str().map(str::to_owned)
}

/// A unit enum from its wire name, refusing anything else.
fn named<T: serde::de::DeserializeOwned + serde::Serialize>(name: &str) -> Result<T, ModelError> {
    let value: T = serde_json::from_value(serde_json::Value::String(name.into())).map_err(|_| INVALID)?;
    // Round trip: refuse aliases so every replica stores the same spelling.
    if wire_name(&value).as_deref() != Some(name) {
        return Err(INVALID);
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patch(json: &str) -> Result<FormatPatch, ModelError> {
        FormatPatch::parse(&serde_json::from_str(json).unwrap())
    }

    #[test]
    fn supported_properties_patch_only_what_they_name() {
        let mut p = CharProps { strike: Some(true), italic: Some(true), ..CharProps::default() };
        patch(r#"{"bold":true,"underline":"double","font":"Liberation Serif","size_half_points":25,"color":"C00000","highlight":"yellow"}"#)
            .unwrap()
            .apply_to(&mut p);
        assert_eq!((p.bold, p.bold_cs, p.italic, p.strike), (Some(true), Some(true), Some(true), Some(true)));
        assert_eq!(p.underline, Some(Underline::Double));
        assert_eq!(p.font.as_deref(), Some("Liberation Serif"));
        assert_eq!(p.size, Some(12.5));
        assert_eq!(p.color, Some(TextColor::Rgb(Rgb(0xC0, 0, 0))));
        assert_eq!(p.highlight, Some(Highlight::Yellow));
        patch(r#"{"bold":false,"color":"auto","highlight":"none","underline":"none"}"#).unwrap().apply_to(&mut p);
        assert_eq!((p.bold, p.color, p.highlight, p.underline), (Some(false), Some(TextColor::Auto), Some(Highlight::None), Some(Underline::None)));
    }

    #[test]
    fn hostile_values_are_refused() {
        for bad in [
            "{}",
            r#"{"underline":"Double"}"#,
            r#"{"underline":"sparkly"}"#,
            r##"{"highlight":"#FFFF00"}"##,
            r#"{"font":"  "}"#,
            r#"{"font":"A\u0007B"}"#,
            r#"{"size_half_points":0}"#,
            r#"{"size_half_points":65535}"#,
            r#"{"color":"red"}"#,
            r#"{"color":"12345"}"#,
            r#"{"color":"GGGGGG"}"#,
        ] {
            assert!(patch(bad).is_err(), "{bad}");
        }
        let long = format!(r#"{{"font":"{}"}}"#, "x".repeat(MAX_FONT_NAME_BYTES + 1));
        assert!(patch(&long).is_err());
        assert!(serde_json::from_str::<BasicFormat>(r#"{"style":"Heading1"}"#).is_err());
    }
}
