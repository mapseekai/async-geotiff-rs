#[test]
fn mode_warp_uses_exact_rotated_and_sheared_footprints() {
    for (values, transform, bbox) in [
        (
            [9., 2., 9., 3., 1., 4., 9., 5., 9.],
            [0., 1., 1., 0., -1., 1.],
            Bbox::new(2., -1., 4., 1.),
        ),
        (
            [9., 1., 9., 2., 1., 9., 2., 9., 9.],
            [0., 1., 1. / 3., 0., 0., 1.],
            Bbox::new(1., 0., 2., 3.),
        ),
    ] {
        let src = ImageData {
            pixels: PixelData::F64(Array3::from_shape_vec((1, 3, 3), values.to_vec()).unwrap()),
            mask: None,
            nodata: None,
            crs: Crs::epsg(3857),
            transform: GeoTransform::from_gdal(&transform),
            bbox: Bbox::new(0., -3., 6., 3.),
        };
        let out = warp(&src, &src.crs, bbox, 1, 1, Resampling::Mode).unwrap();
        assert_eq!(out.pixels.to_f64()[[0, 0, 0]], 1.);
        assert!(out.mask.unwrap()[[0, 0]]);
    }
}

#[test]
fn native_selected_nearest_matches_f64_for_nodata_masks_and_projection() {
    let values = Array3::from_shape_fn((3, 4, 5), |(b, r, c)| (b * 40 + r * 5 + c) as u16);
    for strided in [false, true] {
        let values = if strided {
            values.clone().permuted_axes([0, 2, 1])
        } else {
            values.clone()
        };
        for pixels in [
            PixelData::U8(values.mapv(|v| v as u8)),
            PixelData::U16(values.clone()),
        ] {
            for nodata in [
                None,
                Some(0.),
                Some(47.),
                Some(47.5),
                Some(-1.),
                Some(f64::NAN),
                Some(f64::INFINITY),
            ] {
                for masked in [false, true] {
                    let (h, w) = (pixels.height(), pixels.width());
                    let src = ImageData {
                        pixels: pixels.clone(),
                        mask: masked
                            .then(|| Array2::from_shape_fn((h, w), |(r, c)| (r + c) % 3 != 0)),
                        nodata,
                        crs: Crs::epsg(3857),
                        bbox: Bbox::new(0., 0., w as f64, h as f64),
                        transform: GeoTransform::from_gdal(&[0., 1., 0., h as f64, 0., -1.]),
                    };
                    let reference = ImageData {
                        pixels: PixelData::F64(src.pixels.to_f64()),
                        ..src.clone()
                    };
                    for (crs, bbox) in [
                        (
                            src.crs.clone(),
                            Bbox::new(-0.5, -0.5, w as f64 + 0.5, h as f64 + 0.5),
                        ),
                        (
                            Crs::epsg(4326),
                            Bbox::new(-0.00001, -0.00001, 0.00006, 0.00006),
                        ),
                    ] {
                        let actual = warp_selected(
                            &src,
                            &[2, 0, 2],
                            &crs,
                            bbox,
                            16,
                            16,
                            Resampling::Nearest,
                        )
                        .unwrap();
                        let expected = warp_selected(
                            &reference,
                            &[2, 0, 2],
                            &crs,
                            bbox,
                            16,
                            16,
                            Resampling::Nearest,
                        )
                        .unwrap();
                        let max = if matches!(pixels, PixelData::U8(_)) {
                            255.
                        } else {
                            65535.
                        };
                        let expected = expected
                            .pixels
                            .to_f64()
                            .mapv(|v| v.round().clamp(0., max) as u16 as f64);
                        assert_eq!(actual.pixels.to_f64(), expected);
                        assert_eq!(
                            actual.mask,
                            warp_selected(
                                &reference,
                                &[2, 0, 2],
                                &crs,
                                bbox,
                                16,
                                16,
                                Resampling::Nearest
                            )
                            .unwrap()
                            .mask
                        );
                        assert!(actual.nodata.is_none());
                    }
                }
            }
        }
    }
}

use super::*;
use ndarray::Array3;

#[test]
fn average_warp_integrates_rotated_pixel_footprint() {
    let mut src = dummy_src(Crs::epsg(3857));
    src.pixels = PixelData::F64(Array3::from_shape_fn((1, 3, 3), |(_, r, c)| {
        if r == 1 && c == 1 {
            100.0
        } else {
            10.0
        }
    }));
    src.transform = GeoTransform::from_gdal(&[-1.0, 0.5, 0.5, 0.5, 0.5, -0.5]);
    let out = warp(
        &src,
        &src.crs,
        Bbox::new(0.0, 0.0, 1.0, 1.0),
        1,
        1,
        Resampling::Average,
    )
    .unwrap();
    assert!((out.pixels.to_f64()[[0, 0, 0]] - 55.0).abs() < 1e-9);
}

#[test]
fn boundary_warp_samples_pixel_centers() {
    let mut src = dummy_src(Crs::epsg(3857));
    src.pixels = PixelData::F64(Array3::from_shape_fn((1, 4, 4), |(_, r, c)| {
        (r * 40 + c * 10) as f64
    }));
    // One extra column forces the scalar path. The first four columns
    // still have exactly the source pixel centers.
    for kernel in [Resampling::Nearest, Resampling::Bilinear] {
        let out = warp(&src, &src.crs, Bbox::new(0.0, 0.0, 5.0, 4.0), 5, 4, kernel).unwrap();
        let values = out.pixels.to_f64();
        assert_eq!(values[[0, 0, 0]], 0.0, "{kernel:?}");
        assert_eq!(values[[0, 3, 3]], 150.0, "{kernel:?}");
        assert!(out.mask.as_ref().unwrap()[[3, 3]]);
        assert!(!out.mask.as_ref().unwrap()[[0, 4]]);
    }
}

#[test]
fn warp_rejects_empty_target_grid() {
    let src = dummy_src(Crs::epsg(3857));
    assert!(warp(&src, &src.crs, src.bbox, 0, 4, Resampling::Nearest).is_err());
}

#[test]
fn average_single_pixel_covers_source() {
    let mut src = dummy_src(Crs::epsg(3857));
    src.pixels = PixelData::F64(Array3::from_shape_fn((1, 4, 4), |(_, r, c)| {
        (r * 4 + c) as f64
    }));
    let out = warp(&src, &src.crs, src.bbox, 1, 1, Resampling::Average).unwrap();
    assert_eq!(out.pixels.to_f64()[[0, 0, 0]], 7.5);
}

#[test]
fn interpolated_valid_support_sets_output_mask() {
    let mut src = dummy_src(Crs::epsg(3857));
    src.pixels = PixelData::F64(Array3::from_elem((1, 4, 4), 100.0));
    src.mask = Some(Array2::from_shape_fn((4, 4), |(_, c)| c != 0));
    let out = warp(
        &src,
        &src.crs,
        Bbox::new(0.25, 2.0, 1.25, 3.0),
        1,
        1,
        Resampling::Bilinear,
    )
    .unwrap();
    assert_eq!(out.pixels.to_f64()[[0, 0, 0]], 100.0);
    assert!(out.mask.unwrap()[[0, 0]]);
}

#[test]
fn zero_weight_nan_neighbor_does_not_pollute_warp() {
    let mut src = dummy_src(Crs::epsg(3857));
    src.pixels = PixelData::F32(Array3::from_shape_fn((1, 4, 4), |(_, _, c)| {
        if c == 1 {
            f32::NAN
        } else {
            100.0
        }
    }));
    let out = warp(&src, &src.crs, src.bbox, 4, 4, Resampling::Bilinear).unwrap();
    assert_eq!(out.pixels.to_f64()[[0, 0, 0]], 100.0);
}

#[test]
fn boundary_nearest_does_not_resize_clipped_extent() {
    let mut src = dummy_src(Crs::epsg(3857));
    src.pixels = PixelData::U8(Array3::from_shape_fn((1, 4, 4), |(_, _, c)| c as u8));
    let out = warp(
        &src,
        &src.crs,
        Bbox::new(-0.5, 0.0, 4.5, 4.0),
        5,
        4,
        Resampling::Nearest,
    )
    .unwrap();
    assert_eq!(
        out.pixels.to_f64().slice(ndarray::s![0, 0, ..]).to_vec(),
        vec![0.0, 1.0, 2.0, 3.0, 3.0]
    );
}

#[test]
fn warp_rejects_mask_shape_mismatch() {
    let mut src = dummy_src(Crs::epsg(3857));
    src.mask = Some(Array2::from_elem((1, 1), true));
    assert!(warp(&src, &src.crs, src.bbox, 4, 4, Resampling::Nearest).is_err());
}

#[cfg(feature = "proj")]
#[test]
fn warp_keeps_valid_part_of_projection_domain() {
    let mut src = dummy_src(Crs::epsg(3413));
    src.pixels = PixelData::F64(Array3::from_elem((1, 4, 4), 100.0));
    src.transform = GeoTransform::from_gdal(&[-2e6, 1e6, 0.0, 2e6, 0.0, -1e6]);
    let out = warp(
        &src,
        &Crs::epsg(4326),
        Bbox::new(-10.0, 80.0, 10.0, 100.0),
        10,
        10,
        Resampling::Nearest,
    )
    .unwrap();
    assert!(out.mask.unwrap().iter().any(|&v| v));
}

#[test]
fn flipped_source_preserves_world_orientation() {
    let mut src = dummy_src(Crs::epsg(3857));
    src.pixels = PixelData::U8(Array3::from_shape_fn((1, 4, 4), |(_, _, c)| c as u8 * 10));
    src.transform = GeoTransform::from_gdal(&[4.0, -1.0, 0.0, 4.0, 0.0, -1.0]);
    let out = warp(&src, &src.crs, src.bbox, 4, 4, Resampling::Nearest).unwrap();
    assert_eq!(out.pixels.to_f64()[[0, 0, 0]], 30.0);
    assert_eq!(out.pixels.to_f64()[[0, 0, 3]], 0.0);
}

#[test]
fn nearest_warp_preserves_wide_numeric_precision() {
    let mut src = dummy_src(Crs::epsg(3857));
    src.pixels = PixelData::U32(Array3::from_elem((1, 4, 4), 16_777_217));
    let out = warp(&src, &src.crs, src.bbox, 4, 4, Resampling::Nearest).unwrap();
    assert_eq!(out.pixels.to_f64()[[0, 0, 0]], 16_777_217.0);
}

#[test]
fn bilinear_warp_excludes_invalid_neighbors() {
    for numeric_nodata in [false, true] {
        let mut src = dummy_src(Crs::epsg(3857));
        src.pixels = PixelData::F32(Array3::from_shape_fn((1, 4, 4), |(_, _, c)| {
            if c == 0 {
                -9999.0
            } else {
                100.0
            }
        }));
        if numeric_nodata {
            src.nodata = Some(-9999.0);
        } else {
            src.mask = Some(Array2::from_shape_fn((4, 4), |(_, c)| c != 0));
        }
        // Source center-index coordinate 0.75: nearest is valid, but
        // interpolation still overlaps the invalid first column.
        let out = warp(
            &src,
            &src.crs,
            Bbox::new(0.75, 2.0, 1.75, 3.0),
            1,
            1,
            Resampling::Bilinear,
        )
        .unwrap();
        assert_eq!(out.pixels.to_f64()[[0, 0, 0]], 100.0);
        assert!(out.mask.unwrap()[[0, 0]]);
    }
}

#[test]
fn interior_and_boundary_paths_use_same_kernel() {
    let mut src = dummy_src(Crs::epsg(3857));
    src.pixels = PixelData::U8(Array3::from_shape_fn((1, 4, 4), |(_, _, c)| {
        if c == 1 {
            100
        } else {
            0
        }
    }));
    for kernel in [
        Resampling::Bilinear,
        Resampling::Cubic,
        Resampling::CubicSpline,
        Resampling::Lanczos,
        Resampling::Average,
    ] {
        let interior = warp(&src, &src.crs, src.bbox, 8, 8, kernel).unwrap();
        let boundary = warp(&src, &src.crs, Bbox::new(0.0, 0.0, 5.0, 4.0), 10, 8, kernel).unwrap();
        assert!(
            (interior.pixels.to_f64()[[0, 3, 3]] - boundary.pixels.to_f64()[[0, 3, 3]]).abs()
                < 1e-9,
            "{kernel:?}"
        );
    }
}

#[test]
fn scalar_warp_preserves_native_display_dtype() {
    let mut src = dummy_src(Crs::epsg(3857));
    src.pixels = PixelData::U8(Array3::from_elem((3, 4, 4), 100));
    let out = warp(&src, &src.crs, src.bbox, 8, 8, Resampling::Bilinear).unwrap();
    assert!(
        matches!(out.pixels, PixelData::U8(_)),
        "U8 styles must not acquire float auto-stretch semantics"
    );
}

#[test]
fn sheared_source_uses_spatial_sampling() {
    let mut src = dummy_src(Crs::epsg(3857));
    src.pixels = PixelData::U8(Array3::from_shape_fn((1, 4, 8), |(_, r, c)| {
        (r * 10 + c) as u8
    }));
    src.transform = GeoTransform::from_gdal(&[0.0, 1.0, 1.0, 4.0, 0.0, -1.0]);
    let out = warp(
        &src,
        &src.crs,
        Bbox::new(4.0, 0.0, 8.0, 4.0),
        4,
        4,
        Resampling::Nearest,
    )
    .unwrap();
    assert_eq!(out.pixels.to_f64()[[0, 0, 0]], 4.0);
    assert_eq!(out.pixels.to_f64()[[0, 3, 0]], 31.0);
}

fn identity_resize_options() -> fast_image_resize::ResizeOptions {
    fast_image_resize::ResizeOptions::new().resize_alg(fast_image_resize::ResizeAlg::Nearest)
}

#[test]
fn resize_bands_preserves_native_multiband_values() {
    let options = identity_resize_options();

    let u8_src = Array3::from_shape_fn((2, 3, 4), |(b, y, x)| (b * 40 + y * 4 + x) as u8);
    let u8_out = resize_bands(&u8_src, fast_image_resize::PixelType::U8, 4, 3, &options).unwrap();
    assert_eq!(u8_out, u8_src);

    let u16_src = Array3::from_shape_fn((2, 3, 4), |(b, y, x)| (b * 1000 + y * 40 + x) as u16);
    let u16_out =
        resize_bands(&u16_src, fast_image_resize::PixelType::U16, 4, 3, &options).unwrap();
    assert_eq!(u16_out, u16_src);

    let f32_src = Array3::from_shape_fn((2, 3, 4), |(b, y, x)| {
        b as f32 * 100.0 + y as f32 * 10.0 + x as f32 + 0.25
    });
    let f32_out =
        resize_bands(&f32_src, fast_image_resize::PixelType::F32, 4, 3, &options).unwrap();
    assert_eq!(f32_out, f32_src);
}

#[test]
fn resize_bands_accepts_non_standard_layout() {
    let src = Array3::from_shape_fn((2, 2, 3), |(b, y, x)| (b * 20 + y * 3 + x) as u8)
        .permuted_axes([0, 2, 1]);
    assert!(!src.is_standard_layout());

    let out = resize_bands(
        &src,
        fast_image_resize::PixelType::U8,
        src.shape()[2],
        src.shape()[1],
        &identity_resize_options(),
    )
    .unwrap();

    assert_eq!(out, src);
}

fn dummy_src(crs: Crs) -> ImageData {
    ImageData {
        pixels: PixelData::F64(Array3::<f64>::zeros((1, 4, 4))),
        mask: None,
        bbox: Bbox::new(0.0, 0.0, 4.0, 4.0),
        crs,
        transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 4.0, 0.0, -1.0]),
        nodata: None,
    }
}

#[test]
fn same_crs_fast_path_clones() {
    let src = dummy_src(Crs::epsg(3857));
    let out = warp(&src, &Crs::epsg(3857), src.bbox, 4, 4, Resampling::Nearest).unwrap();
    assert_eq!(out.crs, src.crs);
}

/// Average over a cross-CRS tile that extends outside the source.
/// The kernel must preserve valid source values and F64 precision.
#[test]
fn average_cross_crs_scalar_path_box_mean() {
    // 8×8 ramp: pixel (y,x) = y*8+x, range [0, 63].
    let arr = Array3::<f64>::from_shape_fn((1, 8, 8), |(_, y, x)| (y * 8 + x) as f64);
    let src = ImageData {
        pixels: PixelData::F64(arr),
        mask: None,
        // EPSG:4326 bbox covering lon 0–8°, lat 0–8°.
        bbox: Bbox::new(0.0, 0.0, 8.0, 8.0),
        crs: Crs::epsg(4326),
        transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 8.0, 0.0, -1.0]),
        nodata: None,
    };

    // Dst bbox in EPSG:3857 covering roughly lon −1°..15°, lat 0°..9°.
    // This deliberately extends outside the source image on both x-edges,
    // forcing try_subtile_affine to return None (out-of-bounds guard).
    //   lon −1° ≈ −111_195 m,  lon 15° ≈ 1_669_792 m
    //   lat  0° = 0 m,          lat  9° ≈ 1_003_750 m
    let dst_bbox = Bbox::new(-111_195.0, 0.0, 1_669_792.0, 1_003_750.0);
    let out = warp(&src, &Crs::epsg(3857), dst_bbox, 4, 4, Resampling::Average).unwrap();

    // F64 source precision must be retained.
    assert!(
        matches!(out.pixels, PixelData::F64(_)),
        "expected scalar fallback (F64 output) but got a different variant"
    );

    // ── Box-mean invariant ───────────────────────────────────────────────
    // Every finite output value must lie in the source value range [0, 63].
    // Out-of-bounds src coords yield fill = nodata.unwrap_or(0.0) = 0.0,
    // which is already inside [0, 63].
    let vals: Vec<f64> = out.pixels.as_f64().iter().copied().collect();
    for &v in vals.iter() {
        assert!(
            (0.0..=63.0).contains(&v),
            "avg {v} out of source range [0, 63]"
        );
    }
    // Guard against silent degradation to fill/constant: at least one
    // interior pixel must be a genuine mid-range average.
    let interior = vals
        .iter()
        .copied()
        .filter(|&v| v > 5.0 && v < 58.0)
        .count();
    assert!(
        interior > 0,
        "expected interior averaged values; got none (averaging may be broken)"
    );
    assert_eq!(out.pixels.height(), 4);
    assert_eq!(out.pixels.width(), 4);
}

#[test]
fn mode_downsample_preserves_class_not_mean() {
    // 4x4 single band: left half class 1, right half class 9.
    let a = ndarray::Array3::from_shape_fn((1, 4, 4), |(_, _, c)| if c < 2 { 1.0 } else { 9.0 });
    let src = ImageData {
        pixels: PixelData::F64(a),
        mask: None,
        crs: Crs::epsg(3857),
        transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 4.0, 0.0, -1.0]),
        nodata: None,
        bbox: Bbox::new(0.0, 0.0, 4.0, 4.0),
    };
    // Downsample 4x4 → 2x2, same CRS. Each output pixel covers a pure
    // 2x2 block (all class 1 or all class 9) → mean (5.0) must NEVER appear.
    let out = warp(&src, &Crs::epsg(3857), src.bbox, 2, 2, Resampling::Mode).unwrap();
    // Preserve F64 categorical values.
    // Value assertions below establish the majority semantics.
    assert!(
        matches!(out.pixels, PixelData::F64(_)),
        "expected F64 categorical output"
    );
    let f = out.pixels.to_f64();
    for v in f.iter() {
        assert!(
            *v == 1.0 || *v == 9.0,
            "mode produced a non-class value {v}"
        );
    }
}

/// Cross-CRS categorical output contains only original source classes.
#[test]
fn mode_cross_crs_preserves_class() {
    // 16×8 single-band categorical source in EPSG:4326:
    //   left 8 columns (lon 0–4°) = class 1
    //   right 8 columns (lon 4–8°) = class 9
    // Warped cross-CRS to EPSG:3857 at 4×4 resolution.
    //
    // The dst bbox is the proper EPSG:3857 rectangle for lon 1–7°, lat 1–7°
    // (a strict subset of the source), so every grid corner projects back
    // inside the 16×8 source pixel space and `try_subtile_affine` *would*
    // return Some(…) for other resamplers — but Mode bypasses it.
    //
    // lon 1° in EPSG:3857 ≈    111 319 m
    // lon 7° in EPSG:3857 ≈    779 236 m
    // lat 1° in EPSG:3857 ≈    111 325 m
    // lat 7° in EPSG:3857 ≈    781 863 m
    let arr = Array3::<f64>::from_shape_fn((1, 8, 16), |(_, _, x)| if x < 8 { 1.0 } else { 9.0 });
    let src = ImageData {
        pixels: PixelData::F64(arr),
        mask: None,
        // lon 0–8°, lat 0–8° in EPSG:4326.
        // GeoTransform: origin at top-left (lon=0, lat=8), 0.5° per pixel.
        bbox: Bbox::new(0.0, 0.0, 8.0, 8.0),
        crs: Crs::epsg(4326),
        transform: GeoTransform::from_gdal(&[0.0, 0.5, 0.0, 8.0, 0.0, -1.0]),
        nodata: None,
    };
    // Dst bbox in EPSG:3857 covering lon 1–7°, lat 1–7° — fully inside src.
    let dst_bbox = Bbox::new(111_319.0, 111_325.0, 779_236.0, 781_863.0);
    let fill = src.nodata.unwrap_or(0.0); // 0.0
    let out = warp(&src, &Crs::epsg(3857), dst_bbox, 4, 4, Resampling::Mode).unwrap();

    assert_eq!(out.pixels.height(), 4);
    assert_eq!(out.pixels.width(), 4);

    // ── Path oracle ──────────────────────────────────────────────────────
    // Preserve the source precision.
    // This trips the moment the bypass is removed (Mode silently takes fr).
    assert!(
        matches!(out.pixels, PixelData::F64(_)),
        "expected F64 categorical output; \
         the `if !matches!(resampling, Resampling::Mode)` bypass may be missing"
    );

    // ── Value oracle ─────────────────────────────────────────────────────
    // Every non-fill output pixel must be exactly one of the two class
    // values. An fr/Nearest path could blend across the class boundary and
    // produce values strictly between 1.0 and 9.0.
    let vals: Vec<f64> = out.pixels.as_f64().iter().copied().collect();
    for &v in vals.iter() {
        if v == fill {
            continue; // edge/nodata fill is acceptable
        }
        assert!(
            v == 1.0 || v == 9.0,
            "mode produced inter-class value {v} (expected only 1.0 or 9.0)"
        );
    }

    // Guard: both classes must appear in the output (not all fill).
    let has_class1 = vals.contains(&1.0);
    let has_class9 = vals.contains(&9.0);
    assert!(has_class1, "class 1 absent from Mode cross-CRS output");
    assert!(has_class9, "class 9 absent from Mode cross-CRS output");
}

#[test]
fn average_cross_crs_downsample_within_source_range() {
    let arr = Array3::<f64>::from_shape_fn((1, 8, 8), |(_, y, x)| (y * 8 + x) as f64);
    let src = ImageData {
        pixels: PixelData::F64(arr),
        mask: None,
        bbox: Bbox::new(0.0, 0.0, 8.0, 8.0),
        crs: Crs::epsg(4326),
        transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 8.0, 0.0, -1.0]),
        nodata: None,
    };
    let out = warp(&src, &Crs::epsg(3857), src.bbox, 4, 4, Resampling::Average).unwrap();
    assert_eq!(out.pixels.height(), 4);
    assert_eq!(out.pixels.width(), 4);
    let vals: Vec<f64> = out.pixels.as_f64().iter().copied().collect();
    for &v in vals.iter() {
        assert!((0.0..=63.0).contains(&v), "avg {v} out of source range");
    }
    // Guard against silent degradation to fill/constant: at least one
    // interior pixel must be a genuine mid-range average.
    let interior = vals
        .iter()
        .copied()
        .filter(|&v| v > 5.0 && v < 58.0)
        .count();
    assert!(
        interior > 0,
        "expected interior averaged values; got none (averaging may be broken)"
    );
}
