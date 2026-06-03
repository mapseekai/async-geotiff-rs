use std::sync::OnceLock;

use async_tiff::Array;
use async_tiff::Tile;

use crate::error::{AgError, AgResult};

fn registry() -> &'static async_tiff::decoder::DecoderRegistry {
    static DECODERS: OnceLock<async_tiff::decoder::DecoderRegistry> = OnceLock::new();
    DECODERS.get_or_init(async_tiff::decoder::DecoderRegistry::default)
}

/// Decode one fetched native tile to its pixel `Array`. CPU-bound; callers run
/// it inside `spawn_blocking` under the decode semaphore.
pub(crate) fn decode_one(tile: Tile) -> AgResult<Array> {
    tile.decode(registry())
        .map_err(|e| AgError::Tiff(format!("decode: {e}")))
}
