//! Fused render pipeline: rescale → color_formula → composite → encode PNG.

use bytes::Bytes;
use ndarray::{Array2, Array3};

use crate::error::{AgError, AgResult};
use crate::pixel::ImageData;
use crate::render::color_formula::Op;
use crate::render::composite::{Composite, HandleGet};
use crate::render::encode::encode_png;

/// Precompiled render plan for the hot path.
#[derive(Debug, Clone)]
pub struct CompiledRenderSpec {
    pub composite: Composite,
    pub rescale: Vec<(f64, f64)>,
    pub formula: Vec<Op>,
    pub bidx: Vec<usize>, // 0-indexed after compile
    pub nodata: Option<f64>,
    pub tile_size: u32,
}

pub fn render(image: &ImageData, spec: &CompiledRenderSpec) -> AgResult<Bytes> {
    let bands = image.pixels.bands();
    let h = image.pixels.height();
    let w = image.pixels.width();

    let src = image.pixels.to_f64();
    let bidx: Vec<usize> = if spec.bidx.is_empty() {
        (0..bands).collect()
    } else {
        spec.bidx.clone()
    };

    let mut bands_view: Vec<Array2<f64>> = bidx
        .iter()
        .map(|&b| src.index_axis(ndarray::Axis(0), b).to_owned())
        .collect();

    if !spec.rescale.is_empty() {
        for (i, band) in bands_view.iter_mut().enumerate() {
            let (lo, hi) = spec.rescale.get(i).copied().unwrap_or((0.0, 1.0));
            let span = (hi - lo).max(1e-12);
            band.mapv_inplace(|v| ((v - lo) / span).clamp(0.0, 1.0));
        }
    }

    let mut rgba = Array3::<u8>::zeros((h, w, 4));
    let ndv: Option<Vec<f64>> = spec.nodata.map(|n| vec![n; bidx.len()]);
    let ndv_ref = ndv.as_deref();

    if bands_view.len() == 1 {
        for r in 0..h {
            for c in 0..w {
                let v = bands_view[0][[r, c]];
                let px = spec.composite.get(&[v], ndv_ref);
                rgba[[r, c, 0]] = px[0];
                rgba[[r, c, 1]] = px[1];
                rgba[[r, c, 2]] = px[2];
                rgba[[r, c, 3]] = px[3];
            }
        }
    } else if bands_view.len() == 3 {
        for r in 0..h {
            for c in 0..w {
                let mut rb = bands_view[0][[r, c]];
                let mut gb = bands_view[1][[r, c]];
                let mut bb = bands_view[2][[r, c]];
                if !spec.formula.is_empty() {
                    let (nr, ng, nb) =
                        crate::render::color_formula::apply(&spec.formula, rb, gb, bb);
                    rb = nr;
                    gb = ng;
                    bb = nb;
                }
                let px = spec.composite.get(&[rb, gb, bb], ndv_ref);
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

    encode_png(&rgba)
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
        };
        let bytes = render(&img, &spec).unwrap();
        assert!(bytes.starts_with(b"\x89PNG"));
    }
}
