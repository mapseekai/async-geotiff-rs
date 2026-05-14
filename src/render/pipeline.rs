//! Fused render pipeline: rescale → color_formula → composite → encode PNG.

use bytes::Bytes;
use ndarray::{Array2, Array3};

use crate::error::{AgError, AgResult};
use crate::pixel::ImageData;
use crate::render::color_formula::Op;
use crate::render::composite::{Composite, HandleGet};
use crate::render::encode::encode_png;
use crate::warp::resampler::Resampling;

/// Precompiled render plan for the hot path.
#[derive(Debug, Clone)]
pub struct CompiledRenderSpec {
    pub composite: Composite,
    pub rescale: Vec<(f64, f64)>,
    pub formula: Vec<Op>,
    pub bidx: Vec<usize>, // 0-indexed after compile
    pub nodata: Option<f64>,
    pub tile_size: u32,
    pub resampling: Resampling,
    pub auto_rgb: bool,
}

pub fn render(image: &ImageData, spec: &CompiledRenderSpec) -> AgResult<Bytes> {
    let bands = image.pixels.bands();
    let h = image.pixels.height();
    let w = image.pixels.width();

    let src = image.pixels.to_f64();
    let auto_rgb = spec.auto_rgb && bands >= 3;
    let bidx: Vec<usize> = if spec.bidx.is_empty() {
        match (auto_rgb, spec.composite.bands) {
            (true, _) => vec![0, 1, 2],
            (false, 1) => vec![0],
            (false, 3) if bands >= 3 => vec![0, 1, 2],
            (false, n) => {
                return Err(AgError::UnsupportedPixel(format!(
                    "expected at least {n} bands, got {bands}"
                )))
            }
        }
    } else {
        spec.bidx.clone()
    };
    for &band in &bidx {
        if band >= bands {
            return Err(AgError::query(
                "bidx",
                format!("band {} out of range for {} bands", band + 1, bands),
            ));
        }
    }

    let raw_bands: Vec<Array2<f64>> = bidx
        .iter()
        .map(|&b| src.index_axis(ndarray::Axis(0), b).to_owned())
        .collect();
    let bands_view: Vec<Array2<f64>> = bidx
        .iter()
        .map(|&b| src.index_axis(ndarray::Axis(0), b).to_owned())
        .collect();

    let mut rgba = Array3::<u8>::zeros((h, w, 4));
    let nodata = spec.nodata.or(image.nodata);

    if bands_view.len() == 1 {
        for r in 0..h {
            for c in 0..w {
                if invalid_pixel(image, &raw_bands, r, c, nodata) {
                    continue;
                }
                let v = bands_view[0][[r, c]];
                let px = spec.composite.get(&[v], None);
                rgba[[r, c, 0]] = px[0];
                rgba[[r, c, 1]] = px[1];
                rgba[[r, c, 2]] = px[2];
                rgba[[r, c, 3]] = px[3];
            }
        }
    } else if bands_view.len() == 3 {
        for r in 0..h {
            for c in 0..w {
                if invalid_pixel(image, &raw_bands, r, c, nodata) {
                    continue;
                }
                let rb = bands_view[0][[r, c]];
                let gb = bands_view[1][[r, c]];
                let bb = bands_view[2][[r, c]];
                let px = if !spec.formula.is_empty() {
                    let (rb, gb, bb) = normalize_rgb(spec, rb, gb, bb);
                    let (rb, gb, bb) =
                        crate::render::color_formula::apply(&spec.formula, rb, gb, bb);
                    [unit_to_u8(rb), unit_to_u8(gb), unit_to_u8(bb), 255]
                } else if auto_rgb {
                    pack_rgb(spec, rb, gb, bb)
                } else {
                    spec.composite.get(&[rb, gb, bb], None)
                };
                rgba[[r, c, 0]] = px[0];
                rgba[[r, c, 1]] = px[1];
                rgba[[r, c, 2]] = px[2];
                rgba[[r, c, 3]] = px[3];
            }
        }
    } else {
        return Err(AgError::UnsupportedPixel(format!(
            "expected 1 or 3 bands after bidx, got {}",
            bands_view.len()
        )));
    }

    let target = spec.tile_size as usize;
    if target > 0 && (h != target || w != target) {
        rgba = resize_rgba(&rgba, target, spec.resampling);
    }
    encode_png(&rgba)
}

fn normalize_rgb(spec: &CompiledRenderSpec, r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    let ranges = rgb_ranges(spec);
    (
        normalize(r, ranges[0]),
        normalize(g, ranges[1]),
        normalize(b, ranges[2]),
    )
}

fn pack_rgb(spec: &CompiledRenderSpec, r: f64, g: f64, b: f64) -> [u8; 4] {
    let (r, g, b) = normalize_rgb(spec, r, g, b);
    [unit_to_u8(r), unit_to_u8(g), unit_to_u8(b), 255]
}

fn rgb_ranges(spec: &CompiledRenderSpec) -> [(f64, f64); 3] {
    [
        spec.rescale.first().copied().unwrap_or((0.0, 255.0)),
        spec.rescale.get(1).copied().unwrap_or((0.0, 255.0)),
        spec.rescale.get(2).copied().unwrap_or((0.0, 255.0)),
    ]
}

fn normalize(value: f64, (lo, hi): (f64, f64)) -> f64 {
    let span = (hi - lo).max(1e-12);
    ((value - lo) / span).clamp(0.0, 1.0)
}

fn unit_to_u8(value: f64) -> u8 {
    (value.clamp(0.0, 1.0) * 255.0).round() as u8
}

fn invalid_pixel(
    image: &ImageData,
    raw_bands: &[Array2<f64>],
    row: usize,
    col: usize,
    nodata: Option<f64>,
) -> bool {
    if image.mask.as_ref().is_some_and(|m| !m[[row, col]]) {
        return true;
    }
    nodata.is_some_and(|nd| raw_bands.iter().any(|band| is_nodata(band[[row, col]], nd)))
}

fn is_nodata(value: f64, nodata: f64) -> bool {
    if nodata.is_nan() {
        value.is_nan()
    } else {
        (value - nodata).abs() < 1e-9
    }
}

fn resize_rgba(src: &Array3<u8>, size: usize, resampling: Resampling) -> Array3<u8> {
    let (h, w) = (src.shape()[0], src.shape()[1]);
    if h == size && w == size {
        return src.clone();
    }
    let mut dst = Array3::<u8>::zeros((size, size, 4));
    let scale_x = w as f64 / size as f64;
    let scale_y = h as f64 / size as f64;
    for r in 0..size {
        for c in 0..size {
            let x = (c as f64 + 0.5) * scale_x - 0.5;
            let y = (r as f64 + 0.5) * scale_y - 0.5;
            match resampling {
                Resampling::Nearest => copy_nearest(src, &mut dst, c, r, x, y),
                Resampling::Bilinear => copy_bilinear(src, &mut dst, c, r, x, y),
            }
        }
    }
    dst
}

fn copy_nearest(src: &Array3<u8>, dst: &mut Array3<u8>, c: usize, r: usize, x: f64, y: f64) {
    let max_x = src.shape()[1].saturating_sub(1) as i64;
    let max_y = src.shape()[0].saturating_sub(1) as i64;
    let sx = (x.round() as i64).clamp(0, max_x) as usize;
    let sy = (y.round() as i64).clamp(0, max_y) as usize;
    for ch in 0..4 {
        dst[[r, c, ch]] = src[[sy, sx, ch]];
    }
}

fn copy_bilinear(src: &Array3<u8>, dst: &mut Array3<u8>, c: usize, r: usize, x: f64, y: f64) {
    let max_x = src.shape()[1].saturating_sub(1) as i64;
    let max_y = src.shape()[0].saturating_sub(1) as i64;
    let x0 = (x.floor() as i64).clamp(0, max_x);
    let y0 = (y.floor() as i64).clamp(0, max_y);
    let x1 = (x0 + 1).clamp(0, max_x);
    let y1 = (y0 + 1).clamp(0, max_y);
    let dx = (x - x0 as f64).clamp(0.0, 1.0);
    let dy = (y - y0 as f64).clamp(0.0, 1.0);
    for ch in 0..4 {
        let v00 = src[[y0 as usize, x0 as usize, ch]] as f64;
        let v01 = src[[y0 as usize, x1 as usize, ch]] as f64;
        let v10 = src[[y1 as usize, x0 as usize, ch]] as f64;
        let v11 = src[[y1 as usize, x1 as usize, ch]] as f64;
        let top = v00 * (1.0 - dx) + v01 * dx;
        let bot = v10 * (1.0 - dx) + v11 * dx;
        dst[[r, c, ch]] = (top * (1.0 - dy) + bot * dy).round().clamp(0.0, 255.0) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bounds::{Bbox, GeoTransform};
    use crate::crs::Crs;
    use crate::pixel::PixelData;
    use crate::render::composite::{ColourDefinition, Composite};

    #[test]
    fn render_single_band_gradient_to_png() {
        let mut arr = Array3::<f64>::zeros((1, 16, 16));
        for r in 0..16 {
            for c in 0..16 {
                arr[[0, r, c]] = (c as f64) / 15.0;
            }
        }
        let img = ImageData {
            pixels: PixelData::F64(arr),
            mask: None,
            bbox: Bbox::new(0.0, 0.0, 16.0, 16.0),
            crs: Crs::epsg(3857),
            transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 16.0, 0.0, -1.0]),
            nodata: None,
        };
        let comp = Composite::from_definition(
            &ColourDefinition::Named("viridis".into()),
            Some(vec![0.0]),
            Some(vec![1.0]),
        )
        .unwrap();
        let spec = CompiledRenderSpec {
            composite: comp,
            rescale: vec![],
            formula: vec![],
            bidx: vec![],
            nodata: None,
            tile_size: 16,
            resampling: Resampling::Nearest,
            auto_rgb: false,
        };
        let bytes = render(&img, &spec).unwrap();
        assert!(bytes.starts_with(b"\x89PNG"));
    }

    #[test]
    fn render_resizes_to_tile_size() {
        let arr = Array3::<f64>::from_shape_vec((1, 2, 4), vec![0.0; 8]).unwrap();
        let img = ImageData {
            pixels: PixelData::F64(arr),
            mask: None,
            bbox: Bbox::new(0.0, 0.0, 4.0, 2.0),
            crs: Crs::epsg(3857),
            transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 2.0, 0.0, -1.0]),
            nodata: None,
        };
        let comp = Composite::from_definition(
            &ColourDefinition::Named("greys".into()),
            Some(vec![0.0]),
            Some(vec![1.0]),
        )
        .unwrap();
        let spec = CompiledRenderSpec {
            composite: comp,
            rescale: vec![],
            formula: vec![],
            bidx: vec![],
            nodata: None,
            tile_size: 8,
            resampling: Resampling::Nearest,
            auto_rgb: false,
        };
        let bytes = render(&img, &spec).unwrap();
        let decoded = image::load_from_memory(&bytes).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (8, 8));
    }

    #[test]
    fn render_uses_dataset_nodata_before_rescale() {
        let arr = Array3::<f64>::from_shape_vec((1, 1, 2), vec![-9999.0, 100.0]).unwrap();
        let img = ImageData {
            pixels: PixelData::F64(arr),
            mask: None,
            bbox: Bbox::new(0.0, 0.0, 2.0, 1.0),
            crs: Crs::epsg(3857),
            transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 1.0, 0.0, -1.0]),
            nodata: Some(-9999.0),
        };
        let comp = Composite::from_definition(
            &ColourDefinition::Named("greys".into()),
            Some(vec![0.0]),
            Some(vec![1.0]),
        )
        .unwrap();
        let spec = CompiledRenderSpec {
            composite: comp,
            rescale: vec![(0.0, 100.0)],
            formula: vec![],
            bidx: vec![],
            nodata: None,
            tile_size: 2,
            resampling: Resampling::Nearest,
            auto_rgb: false,
        };
        let bytes = render(&img, &spec).unwrap();
        let rgba = image::load_from_memory(&bytes).unwrap().to_rgba8();
        assert_eq!(rgba.get_pixel(0, 0).0[3], 0);
        assert_eq!(rgba.get_pixel(1, 0).0[3], 255);
    }

    #[test]
    fn render_rejects_out_of_range_bidx() {
        let arr = Array3::<f64>::zeros((1, 1, 1));
        let img = ImageData {
            pixels: PixelData::F64(arr),
            mask: None,
            bbox: Bbox::new(0.0, 0.0, 1.0, 1.0),
            crs: Crs::epsg(3857),
            transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 1.0, 0.0, -1.0]),
            nodata: None,
        };
        let comp = Composite::from_definition(
            &ColourDefinition::Named("greys".into()),
            Some(vec![0.0]),
            Some(vec![1.0]),
        )
        .unwrap();
        let spec = CompiledRenderSpec {
            composite: comp,
            rescale: vec![],
            formula: vec![],
            bidx: vec![1],
            nodata: None,
            tile_size: 1,
            resampling: Resampling::Nearest,
            auto_rgb: false,
        };
        assert!(render(&img, &spec).is_err());
    }

    #[test]
    fn rgb_rescale_is_applied_once() {
        let arr = Array3::<f64>::from_shape_vec((3, 1, 1), vec![128.0, 64.0, 32.0]).unwrap();
        let img = ImageData {
            pixels: PixelData::F64(arr),
            mask: None,
            bbox: Bbox::new(0.0, 0.0, 1.0, 1.0),
            crs: Crs::epsg(3857),
            transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 1.0, 0.0, -1.0]),
            nodata: None,
        };
        let comp = Composite::from_definition(
            &ColourDefinition::Rgb([0.0, 0.0, 0.0], [255.0, 255.0, 255.0]),
            None,
            None,
        )
        .unwrap();
        let spec = CompiledRenderSpec {
            composite: comp,
            rescale: vec![(0.0, 255.0), (0.0, 255.0), (0.0, 255.0)],
            formula: vec![],
            bidx: vec![],
            nodata: None,
            tile_size: 1,
            resampling: Resampling::Nearest,
            auto_rgb: false,
        };

        let bytes = render(&img, &spec).unwrap();
        let rgba = image::load_from_memory(&bytes).unwrap().to_rgba8();
        assert_eq!(rgba.get_pixel(0, 0).0, [128, 64, 32, 255]);
    }

    #[test]
    fn auto_rgb_uses_first_three_bands_with_byte_range() {
        let arr = Array3::<f64>::from_shape_vec((3, 1, 1), vec![120.0, 80.0, 40.0]).unwrap();
        let img = ImageData {
            pixels: PixelData::F64(arr),
            mask: None,
            bbox: Bbox::new(0.0, 0.0, 1.0, 1.0),
            crs: Crs::epsg(3857),
            transform: GeoTransform::from_gdal(&[0.0, 1.0, 0.0, 1.0, 0.0, -1.0]),
            nodata: None,
        };
        let comp = Composite::from_definition(
            &ColourDefinition::Named("greys".into()),
            Some(vec![0.0]),
            Some(vec![1.0]),
        )
        .unwrap();
        let spec = CompiledRenderSpec {
            composite: comp,
            rescale: vec![],
            formula: vec![],
            bidx: vec![],
            nodata: None,
            tile_size: 1,
            resampling: Resampling::Nearest,
            auto_rgb: true,
        };

        let bytes = render(&img, &spec).unwrap();
        let rgba = image::load_from_memory(&bytes).unwrap().to_rgba8();
        assert_eq!(rgba.get_pixel(0, 0).0, [120, 80, 40, 255]);
    }
}
