//! Public integration tests for src/render/pipeline.rs.
#[path = "../support/raster.rs"]
mod raster;

use async_geotiff::{
    bounds::{Bbox, GeoTransform},
    crs::Crs,
    pixel::{ImageData, PixelData},
    render::pipeline::render,
    style::{params::compile, TileStyle},
};
use ndarray::{s, Array2, Array3};
use raster::pixels;
use serde_json::json;

fn image(pixels: PixelData, mask: Option<Array2<bool>>) -> ImageData {
    let (h, w) = (pixels.height(), pixels.width());
    ImageData {
        pixels,
        mask,
        bbox: Bbox::new(0.0, 0.0, w as f64, h as f64),
        crs: Crs::epsg(4326),
        transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, h as f64, 0.0, -1.0]),
        nodata: None,
    }
}

fn alphas(image: &ImageData, query: &str) -> Vec<u8> {
    let style = compile(&TileStyle::from_raster_query(query).unwrap()).unwrap();
    let bytes = render(image, &style).unwrap();
    image::load_from_memory(&bytes)
        .unwrap()
        .to_rgba8()
        .pixels()
        .map(|p| p[3])
        .collect()
}

#[test]
fn fractional_nodata_does_not_mask_integer_rgb() {
    let img = image(
        PixelData::U8(Array3::from_shape_vec((3, 1, 1), vec![1, 100, 200]).unwrap()),
        None,
    );
    for query in [
        "version=3.0&type=rgb&nodata=1.5&bidx=1&bidx=2&bidx=3",
        "version=3.0&type=rgb&nodata=1.5&bidx=1&bidx=2&bidx=3&color_formula=gamma%20rgb%201",
    ] {
        assert_eq!(alphas(&img, query), vec![255], "{query}");
    }
    assert_eq!(
        alphas(&img, "version=3.0&type=rgb&nodata=1&bidx=1&bidx=2&bidx=3"),
        vec![0]
    );
}

#[test]
fn rgb_render_honors_strided_masks() {
    let base = Array2::from_shape_vec((2, 2), vec![true, false, true, true]).unwrap();
    for (mask, expected) in [
        (base.clone().reversed_axes(), vec![255, 255, 0, 255]),
        (base.slice_move(s![.., ..;-1]), vec![0, 255, 255, 255]),
    ] {
        assert!(!mask.is_standard_layout());
        for pixels in [
            PixelData::U8(Array3::from_elem((3, 2, 2), 100)),
            PixelData::U16(Array3::from_elem((3, 2, 2), 100)),
        ] {
            assert_eq!(
                alphas(
                    &image(pixels, Some(mask.clone())),
                    "version=3.0&type=rgb&bidx=1&bidx=2&bidx=3"
                ),
                expected
            );
        }
    }
}

#[test]
fn generic_render_masks_infinite_nodata() {
    for (sentinel, query) in [
        (f32::INFINITY, "version=3.0&type=gray&bidx=1"),
        (f32::NEG_INFINITY, "version=3.0&type=gray&bidx=1"),
        (f32::NAN, "version=3.0&type=gray&nodata=nan&bidx=1"),
    ] {
        let img = image(
            PixelData::F32(Array3::from_shape_vec((1, 1, 2), vec![sentinel, 10.0]).unwrap()),
            None,
        );
        assert_eq!(alphas(&img, query), vec![0, 255], "{query}");
    }
}

#[test]
fn composite_get_masks_infinite_nodata() {
    for nd in [f64::INFINITY, f64::NEG_INFINITY] {
        let img = image(PixelData::F64(Array3::from_elem((1, 1, 1), nd)), None);
        assert_eq!(
            alphas(
                &img,
                "version=3.0&type=pseudocolor&bidx=1&colormap_name=viridis"
            ),
            vec![0]
        );
    }
}

#[test]
fn defaults_gamma_nodata_and_alpha_are_explicit() {
    let mut style = json!({"version":"3.0","renderer":{"type":"gray","bidx":[1]}});
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
    let intervals = json!({"version":"3.0","renderer":{"type":"pseudocolor","bidx":[1],"colormap":[[[0,1],[255,0,0,255]],[[1,2],[0,255,0,255]]]}});
    assert_eq!(pixels(intervals.clone(), &[1.]), [0, 255, 0, 255]);
    assert_eq!(pixels(intervals, &[2.]), [0, 0, 0, 0]);
    let exact = json!({"version":"3.0","renderer":{"type":"categorized","bidx":[1],"colormap":{"1":[255,0,0,255]}}});
    assert_eq!(pixels(exact.clone(), &[1.]), [255, 0, 0, 255]);
    assert_eq!(pixels(exact, &[1.5]), [0, 0, 0, 0]);
}

#[test]
fn colormap_lookup_preserves_numeric_order_signed_zero_and_interval_gaps() {
    let exact = json!({"version":"3.0","renderer":{"type":"categorized","bidx":[1],
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
    let intervals = json!({"version":"3.0","renderer":{"type":"pseudocolor","bidx":[1],
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
    let style = json!({"version":"3.0","renderer":{"type":"gray","expression":"(b2-b1)/(b2+b1)"},"stretch":{"method":"linear","rescale":[[-1,1]],"range_policy":"transparent"}});
    assert_eq!(pixels(style.clone(), &[1., 3.]), [191, 191, 191, 255]);
    assert_eq!(pixels(style.clone(), &[-1., 1.]), [0, 0, 0, 0]);
    assert_eq!(pixels(style, &[-1., 3.]), [0, 0, 0, 0]);
}

#[test]
fn rgb_expression_solid_invert_and_extreme_ranges() {
    assert_eq!(
        pixels(
            json!({"version":"3.0","renderer":{"type":"rgb","expression":"b3;b2;b1"}}),
            &[10., 20., 30.]
        ),
        [30, 20, 10, 255]
    );
    assert_eq!(
        pixels(
            json!({"version":"3.0","renderer":{"type":"single_color","bidx":[1],"color":"#ff000080"},"opacity":0.5}),
            &[0.]
        ),
        [255, 0, 0, 64]
    );
    assert_eq!(
        pixels(
            json!({"version":"3.0","renderer":{"type":"gray","bidx":[1],"renderer_invert":true}}),
            &[0.]
        ),
        [255, 255, 255, 255]
    );
    assert_eq!(
        pixels(
            json!({"version":"3.0","renderer":{"type":"gray","bidx":[1]},"stretch":{"method":"linear","rescale":[[-1e308,1e308]]}}),
            &[0.]
        ),
        [128, 128, 128, 255]
    );
}
