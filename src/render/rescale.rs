//! Per-band linear rescale with clamping.

use ndarray::Array3;

pub fn rescale(mut data: Array3<f64>, ranges: &[(f64, f64)]) -> Array3<f64> {
    let bands = data.shape()[0];
    for b in 0..bands {
        let (lo, hi) = ranges.get(b).copied().unwrap_or((0.0, 1.0));
        let span = (hi - lo).max(1e-12);
        let mut view = data.index_axis_mut(ndarray::Axis(0), b);
        view.mapv_inplace(|v| ((v - lo) / span).clamp(0.0, 1.0));
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_to_unit_range() {
        let data =
            Array3::<f64>::from_shape_vec((1, 1, 4), vec![-10.0, 0.0, 50.0, 1000.0]).unwrap();
        let out = rescale(data, &[(0.0, 100.0)]);
        assert_eq!(out[[0, 0, 0]], 0.0);
        assert_eq!(out[[0, 0, 1]], 0.0);
        assert_eq!(out[[0, 0, 2]], 0.5);
        assert_eq!(out[[0, 0, 3]], 1.0);
    }

    #[test]
    fn per_band_ranges() {
        let data =
            Array3::<f64>::from_shape_vec((2, 1, 2), vec![50.0, 100.0, 0.0, 1000.0]).unwrap();
        let out = rescale(data, &[(0.0, 100.0), (0.0, 2000.0)]);
        assert_eq!(out[[0, 0, 0]], 0.5);
        assert_eq!(out[[1, 0, 1]], 0.5);
    }
}
