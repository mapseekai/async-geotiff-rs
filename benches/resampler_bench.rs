//! Measures the resampler hot loop with the contiguous-slice fast path
//! (`as_slice()` => flat indexing) versus the strided ndarray fallback.
//!
//! Both arms call the *same* `sample_masked`. The only difference is the
//! input view's memory layout: a band sliced from a C-order array is
//! standard-contiguous (fast path engages); a transposed view is not, so
//! the function falls back to `view[[r,c]]` strided indexing — exactly the
//! code path that existed before this optimization.

use async_geotiff::warp::Resampling;
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use ndarray::Array2;

const SRC: usize = 512;
const DST: usize = 256;

fn sweep(view: ndarray::ArrayView2<f64>, mask: Option<ndarray::ArrayView2<bool>>) -> f64 {
    let kernel = Resampling::Bilinear;
    let scale = SRC as f64 / DST as f64;
    let mut acc = 0.0;
    for r in 0..DST {
        for c in 0..DST {
            // Fractional source coords with a sub-pixel offset, the same
            // shape of access the scalar cross-CRS warp generates.
            let sc = (c as f64 + 0.37) * scale;
            let sr = (r as f64 + 0.62) * scale;
            acc += kernel.sample_masked(view, mask, sc, sr, 0.0);
        }
    }
    acc
}

fn bench(c: &mut Criterion) {
    let base = Array2::<f64>::from_shape_fn((SRC, SRC), |(y, x)| (y * SRC + x) as f64);
    let mask = Array2::<bool>::from_elem((SRC, SRC), true);

    let mut g = c.benchmark_group("resampler_sample_masked");

    // Contiguous (standard layout) -> as_slice() fast path.
    g.bench_function("contiguous_fastpath", |b| {
        b.iter(|| black_box(sweep(base.view(), None)))
    });
    g.bench_function("contiguous_fastpath_masked", |b| {
        b.iter(|| black_box(sweep(base.view(), Some(mask.view()))))
    });

    // Transposed view -> as_slice() is None -> strided fallback (old path).
    let t = base.t();
    let tm = mask.t();
    g.bench_function("strided_fallback", |b| {
        b.iter(|| black_box(sweep(t, None)))
    });
    g.bench_function("strided_fallback_masked", |b| {
        b.iter(|| black_box(sweep(t, Some(tm))))
    });

    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
