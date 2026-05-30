//! Parse titiler-style `colormap=` JSON into the library's `ColourDefinition`.
//!
//! Routing is by JSON shape, NOT by serde's untagged ordering (which is
//! first-variant-wins and fragile): a leading `{` is a discrete value→colour
//! map; a leading `[` is a list of `[value, colour]` gradient breaks.

use crate::error::{AgError, AgResult};
use crate::render::colour::Colour;
use crate::render::composite::ColourDefinition;

/// Route titiler `colormap=` JSON to a `ColourDefinition` by top-level shape.
#[allow(dead_code)]
pub fn parse_colormap_param(s: &str) -> AgResult<ColourDefinition> {
    let head = s.trim_start().chars().next();
    match head {
        Some('{') => {
            // Discrete: { "<int>": [r,g,b,a], ... }
            let map: std::collections::BTreeMap<String, Colour> = serde_json::from_str(s)
                .map_err(|e| AgError::query("colormap", format!("discrete colormap: {e}")))?;
            let mut pairs = Vec::with_capacity(map.len());
            for (k, c) in map {
                let key: i64 = k
                    .parse()
                    .map_err(|_| AgError::query("colormap", format!("key {k:?} is not an integer")))?;
                pairs.push((key, c));
            }
            Ok(ColourDefinition::Discrete(pairs))
        }
        Some('[') => {
            // Gradient: [ [value, [r,g,b,a]], ... ]
            let breaks: Vec<(f64, Colour)> = serde_json::from_str(s)
                .map_err(|e| AgError::query("colormap", format!("gradient colormap: {e}")))?;
            Ok(ColourDefinition::ColoursAndBreaks(breaks))
        }
        _ => Err(AgError::query(
            "colormap",
            "expected a JSON object (discrete) or array (gradient)",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_discrete_object() {
        let def = parse_colormap_param(r#"{"1":[255,0,0,255],"2":[0,255,0,255]}"#).unwrap();
        match def {
            ColourDefinition::Discrete(pairs) => {
                assert!(pairs.iter().any(|(k, _)| *k == 1));
                assert!(pairs.iter().any(|(k, _)| *k == 2));
                assert_eq!(pairs.len(), 2);
            }
            other => panic!("expected Discrete, got {other:?}"),
        }
    }

    #[test]
    fn parses_gradient_array() {
        let def =
            parse_colormap_param(r#"[[0.0,[255,0,0,255]],[1.0,[0,0,255,255]]]"#).unwrap();
        match def {
            ColourDefinition::ColoursAndBreaks(breaks) => {
                assert_eq!(breaks.len(), 2);
                assert_eq!(breaks[0].0, 0.0);
                assert_eq!(breaks[1].0, 1.0);
            }
            other => panic!("expected ColoursAndBreaks, got {other:?}"),
        }
    }

    #[test]
    fn malformed_json_is_query_error() {
        let err = parse_colormap_param("{not json").unwrap_err();
        assert!(matches!(err, AgError::Query { .. }));
    }

    #[test]
    fn non_object_non_array_is_query_error() {
        let err = parse_colormap_param("\"viridis\"").unwrap_err();
        assert!(matches!(err, AgError::Query { .. }));
    }

    #[test]
    fn discrete_non_integer_key_is_query_error() {
        let err = parse_colormap_param(r#"{"abc":[1,2,3,4]}"#).unwrap_err();
        assert!(matches!(err, AgError::Query { .. }));
    }
}
