mod cache;
pub mod cog;
mod decode;
pub mod stats;
pub mod window;

pub use cache::CacheConfig;
pub use cog::{CogReader, ReaderConfig};

// Re-exported so crate consumers can name the reader type and wire their own
// object_store WITHOUT taking a direct, version-locked dependency on async-tiff
// or object_store. `object_store::ClientOptions` is already in our public API
// (see `CogReader::open_http_with_options`), so this only formalises an existing
// requirement.
pub use async_tiff::reader::ObjectReader;
pub use object_store;
