use async_geotiff::{
    bounds::{Bbox, GeoTransform},
    crs::Crs,
    pixel::{ImageData, PixelData},
    render::pipeline::render,
    style::{compile, RasterStyle, TileStyle},
};
use ndarray::Array3;
use serde_json::{json, Value};

fn pixels(style: Value, values: &[f64]) -> Vec<u8> {
    let style = TileStyle::from_json(&style.to_string()).unwrap();
    let image = ImageData {
        pixels: PixelData::F64(
            Array3::from_shape_vec((values.len(), 1, 1), values.to_vec()).unwrap(),
        ),
        mask: None,
        nodata: None,
        crs: Crs::epsg(3857),
        bbox: Bbox::new(0., 0., 1., 1.),
        transform: GeoTransform::from_gdal(&[0., 1., 0., 1., 0., -1.]),
    };
    image::load_from_memory(&render(&image, &compile(&style).unwrap()).unwrap())
        .unwrap()
        .to_rgba8()
        .into_raw()
}

#[test]
fn json_and_q2_share_validation_broadcast_and_pixel_results() {
    let document = json!({"version":"2.0","renderer":{"type":"rgb","bidx":[3,2,1]},"stretch":{"method":"linear","rescale":[[0,100]]}});
    let sdk = RasterStyle::from_json(&document.to_string()).unwrap();
    let query = raster_style::encode_query(&sdk).unwrap();
    let json = TileStyle::from_raster_style(&sdk).unwrap();
    let q2 = TileStyle::from_raster_query(&query).unwrap();
    assert_eq!(q2.raster_style_json(), json.raster_style_json());
    assert_eq!(pixels(document, &[0., 50., 100.]), [255, 128, 0, 255]);
    // Integer-valued JSON floats and Q2 numbers are legal in the SDK.
    assert!(
        TileStyle::from_json(r#"{"version":"2.0","renderer":{"type":"gray","bidx":[1.0]}}"#)
            .is_ok()
    );
}

#[test]
fn defaults_gamma_nodata_and_alpha_are_explicit() {
    let mut style = json!({"version":"2.0","renderer":{"type":"gray","bidx":[1]}});
    assert_eq!(pixels(style.clone(), &[1.]), [1, 1, 1, 255]);
    style["stretch"] = json!({"method":"linear","rescale":[[0,1]]});
    style["effects"] = json!({"color_formula":[{"op":"gamma","value":2}]});
    style["opacity"] = json!(0.5);
    assert_eq!(pixels(style.clone(), &[0.25]), [128, 128, 128, 128]);
    style["nodata"] = json!(0.25);
    assert_eq!(pixels(style.clone(), &[0.25]), [0, 0, 0, 0]);
    assert_eq!(pixels(style, &[f64::INFINITY]), [0, 0, 0, 0]);
}

#[test]
fn intervals_are_half_open_and_exact_values_are_not_rounded() {
    let intervals = json!({"version":"2.0","renderer":{"type":"pseudocolor","bidx":[1],"colormap":[[[0,1],[255,0,0,255]],[[1,2],[0,255,0,255]]]}});
    assert_eq!(pixels(intervals.clone(), &[1.]), [0, 255, 0, 255]);
    assert_eq!(pixels(intervals, &[2.]), [0, 0, 0, 0]);
    let exact = json!({"version":"2.0","renderer":{"type":"categorized","bidx":[1],"colormap":{"1":[255,0,0,255]}}});
    assert_eq!(pixels(exact.clone(), &[1.]), [255, 0, 0, 255]);
    assert_eq!(pixels(exact, &[1.5]), [0, 0, 0, 0]);
}

#[test]
fn colormap_lookup_preserves_numeric_order_signed_zero_and_interval_gaps() {
    let exact = json!({"version":"2.0","renderer":{"type":"categorized","bidx":[1],
        "colormap":{"-10":[10,0,0,255],"-2":[20,0,0,255],"0":[30,0,0,255],
                    "2":[40,0,0,255],"10":[50,0,0,255]}}});
    for (value, red) in [
        (-10., 10),
        (-2., 20),
        (-0., 30),
        (0., 30),
        (2., 40),
        (10., 50),
    ] {
        assert_eq!(pixels(exact.clone(), &[value]), [red, 0, 0, 255]);
    }
    for value in [-11., -2.5, 1.5, 11.] {
        assert_eq!(pixels(exact.clone(), &[value]), [0; 4]);
    }
    let intervals = json!({"version":"2.0","renderer":{"type":"pseudocolor","bidx":[1],
        "colormap":[[[-10,-2],[10,0,0,255]],[[0,2],[20,0,0,255]],[[2,10],[30,0,0,255]]]}});
    for (value, red) in [
        (-10., 10),
        (-2.5, 10),
        (-0., 20),
        (1.5, 20),
        (2., 30),
        (9.5, 30),
    ] {
        assert_eq!(pixels(intervals.clone(), &[value]), [red, 0, 0, 255]);
    }
    for value in [-11., -2., -1., 10., 11.] {
        assert_eq!(pixels(intervals.clone(), &[value]), [0; 4]);
    }
}

#[test]
fn expression_and_transparent_range_keep_invalid_pixels_transparent() {
    let style = json!({"version":"2.0","renderer":{"type":"gray","expression":"(b2-b1)/(b2+b1)"},"stretch":{"method":"linear","rescale":[[-1,1]],"range_policy":"transparent"}});
    assert_eq!(pixels(style.clone(), &[1., 3.]), [191, 191, 191, 255]);
    assert_eq!(pixels(style.clone(), &[-1., 1.]), [0, 0, 0, 0]);
    assert_eq!(pixels(style, &[-1., 3.]), [0, 0, 0, 0]);
}

#[test]
fn unsupported_fields_and_ambiguous_transport_fail_closed() {
    for extra in [
        json!({"statistics":{"scope":"viewport","accuracy":"exact"}}),
        json!({"statistics":{"scope":"dataset","accuracy":"exact","ref":"unbound"}}),
        json!({"effects":{"post_color_formula":[{"op":"invert"}]}}),
        json!({"image":{"format":"jpeg"}}),
        json!({"calibration":{"mode":"metadata"}}),
    ] {
        let mut v = json!({"version":"2.0","renderer":{"type":"gray","bidx":[1]}});
        v.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(TileStyle::from_json(&v.to_string()).is_err(), "{v}");
    }
    for query in [
        "version=2.0&type=gray&type=gray&bidx=1",
        "version=2.0&type=gray&bidx=1&typo=1",
        "version=2.0&type=gray&expression=b1+b1",
        "version=2.0&type=gray&expression=b1%252Bb1",
    ] {
        assert!(TileStyle::from_raster_query(query).is_err(), "{query}");
    }
    assert!(TileStyle::from_raster_query("version=2.0&type=gray&expression=b1%2Bb1").is_ok());
    assert!(TileStyle::from_json(
        r#"{"version":"2.0","version":"2.0","renderer":{"type":"gray","bidx":[1]}}"#
    )
    .is_err());
}

#[tokio::test]
async fn compile_cache_separates_protocol_options() {
    let a =
        TileStyle::from_json(r#"{"version":"2.0","renderer":{"type":"gray","bidx":[1]}}"#).unwrap();
    let b = TileStyle::from_json(
        r#"{"version":"2.0","renderer":{"type":"gray","bidx":[1]},"opacity":0.5}"#,
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
fn rgb_expression_solid_invert_and_extreme_ranges() {
    assert_eq!(
        pixels(
            json!({"version":"2.0","renderer":{"type":"rgb","expression":"b3;b2;b1"}}),
            &[10., 20., 30.]
        ),
        [30, 20, 10, 255]
    );
    assert_eq!(
        pixels(
            json!({"version":"2.0","renderer":{"type":"single_color","bidx":[1],"color":"#ff000080"},"opacity":0.5}),
            &[0.]
        ),
        [255, 0, 0, 64]
    );
    assert_eq!(
        pixels(
            json!({"version":"2.0","renderer":{"type":"gray","bidx":[1],"renderer_invert":true}}),
            &[0.]
        ),
        [255, 255, 255, 255]
    );
    assert_eq!(
        pixels(
            json!({"version":"2.0","renderer":{"type":"gray","bidx":[1]},"stretch":{"method":"linear","rescale":[[-1e308,1e308]]}}),
            &[0.]
        ),
        [128, 128, 128, 255]
    );
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
            TileStyle::from_raster_query(&format!("version=2.0&type=gray&bidx=1&tilesize={size}"))
                .unwrap()
                .tilesize(),
            size
        );
    }
    for size in [0, 96, 2048, 4096] {
        assert!(TileStyle::from_raster_query(&format!(
            "version=2.0&type=gray&bidx=1&tilesize={size}"
        ))
        .is_err());
    }
    for query in [
        "",
        "bidx=1",
        "stretch=stddev",
        "version=2.0&type=gray&bidx=1&unscale=true",
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
        let doc = json!({"version":"2.0","renderer":{"type":"gray","bidx":[1]},"stretch":stretch});
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
    let curve = json!({"version":"2.0","renderer":{"type":"rgb","bidx":[1,2,3]},"stretch":{"method":"curve","curves":[[[0,0.2],[20,0.8]]]}});
    assert_eq!(pixels(curve.clone(), &[-1., 10., 21.]), [51, 128, 204, 255]);
    let mut transparent = curve;
    transparent["stretch"]["range_policy"] = json!("transparent");
    assert_eq!(pixels(transparent, &[-1., 10., 21.]), [0; 4]);
}
