#[cfg(feature = "perf-tracing")]
use std::time::Duration;

#[cfg(feature = "perf-tracing")]
#[macro_export]
macro_rules! perf_trace {
    ($($tt:tt)*) => {
        $($tt)*
    };
}

#[cfg(not(feature = "perf-tracing"))]
#[macro_export]
macro_rules! perf_trace {
    ($($tt:tt)*) => {};
}

#[cfg(feature = "perf-tracing")]
#[inline]
pub(crate) fn duration_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}
