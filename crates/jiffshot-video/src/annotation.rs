//! Validation for annotation objects shared by recording and document controls.

use jiffshot_control::{Error, ErrorCode};
use jiffshot_doc::object::{Kind, Style};
use serde_json::Value;

const MAX_OBJECTS: usize = 4096;
const MAX_POINTS: usize = 10_000;

fn invalid(message: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidRequest, message)
}

fn internal(message: impl ToString) -> Error {
    Error::new(ErrorCode::Internal, message.to_string())
}

/// Bounds serialized geometry and arrays before calling the raster engine.
///
/// # Errors
/// Returns an error when a value exceeds the annotation input limits.
pub fn validate_json(value: &Value) -> Result<(), Error> {
    match value {
        Value::Number(number)
            if number
                .as_f64()
                .is_none_or(|number| !number.is_finite() || number.abs() > 1_000_000.0) =>
        {
            Err(invalid(
                "annotation numbers must be finite and within +/-1000000",
            ))
        }
        Value::Array(items) => {
            if items.len() > MAX_POINTS {
                return Err(invalid("annotation arrays are limited to 10000 entries"));
            }
            for item in items {
                validate_json(item)?;
            }
            Ok(())
        }
        Value::Object(fields) => {
            for item in fields.values() {
                validate_json(item)?;
            }
            Ok(())
        }
        Value::String(text) if text.len() > 65_536 => {
            Err(invalid("annotation text is limited to 65536 bytes"))
        }
        _ => Ok(()),
    }
}

/// Merges and validates annotation style overrides.
///
/// # Errors
/// Returns an error for unknown, malformed, or out-of-range style properties.
pub fn style(value: Option<&Value>, base: Style) -> Result<Style, Error> {
    let Some(value) = value else { return Ok(base) };
    validate_json(value)?;
    let fields = value
        .as_object()
        .ok_or_else(|| invalid("style must be an object"))?;
    let mut merged = serde_json::to_value(base).map_err(internal)?;
    let map = merged
        .as_object_mut()
        .ok_or_else(|| internal("style is not an object"))?;
    for (key, value) in fields {
        if !map.contains_key(key) {
            return Err(invalid(format!("unknown style property {key:?}")));
        }
        map.insert(key.clone(), value.clone());
    }
    let style: Style =
        serde_json::from_value(merged).map_err(|error| invalid(format!("style: {error}")))?;
    if !(0.0..=1.0).contains(&style.opacity)
        || !(0.0..=512.0).contains(&style.width)
        || !(1.0..=512.0).contains(&style.font_size)
        || !(0.0..=512.0).contains(&style.shadow_blur)
        || !(0.0..=512.0).contains(&style.outline_width)
        || !(0.0..=512.0).contains(&style.shadow_distance)
        || !(0.0..=16.0).contains(&style.head_size)
    {
        return Err(invalid("style opacity must be 0..1; width, shadow_blur, outline_width 0..512; font_size 1..512"));
    }
    Ok(style)
}

/// Parses an annotation object after validating its geometry and nesting.
///
/// # Errors
/// Returns an error for malformed or unsupported annotation objects.
pub fn kind(value: &Value) -> Result<Kind, Error> {
    validate_json(value)?;
    let kind: Kind =
        serde_json::from_value(value.clone()).map_err(|error| invalid(format!("kind: {error}")))?;
    validate_kind(&kind, 0)?;
    Ok(kind)
}

fn validate_kind(kind: &Kind, depth: usize) -> Result<(), Error> {
    if depth > 8 {
        return Err(invalid("groups are limited to eight nesting levels"));
    }
    let rect = match kind {
        Kind::Shape { rect, .. }
        | Kind::Text { rect, .. }
        | Kind::Callout { rect, .. }
        | Kind::Highlight { rect, .. }
        | Kind::Blur { rect, .. }
        | Kind::Magnify { rect, .. }
        | Kind::Spotlight { rect, .. }
        | Kind::Stamp { rect, .. }
        | Kind::Image { rect, .. } => Some(rect),
        _ => None,
    };
    if rect.is_some_and(|rect| rect.width() <= 0.0 || rect.height() <= 0.0) {
        return Err(invalid(
            "annotation rectangles must have x1 > x0 and y1 > y0",
        ));
    }
    match kind {
        Kind::Image { .. } => return Err(invalid("use doc.image to load image pixels")),
        Kind::Group { objects } => {
            if objects.len() > MAX_OBJECTS {
                return Err(invalid("too many grouped objects"));
            }
            for object in objects {
                validate_kind(&object.kind, depth + 1)?;
                style(
                    Some(&serde_json::to_value(&object.style).map_err(internal)?),
                    Style::default(),
                )?;
            }
        }
        Kind::Blur { amount, .. } if !(0.0..=128.0).contains(amount) => {
            return Err(invalid("blur amount must be 0..128"))
        }
        Kind::Magnify { zoom, .. } if !(1.0..=16.0).contains(zoom) => {
            return Err(invalid("magnification must be 1..16"))
        }
        Kind::Spotlight { dim, blur, .. }
            if !(0.0..=1.0).contains(dim) || !(0.0..=128.0).contains(blur) =>
        {
            return Err(invalid("spotlight dim must be 0..1 and blur 0..128"))
        }
        Kind::Step { radius, .. } if !(1.0..=512.0).contains(radius) => {
            return Err(invalid("step radius must be 1..512"))
        }
        Kind::Pen { points } | Kind::Polygon { points } if points.len() < 2 => {
            return Err(invalid("a path needs at least two points"))
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn annotation_parser_accepts_shared_text_schema_and_rejects_bad_geometry() {
        let value = json!({
            "type": "Text",
            "rect": {"x0": 2, "y0": 3, "x1": 40, "y1": 20},
            "text": "Open settings"
        });
        assert!(
            matches!(kind(&value).unwrap(), Kind::Text { text, .. } if text == "Open settings")
        );
        let invalid = json!({
            "type": "Text",
            "rect": {"x0": 40, "y0": 3, "x1": 2, "y1": 20},
            "text": "Open settings"
        });
        assert_eq!(kind(&invalid).unwrap_err().code, ErrorCode::InvalidRequest);
        assert!(kind(&json!({"type": "Pen", "points": [{"x": 1, "y": 2}]})).is_err());
    }

    #[test]
    fn style_overrides_preserve_defaults_and_reject_invalid_fields() {
        let base = Style::default();
        let parsed = style(Some(&json!({"font_size": 24})), base.clone()).unwrap();
        assert_eq!(parsed.font_size, 24.0);
        assert_eq!(parsed.stroke, base.stroke);
        assert!(style(Some(&json!({"font_size": 0})), base.clone()).is_err());
        assert!(style(Some(&json!({"unknown": 1})), base).is_err());
        assert!(validate_json(&json!({"x": 1_000_001})).is_err());
    }
}
