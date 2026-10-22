//! Public integration tests for src/style/params.rs.
#[path = "../support/raster.rs"]
mod raster;

use async_geotiff::style::{RasterStyle, TileStyle};
use raster::pixels;
use serde_json::{json, Value};

#[test]
fn json_and_q3_share_validation_broadcast_and_pixel_results() {
    let document = json!({"version":"3.0","renderer":{"type":"rgb","bidx":[3,2,1]},"stretch":{"method":"linear","rescale":[[0,100]]}});
    let sdk = RasterStyle::from_json(&document.to_string()).unwrap();
    let query = raster_style::encode_query(&sdk).unwrap();
    let json = TileStyle::from_raster_style(&sdk).unwrap();
    assert_eq!(
        TileStyle::from_raster_query(&query)
            .unwrap()
            .raster_style_json(),
        json.raster_style_json()
    );
    assert_eq!(pixels(document, &[0., 50., 100.]), [255, 128, 0, 255]);
    // Integer-valued JSON floats and Q3 numbers are legal in the SDK.
    assert!(
        TileStyle::from_json(r#"{"version":"3.0","renderer":{"type":"gray","bidx":[1.0]}}"#)
            .is_ok()
    );
}

#[test]
fn unsupported_fields_and_ambiguous_transport_fail_closed() {
    for extra in [
        json!({"statistics":{"scope":"viewport","accuracy":"exact"}}),
        json!({"statistics":{"scope":"dataset","accuracy":"exact","ref":"unbound"}}),
        json!({"effects":{"post_color_formula":[{"op":"invert"}]}}),
        json!({"extensions":[raster_style::TILE_SCHEMA_URL],"raster:tile":{"format":"jpeg"}}),
        json!({"calibration":{"mode":"metadata"}}),
    ] {
        let mut v = json!({"version":"3.0","renderer":{"type":"gray","bidx":[1]}});
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let error = TileStyle::from_json(&v.to_string()).unwrap_err();
        if v.get("raster:tile").is_some() {
            assert!(error.to_string().contains("/raster:tile/format"), "{error}");
        }
    }
    for query in [
        "version=3.0&type=gray&type=gray&bidx=1",
        "version=3.0&type=gray&bidx=1&typo=1",
        "version=3.0&type=gray&expression=b1+b1",
        "version=3.0&type=gray&expression=b1%252Bb1",
    ] {
        assert!(TileStyle::from_raster_query(query).is_err(), "{query}");
    }
    assert!(TileStyle::from_raster_query("version=3.0&type=gray&expression=b1%2Bb1").is_ok());
    assert!(TileStyle::from_json(
        r#"{"version":"3.0","version":"3.0","renderer":{"type":"gray","bidx":[1]}}"#
    )
    .is_err());
}

#[tokio::test]
async fn compile_cache_separates_protocol_options() {
    let a =
        TileStyle::from_json(r#"{"version":"3.0","renderer":{"type":"gray","bidx":[1]}}"#).unwrap();
    let b = TileStyle::from_json(
        r#"{"version":"3.0","renderer":{"type":"gray","bidx":[1]},"opacity":0.5}"#,
    )
    .unwrap();
    let x = async_geotiff::style::params::compile_cached(&a)
        .await
        .unwrap();
    let y = async_geotiff::style::params::compile_cached(&b)
        .await
        .unwrap();
    assert!(!std::sync::Arc::ptr_eq(&x, &y));
    let z = async_geotiff::style::params::compile_cached(&a)
        .await
        .unwrap();
    assert!(std::sync::Arc::ptr_eq(&x, &z));
}

#[test]
fn defaults_and_tile_sizes_use_the_protocol_contract() {
    let style = TileStyle::default();
    let doc: Value = serde_json::from_str(style.raster_style_json()).unwrap();
    assert_eq!(doc["renderer"]["type"], "gray");
    assert_eq!(style.tilesize(), 256);
    assert_eq!(style.format(), async_geotiff::style::TileFormat::Png);
    for size in [64, 128, 256, 512, 1024] {
        assert_eq!(
            TileStyle::from_raster_query(&format!("version=3.0&type=gray&bidx=1&extensions=%5B%22https%3A%2F%2Fmapseekai.github.io%2Fraster-style%2Fextensions%2Fraster%2Ftile%2Fv1.0.0%2Fschema.json%22%5D&extension_fields=%7B%22raster%3Atile%22%3A%7B%22tilesize%22%3A{size}%7D%7D"))
                .unwrap()
                .tilesize(),
            size
        );
    }
    for size in [0, 96, 2048, 4096] {
        assert!(TileStyle::from_raster_query(&format!(
            "version=3.0&type=gray&bidx=1&extensions=%5B%22https%3A%2F%2Fmapseekai.github.io%2Fraster-style%2Fextensions%2Fraster%2Ftile%2Fv1.0.0%2Fschema.json%22%5D&extension_fields=%7B%22raster%3Atile%22%3A%7B%22tilesize%22%3A{size}%7D%7D"
        ))
        .is_err());
    }
    for query in [
        "",
        "bidx=1",
        "stretch=stddev",
        "version=3.0&type=gray&bidx=1&unscale=true",
    ] {
        assert!(TileStyle::from_raster_query(query).is_err());
    }
}

#[test]
fn all_stretch_methods_round_trip_and_curve_renders_in_display_space() {
    for stretch in [
        json!({"method":"none"}),
        json!({"method":"linear","rescale":[[0,20]]}),
        json!({"method":"minmax"}),
        json!({"method":"percentile","percentiles":[2,98]}),
        json!({"method":"stddev","stddev":2}),
        json!({"method":"histogram_equalization"}),
        json!({"method":"curve","curves":[[[0,0.2],[20,0.8]]]}),
    ] {
        let doc = json!({"version":"3.0","renderer":{"type":"gray","bidx":[1]},"stretch":stretch});
        let sdk = RasterStyle::from_json(&doc.to_string()).unwrap();
        let query = raster_style::encode_query(&sdk).unwrap();
        assert_eq!(
            TileStyle::from_json(&doc.to_string())
                .unwrap()
                .raster_style_json(),
            TileStyle::from_raster_query(&query)
                .unwrap()
                .raster_style_json()
        );
    }
    let curve = json!({"version":"3.0","renderer":{"type":"rgb","bidx":[1,2,3]},"stretch":{"method":"curve","curves":[[[0,0.2],[20,0.8]]]}});
    assert_eq!(pixels(curve.clone(), &[-1., 10., 21.]), [51, 128, 204, 255]);
    let mut transparent = curve;
    transparent["stretch"]["range_policy"] = json!("transparent");
    assert_eq!(pixels(transparent, &[-1., 10., 21.]), [0; 4]);
}

#[test]
fn extensions_require_complete_validation_and_exact_executor_support() {
    use raster_style::{SchemaRegistry, MOSAIC_SCHEMA_URL, TILE_SCHEMA_URL};

    let core = json!({"version":"3.0","renderer":{"type":"gray","bidx":[1]}});
    let mut tile = core.clone();
    tile["extensions"] = json!([TILE_SCHEMA_URL]);
    tile["raster:tile"] = json!({"format":"png","tilesize":64});
    let sdk = RasterStyle::from_value(tile.clone()).unwrap();
    let query = raster_style::encode_query(&sdk).unwrap();
    assert_eq!(TileStyle::from_raster_query(&query).unwrap().tilesize(), 64);
    tile.as_object_mut().unwrap().remove("extensions");
    assert!(TileStyle::from_json(&tile.to_string()).is_err());

    let mut mosaic = core.clone();
    mosaic["extensions"] = json!([MOSAIC_SCHEMA_URL]);
    mosaic["raster:mosaic"] = json!({"pixel_selection":"first","stage":"before_channels"});
    assert_eq!(
        TileStyle::from_json(&mosaic.to_string()).is_ok(),
        cfg!(feature = "mosaic")
    );

    let url = "https://example.test/vendor/v1.0.0/schema.json";
    let mut vendor = core;
    vendor["extensions"] = json!([url]);
    vendor["vendor:gain"] = json!(2);
    let unresolved = RasterStyle::from_value(vendor.clone()).unwrap();
    assert!(!unresolved.validation().complete);
    assert!(TileStyle::from_raster_style(&unresolved).is_err());
    let mut registry = SchemaRegistry::new().unwrap();
    registry
        .register(json!({"$id":url,"type":"object","properties":{"vendor:gain":{"type":"number"}}}))
        .unwrap();
    let validated = registry.parse_json(&vendor.to_string()).unwrap();
    assert!(validated.validation().complete);
    assert!(TileStyle::from_raster_style(&validated).is_err());

    for legacy in [
        "version=2.0&type=gray&bidx=1",
        "version=3.0&type=gray&bidx=1&tilesize=64",
        "version=3.0&type=gray&bidx=1&format=png",
        "version=3.0&type=gray&bidx=1&pixel_selection=first&stage=before_channels",
    ] {
        assert!(TileStyle::from_raster_query(legacy).is_err(), "{legacy}");
    }
}
