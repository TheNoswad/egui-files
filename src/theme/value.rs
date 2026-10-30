//! Merging and diffing TOML tables: how a theme inherits from its base, and
//! how the editor's "Advanced" settings become a `[style]` table.

use toml::{Table, Value};

use super::file::Hex;

/// Lays `over` onto `base`: tables merge key by key, anything else is replaced.
pub fn merge(base: &mut Table, over: &Table) {
    for (key, value) in over {
        match (base.get_mut(key), value) {
            (Some(Value::Table(base)), Value::Table(over)) => merge(base, over),
            (Some(base), value) => *base = from_hex(base, value),
            (None, value) => {
                base.insert(key.clone(), value.clone());
            }
        }
    }
}

/// egui writes colors as `[r, g, b, a]`, premultiplied; theme files may use hex.
fn from_hex(base: &Value, value: &Value) -> Value {
    match (base, value) {
        (Value::Array(_), Value::String(text)) => Hex::parse(text).map_or_else(
            || value.clone(),
            |color| Value::Array(color.to_array().map(|c| Value::Integer(c.into())).into()),
        ),
        _ => value.clone(),
    }
}

/// What in `edited` differs from `base`, or `None` if nothing does.
pub fn diff(base: &Table, edited: &Table) -> Option<Table> {
    let mut changes = Table::new();
    for (key, value) in edited {
        match (base.get(key), value) {
            (Some(Value::Table(base)), Value::Table(edited)) => {
                if let Some(table) = diff(base, edited) {
                    changes.insert(key.clone(), Value::Table(table));
                }
            }
            (Some(base), value) if base == value => {}
            (_, value) => {
                changes.insert(key.clone(), readable(value));
            }
        }
    }
    (!changes.is_empty()).then_some(changes)
}

/// `value` as a person would write it: colors in hex and floats without the
/// noise of their trip through `f32`.
fn readable(value: &Value) -> Value {
    match value {
        Value::Float(f) => Value::Float(short_float(*f)),
        Value::Array(items) => {
            if let Some(color) = as_color(items) {
                return Value::String(Hex(color).to_string());
            }
            Value::Array(items.iter().map(readable).collect())
        }
        Value::Table(table) => Value::Table(
            table
                .iter()
                .map(|(key, value)| (key.clone(), readable(value)))
                .collect(),
        ),
        value => value.clone(),
    }
}

fn short_float(f: f64) -> f64 {
    (f as f32).to_string().parse().unwrap_or(f)
}

/// Four bytes, which in egui's style is always a `Color32`.
fn as_color(items: &[Value]) -> Option<egui::Color32> {
    let bytes: Vec<u8> = items
        .iter()
        .map(|v| v.as_integer().and_then(|i| u8::try_from(i).ok()))
        .collect::<Option<_>>()?;
    let [r, g, b, a] = bytes.try_into().ok()?;
    Some(egui::Color32::from_rgba_premultiplied(r, g, b, a))
}

/// Applies `overrides` to `style`, or says why they don't fit.
pub fn apply_style(style: &mut egui::Style, overrides: &Table) -> Result<(), String> {
    if overrides.is_empty() {
        return Ok(());
    }
    let mut table = Table::try_from(&*style).map_err(|e| e.to_string())?;
    merge(&mut table, overrides);
    *style = Value::Table(table)
        .try_into()
        .map_err(|e: toml::de::Error| e.message().to_owned())?;
    Ok(())
}

/// The `[style]` table that turns `base` into `edited`.
pub fn style_diff(base: &egui::Style, edited: &egui::Style) -> Table {
    let (Ok(base), Ok(edited)) = (Table::try_from(base), Table::try_from(edited)) else {
        return Table::new();
    };
    diff(&base, &edited).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn style_overrides_round_trip() {
        let base = egui::Style::default();
        let mut edited = base.clone();
        edited.visuals.panel_fill = egui::Color32::from_rgb(1, 2, 3);
        edited.spacing.item_spacing = egui::vec2(10.0, 4.4);
        let changes = style_diff(&base, &edited);
        let text = toml::to_string(&changes).unwrap();
        assert!(text.contains("panel_fill = \"#010203\""), "{text}");
        assert!(text.contains("4.4"), "{text}");
        assert!(!text.contains("4.40000"), "{text}");

        let mut restyled = base.clone();
        apply_style(&mut restyled, &toml::from_str(&text).unwrap()).unwrap();
        assert_eq!(restyled.visuals, edited.visuals);
        assert_eq!(restyled.spacing.item_spacing, edited.spacing.item_spacing);
    }

    #[test]
    fn bad_overrides_are_reported() {
        let mut style = egui::Style::default();
        let overrides = toml::from_str("[visuals]\npanel_fill = \"red\"").unwrap();
        assert!(apply_style(&mut style, &overrides).is_err());
    }

    #[test]
    fn merging_keeps_what_the_child_leaves_out() {
        let mut base: Table = toml::from_str("a = 1\n[t]\nx = 1\ny = 2").unwrap();
        merge(&mut base, &toml::from_str("[t]\ny = 3").unwrap());
        assert_eq!(base["a"].as_integer(), Some(1));
        assert_eq!(base["t"]["x"].as_integer(), Some(1));
        assert_eq!(base["t"]["y"].as_integer(), Some(3));
    }
}
