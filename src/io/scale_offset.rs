//! Per-band SCALE/OFFSET from a COG's `GDAL_METADATA` XML (TIFF tag 42112).
//!
//! GDAL writes `<Item name="SCALE" sample="0">0.0001</Item>` /
//! `<Item name="OFFSET" sample="0">0</Item>`. Bands without metadata default
//! to identity `(1.0, 0.0)`.

fn items(xml: &str) -> Vec<(String, usize, String)> {
    use quick_xml::events::Event;

    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut out = Vec::new();
    let mut current: Option<(String, usize)> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) if e.name().as_ref() == b"Item" => {
                let mut name = String::new();
                let mut sample = 0usize;
                for attr in e.attributes().flatten() {
                    let Ok(value) = attr.decode_and_unescape_value(reader.decoder()) else {
                        continue;
                    };
                    match attr.key.as_ref() {
                        b"name" => name = value.into_owned(),
                        b"sample" => sample = value.parse().unwrap_or(0),
                        _ => {}
                    }
                }
                current = (!name.is_empty()).then_some((name, sample));
            }
            Ok(Event::Text(e)) => {
                if let Some((name, sample)) = current.take() {
                    if let Ok(value) = e.decode() {
                        out.push((name, sample, value.trim().to_string()));
                    }
                }
            }
            Ok(Event::End(e)) if e.name().as_ref() == b"Item" => {
                current = None;
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
    out
}

/// Returns `bands` entries of `(scale, offset)`, identity where absent.
pub fn parse_scale_offset(gdal_metadata: Option<&str>, bands: usize) -> Vec<(f64, f64)> {
    let parsed = gdal_metadata.map(items).unwrap_or_default();
    let get = |name: &str, b: usize| -> Option<f64> {
        parsed
            .iter()
            .find(|(n, s, _)| n == name && *s == b)
            .and_then(|(_, _, v)| v.parse::<f64>().ok())
    };
    (0..bands)
        .map(|b| {
            (
                get("SCALE", b).unwrap_or(1.0),
                get("OFFSET", b).unwrap_or(0.0),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = r#"<GDALMetadata>
      <Item name="SCALE" sample="0">0.0001</Item>
      <Item name="OFFSET" sample="0">-0.1</Item>
      <Item name="SCALE" sample="1">2.0</Item>
    </GDALMetadata>"#;

    #[test]
    fn parses_per_band_scale_offset() {
        let so = parse_scale_offset(Some(XML), 3);
        assert_eq!(so.len(), 3);
        assert_eq!(so[0], (0.0001, -0.1));
        assert_eq!(so[1], (2.0, 0.0)); // offset absent → 0
        assert_eq!(so[2], (1.0, 0.0)); // band absent → identity
    }

    #[test]
    fn none_metadata_is_all_identity() {
        assert_eq!(parse_scale_offset(None, 2), vec![(1.0, 0.0), (1.0, 0.0)]);
    }

    #[test]
    fn malformed_xml_is_identity() {
        assert_eq!(parse_scale_offset(Some("<not xml"), 1), vec![(1.0, 0.0)]);
    }
}
