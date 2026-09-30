use super::*;
use crate::style::TileStyle;
use crate::{
    bounds::{Bbox, GeoTransform},
    crs::Crs,
};
use serde_json::Value;

#[test]
fn public_render_checks_shapes_and_preserves_strided_masks() {
    use crate::{render::pipeline, style::compile};
    for query in [
        "version=2.0&type=gray&bidx=1",
        "version=2.0&type=rgb&bidx=1&bidx=2&bidx=3",
    ] {
        let compiled = compile(&TileStyle::from_raster_query(query).unwrap()).unwrap();
        let mut image = ImageData {
            pixels: PixelData::U8(Array3::from_elem((3, 2, 2), 10)),
            mask: None,
            bbox: crate::bounds::Bbox::new(0., 0., 2., 2.),
            crs: crate::crs::Crs::epsg(3857),
            transform: crate::bounds::GeoTransform::from_gdal(&[0., 1., 0., 2., 0., -1.]),
            nodata: None,
        };
        for shape in [(1, 1), (3, 2), (2, 3)] {
            image.mask = Some(Array2::from_elem(shape, true));
            assert!(matches!(
                pipeline::render(&image, &compiled),
                Err(AgError::Query { .. })
            ));
        }
        let mask = ndarray::array![[true, false], [true, true]].reversed_axes();
        image.mask = Some(mask.as_standard_layout().into_owned());
        let expected = pipeline::render(&image, &compiled).unwrap();
        image.mask = Some(mask);
        assert_eq!(pipeline::render(&image, &compiled).unwrap(), expected);
        if query.contains("type=rgb") {
            image.pixels = PixelData::U8(Array3::zeros((1, 2, 2)));
            assert!(pipeline::render(&image, &compiled).is_err());
        }
        for shape in [(0, 2, 2), (3, 0, 2), (3, 2, 0)] {
            image.mask = None;
            image.pixels = PixelData::U8(Array3::zeros(shape));
            assert!(pipeline::render(&image, &compiled).is_err());
        }
    }
}

#[test]
fn native_full_band_warp_matches_selected_copy_for_masks_and_edges() {
    for pixels in [
        PixelData::U8(Array3::from_shape_fn((3, 8, 8), |(b, r, c)| {
            (b * 64 + r * 8 + c) as u8
        })),
        PixelData::U16(Array3::from_shape_fn((3, 8, 8), |(b, r, c)| {
            (b * 640 + r * 80 + c) as u16
        })),
    ] {
        let src = ImageData {
            pixels,
            mask: Some(Array2::from_shape_fn((8, 8), |(r, c)| (r + c) % 5 != 0)),
            bbox: Bbox::new(0., 0., 8., 8.),
            crs: Crs::epsg(3857),
            transform: GeoTransform::from_gdal(&[0., 1., 0., 8., 0., -1.]),
            nodata: None,
        };
        let copied = ImageData {
            pixels: match &src.pixels {
                PixelData::U8(a) => PixelData::U8(a.select(Axis(0), &[0, 1, 2])),
                PixelData::U16(a) => PixelData::U16(a.select(Axis(0), &[0, 1, 2])),
                _ => unreachable!(),
            },
            mask: src.mask.clone(),
            bbox: src.bbox,
            crs: src.crs.clone(),
            transform: src.transform,
            nodata: None,
        };
        let style = TileStyle::from_raster_query(
            "version=2.0&type=rgb&bidx=3&bidx=1&bidx=2&method=linear&rescale=0,2000",
        )
        .unwrap();
        for bbox in [
            Bbox::new(0.25, 0.25, 7.75, 7.75),
            Bbox::new(-1., -1., 7., 7.),
        ] {
            let actual = style
                .raster
                .warp_dependencies(&src, &src.crs, bbox, 16, Resampling::Nearest)
                .unwrap();
            let expected =
                crate::warp::warp(&copied, &src.crs, bbox, 16, 16, Resampling::Nearest).unwrap();
            assert_eq!(actual.mask, expected.mask);
            assert_eq!(actual.pixels.as_f64(), expected.pixels.as_f64());
            assert_eq!(
                style
                    .raster
                    .compact()
                    .render(&actual, None, TileFormat::Png)
                    .unwrap(),
                style
                    .raster
                    .compact()
                    .render(&expected, None, TileFormat::Png)
                    .unwrap()
            );
        }
    }
}

#[test]
fn reciprocal_rgb_quantization_matches_division_at_rounding_edges() {
    for (lo, hi) in [
        (0., 255.),
        (-10., 10.),
        (1e100, 2e100),
        (0., 1e308),
        (-1e308, 1e308),
        (0., f64::from_bits(1)),
        (127., 127.),
    ] {
        let range = (lo, hi, hi - lo, 255. / (hi - lo));
        let transfer = Transfer::Range(lo, hi);
        let check = |v: f64| {
            assert_eq!(
                range_byte(v, range),
                byte(transfer.apply(v, false).unwrap()),
                "{lo}..{hi}, {v}"
            )
        };
        for v in [lo, hi, 0., -f64::MAX, f64::MAX] {
            check(v);
        }
        if (hi - lo).is_finite() && hi != lo {
            for i in 0..255 {
                for delta in [
                    -1e-8, -1.01e-10, -1e-10, -0.99e-10, -1e-14, 0., 1e-14, 1e-10, 1e-8,
                ] {
                    let v = lo + (hi - lo) * ((i as f64 + 0.5 + delta) / 255.);
                    check(v);
                    check(v.next_up());
                    check(v.next_down());
                }
            }
        }
        let mut bits = 37u64;
        for _ in 0..10000 {
            bits = bits.wrapping_mul(6364136223846793005).wrapping_add(1);
            let v = f64::from_bits(bits);
            if v.is_finite() {
                check(v);
            }
        }
    }
}

#[test]
fn direct_samples_match_expression_fallback_and_float_ranges() {
    let source = Array3::from_shape_fn((3, 3, 4), |(b, r, c)| {
        if r == b && c == b {
            f64::NAN
        } else {
            (r * 4 + c + b * 20) as f64
        }
    });
    let plan =
        TileStyle::from_raster_query("version=2.0&type=rgb&bidx=3&bidx=1&bidx=3&method=minmax")
            .unwrap();
    for pixels in [
        PixelData::U8(source.mapv(|v| v as u8)),
        PixelData::U16(source.mapv(|v| v as u16)),
        PixelData::I16(source.mapv(|v| v as i16)),
        PixelData::U32(source.mapv(|v| v as u32)),
        PixelData::I32(source.mapv(|v| v as i32)),
        PixelData::F32(source.mapv(|v| v as f32)),
        PixelData::F64(source.clone()),
        PixelData::F64(source.clone().permuted_axes([0, 2, 1])),
    ] {
        let mut image = ImageData {
            pixels,
            mask: None,
            nodata: None,
            crs: Crs::epsg(4326),
            bbox: Bbox::new(0., 0., 4., 3.),
            transform: GeoTransform::from_gdal(&[0., 1., 0., 3., 0., -1.]),
        };
        for masked in [false, true] {
            if masked {
                image.mask = Some(Array2::from_shape_fn(
                    (image.pixels.height(), image.pixels.width()),
                    |(r, c)| (r + c) % 3 != 0,
                ));
            }
            for k in [0, 1, 5, 12, 100] {
                for ready in [false, true] {
                    let mut direct = (*plan.raster).clone();
                    direct.statistics.sample_size = Some(k);
                    if ready {
                        direct.inputs = (0..3).map(|b| vec![expression::Token::Band(b)]).collect();
                        direct.dependencies = (0..3).collect();
                    }
                    let mut fallback = direct.clone();
                    for tokens in &mut fallback.inputs {
                        tokens.extend([expression::Token::Num(0.), expression::Token::Add]);
                    }
                    assert_eq!(
                        direct.statistics_samples(&image, ready),
                        fallback.statistics_samples(&image, false)
                    );
                }
            }
        }
    }
    for (lo, hi) in [
        (-1e308, 1e308),
        (0., f64::from_bits(1)),
        (127., 127.),
        (-10., 10.),
    ] {
        let values = [lo, hi, 0., -1., 1., 127., 127.5, f64::NAN];
        let image = ImageData {
            pixels: PixelData::F64(Array3::from_shape_fn((3, 1, values.len()), |(_, _, c)| {
                values[c]
            })),
            mask: None,
            nodata: None,
            crs: Crs::epsg(4326),
            bbox: Bbox::new(0., 0., 8., 1.),
            transform: GeoTransform::from_gdal(&[0., 1., 0., 1., 0., -1.]),
        };
        let mut direct = (*plan.raster).clone();
        direct.stretch = Stretch::Resolved(vec![Transfer::Range(lo, hi); 3]);
        let mut fallback = direct.clone();
        for tokens in &mut fallback.inputs {
            tokens.extend([expression::Token::Num(0.), expression::Token::Add]);
        }
        let decode = |bytes: Bytes| {
            image::load_from_memory(&bytes)
                .unwrap()
                .to_rgba8()
                .into_raw()
        };
        assert_eq!(
            decode(direct.render(&image, None, TileFormat::Png).unwrap()),
            decode(fallback.render(&image, None, TileFormat::Png).unwrap())
        );
    }
}

#[test]
fn chunked_float_rgb_matches_expression_renderer() {
    let source = Array3::from_shape_fn((3, 257, 257), |(b, r, c)| match (r * 257 + c + b) % 101 {
        0 => f64::NAN,
        1 => f64::INFINITY,
        2 => f64::NEG_INFINITY,
        3 => 7.,
        _ => (r as f64 - c as f64) * 1.25 + b as f64 + 0.5,
    });
    let style = TileStyle::from_raster_query(
        "version=2.0&type=rgb&bidx=3&bidx=1&bidx=3&method=linear&rescale=0,255",
    )
    .unwrap();
    for pixels in [
        PixelData::F64(source.clone()),
        PixelData::F32(source.mapv(|v| v as f32)),
        PixelData::U8(source.mapv(|v| v as u8)),
        PixelData::U16(source.mapv(|v| v as u16)),
    ] {
        for transparent in [false, true] {
            for nodata in [None, Some(7.)] {
                let mut plan = (*style.raster).clone();
                plan.transparent = transparent;
                plan.opacity = 0.5;
                let mut generic = plan.clone();
                for tokens in &mut generic.inputs {
                    tokens.extend([expression::Token::Num(0.), expression::Token::Add]);
                }
                let image = ImageData {
                    pixels: pixels.clone(),
                    mask: Some(Array2::from_shape_fn((257, 257), |(r, c)| {
                        (r + c) % 13 != 0
                    })),
                    nodata,
                    crs: Crs::epsg(3857),
                    bbox: Bbox::new(0., 0., 257., 257.),
                    transform: GeoTransform::from_gdal(&[0., 1., 0., 257., 0., -1.]),
                };
                assert_eq!(
                    plan.render(&image, nodata, TileFormat::Png).unwrap(),
                    generic.render(&image, nodata, TileFormat::Png).unwrap(),
                );
            }
        }
    }
}

#[test]
fn rgb_fast_path_matches_expression_renderer_for_types_masks_and_ranges() {
    let source = Array3::from_shape_fn((3, 3, 3), |(b, r, c)| {
        [0., 1., 2., 127., 128., 200., 255., 65535., f64::NAN][r * 3 + c] + b as f64
    });
    let pixels = [
        PixelData::U8(source.mapv(|v| v as u8)),
        PixelData::U16(source.mapv(|v| v as u16)),
        PixelData::I16(source.mapv(|v| v as i16)),
        PixelData::U32(source.mapv(|v| v as u32)),
        PixelData::I32(source.mapv(|v| v as i32)),
        PixelData::F32(source.mapv(|v| v as f32)),
        PixelData::F64(source.clone()),
        PixelData::F64(source.permuted_axes([0, 2, 1])),
    ];
    for pixels in pixels {
        for (range, transparent, opacity) in [
            ("0,255", false, 1.),
            ("2,200", true, 0.5),
            ("127,127", false, 1.),
            ("0,255", true, 0.),
        ] {
            let valid_range = if range == "127,127" { "0,255" } else { range };
            let mut plan = (*TileStyle::from_raster_query(&format!(
                "version=2.0&type=rgb&bidx=3&bidx=1&bidx=3&method=linear&rescale={valid_range}"
            ))
            .unwrap()
            .raster)
                .clone();
            if range == "127,127" {
                plan.stretch = Stretch::Resolved(vec![Transfer::Range(127., 127.); 3]);
            }
            plan.transparent = transparent;
            plan.opacity = opacity;
            let mut generic = plan.clone();
            for tokens in &mut generic.inputs {
                tokens.extend([expression::Token::Num(0.), expression::Token::Add]);
            }
            for nodata in [None, Some(128.), Some(128.5)] {
                for strided_mask in [false, true] {
                    let mut mask = Array2::from_elem((3, 3), true);
                    mask[[0, 1]] = false;
                    if strided_mask {
                        mask = mask.reversed_axes();
                    }
                    let image = ImageData {
                        pixels: pixels.clone(),
                        mask: Some(mask),
                        nodata,
                        crs: Crs::epsg(4326),
                        bbox: Bbox::new(0., 0., 3., 3.),
                        transform: GeoTransform::from_gdal(&[0., 1., 0., 3., 0., -1.]),
                    };
                    let decode = |bytes: Bytes| {
                        image::load_from_memory(&bytes)
                            .unwrap()
                            .to_rgba8()
                            .into_raw()
                    };
                    assert_eq!(decode(plan.render(&image, nodata, TileFormat::Png).unwrap()),
                            decode(generic.render(&image, nodata, TileFormat::Png).unwrap()),
                            "range={range}, transparent={transparent}, opacity={opacity}, nodata={nodata:?}");
                }
            }
        }
    }
}

#[test]
fn shared_warp_matches_independent_f64_bands_without_joint_nodata() {
    let source = Array3::from_shape_fn((4, 8, 8), |(b, r, c)| {
        if b == 1 || (r == b && c == b) {
            0.
        } else if b == 2 && r == 5 && c == 5 {
            f64::NAN
        } else {
            (b * 30 + r * 8 + c + 1) as f64
        }
    });
    let plan = TileStyle::from_raster_query("version=2.0&type=rgb&bidx=3&bidx=1&bidx=3")
        .unwrap()
        .raster;
    for pixels in [
        PixelData::F64(source.clone()),
        PixelData::U8(source.mapv(|v| v as u8)),
        PixelData::U16(source.mapv(|v| v as u16)),
    ] {
        for nodata in [None, Some(0.)] {
            let mut mask = Array2::from_elem((8, 8), true);
            mask[[6, 6]] = false;
            let src = ImageData {
                pixels: pixels.clone(),
                mask: Some(mask),
                nodata,
                crs: Crs::epsg(4326),
                bbox: Bbox::new(-2., -2., 2., 2.),
                transform: GeoTransform::from_gdal(&[-2., 0.5, 0., 2., 0., -0.5]),
            };
            for (crs, bbox) in [
                (src.crs.clone(), src.bbox),
                (src.crs.clone(), Bbox::new(-3., -3., 3., 3.)),
                (
                    Crs::epsg(3857),
                    Bbox::new(-333958., -334111., 333958., 334111.),
                ),
            ] {
                for kernel in [
                    Resampling::Nearest,
                    Resampling::Bilinear,
                    Resampling::Cubic,
                    Resampling::Lanczos,
                    Resampling::Average,
                    Resampling::Mode,
                ] {
                    let actual = plan.warp_dependencies(&src, &crs, bbox, 9, kernel).unwrap();
                    let mut expected_mask = Array2::from_elem((9, 9), true);
                    for (i, &b) in plan.dependencies.iter().enumerate() {
                        let band = ImageData {
                            pixels: PixelData::F64(src.pixels.band_to_f64(b).insert_axis(Axis(0))),
                            ..src.clone()
                        };
                        let expected = crate::warp::warp(&band, &crs, bbox, 9, 9, kernel).unwrap();
                        ndarray::Zip::from(&mut expected_mask)
                            .and(expected.mask.as_ref().unwrap())
                            .for_each(|a, b| *a &= *b);
                        let actual_values = actual.pixels.band_to_f64(i);
                        let expected_values = expected.pixels.band_to_f64(0);
                        for ((r, c), &valid) in expected.mask.as_ref().unwrap().indexed_iter() {
                            if valid {
                                assert_eq!(
                                    actual_values[[r, c]],
                                    expected_values[[r, c]],
                                    "{kernel:?}, band={b}, nodata={nodata:?}"
                                );
                            }
                        }
                    }
                    assert_eq!(actual.mask.as_ref().unwrap(), &expected_mask);
                    assert_eq!(actual.nodata, None);
                }
            }
        }
    }
}

#[test]
fn statistics_apply_expressions_valid_tuples_and_sampling_before_stretch() {
    let src = ImageData {
        pixels: PixelData::F64(
            Array3::from_shape_vec((2, 1, 4), vec![1., 2., 0., 1., 0., 20., 999., 100.]).unwrap(),
        ),
        mask: Some(Array2::from_shape_vec((1, 4), vec![true, true, true, false]).unwrap()),
        nodata: None,
        crs: Crs::epsg(4326),
        bbox: Bbox::new(0., 0., 4., 1.),
        transform: GeoTransform::from_gdal(&[0., 1., 0., 1., 0., -1.]),
    };
    let style =
        TileStyle::from_raster_query("version=2.0&type=gray&expression=b2%2Fb1&method=minmax")
            .unwrap();
    let plan = style.raster.resolve_statistics(&src, false).unwrap();
    let rgba = image::load_from_memory(&plan.render(&src, None, TileFormat::Png).unwrap())
        .unwrap()
        .to_rgba8();
    assert_eq!(
        rgba.into_raw(),
        vec![0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 0, 0, 0, 0, 0, 0]
    );
    let mut plan = (*style.raster).clone();
    plan.statistics.sample_size = Some(1);
    let resolved = plan.resolve_statistics(&src, false).unwrap();
    assert_eq!(
        resolved.stretch.transfers().unwrap()[0].apply(10., false),
        Some(0.5)
    );
    let mut invalid = src.clone();
    invalid.mask.as_mut().unwrap().fill(false);
    assert!(matches!(
        plan.resolve_statistics(&invalid, false)
            .unwrap()
            .stretch
            .transfers()
            .unwrap()[0],
        Transfer::Empty
    ));
}

#[test]
fn warp_uses_only_dependencies_and_does_not_reapply_source_nodata() {
    let src = ImageData {
        pixels: PixelData::F64(
            Array3::from_shape_vec((2, 2, 2), vec![0., 2., 0., 2., 1., 1., 1., 1.]).unwrap(),
        ),
        mask: None,
        nodata: Some(1.),
        crs: Crs::epsg(3857),
        bbox: Bbox::new(0., 0., 2., 2.),
        transform: GeoTransform::from_gdal(&[0., 1., 0., 2., 0., -1.]),
    };
    let style =
        TileStyle::from_raster_query("version=2.0&type=gray&bidx=1&method=linear&rescale=0,2")
            .unwrap();
    let bytes = style
        .raster
        .render_warped(
            &src,
            &src.crs,
            src.bbox,
            1,
            Resampling::Bilinear,
            TileFormat::Png,
        )
        .unwrap();
    assert_eq!(
        image::load_from_memory(&bytes)
            .unwrap()
            .to_rgba8()
            .into_raw(),
        [128, 128, 128, 255]
    );
}

#[test]
fn negative_kernel_support_survives_raster_style_warp_and_render() {
    let src = ImageData {
        pixels: PixelData::F64(
            Array3::from_shape_vec((1, 1, 4), vec![10., 20., 30., 40.]).unwrap(),
        ),
        mask: Some(ndarray::array![[true, false, false, false]]),
        nodata: None,
        crs: Crs::epsg(3857),
        bbox: Bbox::new(0., 0., 4., 1.),
        transform: GeoTransform::from_gdal(&[0., 1., 0., 1., 0., -1.]),
    };
    for kernel in ["cubic", "lanczos"] {
        let style = TileStyle::from_raster_query(&format!(
            "version=2.0&type=gray&bidx=1&resampling={kernel}"
        ))
        .unwrap();
        let bytes = style
            .raster
            .render_warped(
                &src,
                &src.crs,
                Bbox::new(1.5, 0., 2.5, 1.),
                1,
                style.resampling(),
                TileFormat::Png,
            )
            .unwrap();
        assert_eq!(
            image::load_from_memory(&bytes)
                .unwrap()
                .to_rgba8()
                .into_raw(),
            [10, 10, 10, 255],
            "{kernel}"
        );
    }
}

#[cfg(all(test, feature = "mosaic"))]
mod mosaic_tests {
    use super::*;
    use crate::{
        bounds::{Bbox, GeoTransform},
        crs::Crs,
        mosaic::selection::Reducer,
    };
    use serde_json::json;

    fn render_sources(style: Value, sources: &[&[f64]]) -> Vec<u8> {
        let style = TileStyle::from_json(&style.to_string()).unwrap();
        let raster = style.raster;
        let config = raster.mosaic.as_ref().unwrap();
        let mut reducer: Option<Reducer<crate::mosaic::selection::NonEmptyReducer>> = None;
        for source in sources {
            let image = ImageData {
                pixels: PixelData::F64(
                    Array3::from_shape_vec((source.len(), 1, 1), source.to_vec()).unwrap(),
                ),
                mask: None,
                nodata: None,
                crs: Crs::epsg(3857),
                bbox: Bbox::new(0., 0., 1., 1.),
                transform: GeoTransform::from_gdal(&[0., 1., 0., 1., 0., -1.]),
            };
            let image = raster
                .warp_dependencies(&image, &image.crs, image.bbox, 1, Resampling::Nearest)
                .unwrap();
            let image = raster.prepare_mosaic(image);
            if let Some(r) = &mut reducer {
                r.feed(&image).unwrap();
            } else {
                reducer = Some(
                    Reducer::new_raster(config.selection(), config.rank)
                        .feed(&image)
                        .unwrap(),
                );
            }
        }
        let image = reducer.unwrap().finish().unwrap();
        let bytes = raster.render_mosaic(&image, TileFormat::Png).unwrap();
        image::load_from_memory(&bytes)
            .unwrap()
            .to_rgba8()
            .into_raw()
    }

    #[test]
    fn stage_changes_expression_order_and_invalid_first_fallback() {
        let mut style = json!({"version":"2.0","renderer":{"type":"gray","expression":"b2/b1"},"stretch":{"method":"linear","rescale":[[0,4]]},"mosaic":{"pixel_selection":"mean","stage":"before_channels"}});
        assert_eq!(
            render_sources(style.clone(), &[&[2., 6.], &[6., 6.]]),
            [96, 96, 96, 255]
        );
        style["mosaic"]["stage"] = json!("after_channels");
        assert_eq!(
            render_sources(style.clone(), &[&[2., 6.], &[6., 6.]]),
            [128, 128, 128, 255]
        );
        style["mosaic"]["pixel_selection"] = json!("first");
        assert_eq!(
            render_sources(style.clone(), &[&[0., 1.], &[1., 1.]]),
            [64, 64, 64, 255]
        );
        style["mosaic"]["stage"] = json!("before_channels");
        assert_eq!(render_sources(style, &[&[0., 1.], &[1., 1.]]), [0, 0, 0, 0]);
    }

    #[test]
    fn ranking_uses_stage_positions_and_preserves_whole_tuples() {
        let mut style = json!({"version":"2.0","renderer":{"type":"rgb","bidx":[3,1,3]},"stretch":{"method":"linear","rescale":[[0,100]]},"mosaic":{"pixel_selection":"highest","rank_channel":2,"stage":"before_channels"}});
        assert_eq!(
            render_sources(style.clone(), &[&[10., 0., 90.], &[80., 0., 20.]]),
            [230, 26, 230, 255]
        );
        style["mosaic"]["stage"] = json!("after_channels");
        assert_eq!(
            render_sources(style.clone(), &[&[10., 0., 90.], &[80., 0., 20.]]),
            [51, 204, 51, 255]
        );
        style["mosaic"]["pixel_selection"] = json!("lowest");
        assert_eq!(
            render_sources(style.clone(), &[&[10., 0., 90.], &[80., 0., 20.]]),
            [230, 26, 230, 255]
        );
        style["mosaic"]["pixel_selection"] = json!("highest");
        assert_eq!(
            render_sources(style, &[&[10., 0., 90.], &[10., 0., 20.]]),
            [230, 26, 230, 255]
        );
    }

    #[test]
    fn incomplete_tuples_never_mix_sources_and_opacity_runs_once() {
        let style = json!({"version":"2.0","renderer":{"type":"rgb","bidx":[1,2,1]},"opacity":0.5,"mosaic":{"pixel_selection":"first","stage":"before_channels"}});
        assert_eq!(
            render_sources(
                style.clone(),
                &[&[1., f64::NAN], &[f64::NAN, 2.], &[3., 4.]]
            ),
            [3, 4, 3, 128]
        );
        assert_eq!(
            render_sources(style, &[&[1., f64::NAN], &[f64::NAN, 2.]]),
            [0, 0, 0, 0]
        );
    }

    #[test]
    fn mean_median_avoid_overflow_and_preserve_fractional_results() {
        for method in ["mean", "median"] {
            let style = json!({"version":"2.0","renderer":{"type":"gray","bidx":[1]},"stretch":{"method":"linear","rescale":[[0.,1e308]]},"mosaic":{"pixel_selection":method,"stage":"before_channels"}});
            assert_eq!(
                render_sources(style.clone(), &[&[1e308], &[1e308]]),
                [255, 255, 255, 255]
            );
            assert_eq!(
                render_sources(style, &[&[-1e308], &[1e308]]),
                [0, 0, 0, 255]
            );
            let style = json!({"version":"2.0","renderer":{"type":"gray","bidx":[1]},"mosaic":{"pixel_selection":method,"stage":"after_channels"}});
            assert_eq!(render_sources(style, &[&[2.], &[3.]]), [3, 3, 3, 255]);
        }
    }

    #[test]
    fn expression_rank_bounds_are_checked_after_parsing() {
        for (stage, rank) in [("before_channels", 3), ("after_channels", 2)] {
            let style = json!({"version":"2.0","renderer":{"type":"gray","expression":"b1+b3"},"mosaic":{"pixel_selection":"highest","stage":stage,"rank_channel":rank}});
            assert!(TileStyle::from_json(&style.to_string())
                .unwrap_err()
                .to_string()
                .contains("rank_channel"));
        }
    }
}
