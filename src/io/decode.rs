use std::sync::Arc;

use async_tiff::Tile;
use ndarray::Array2;
use tokio::sync::Semaphore;

use crate::error::{AgError, AgResult};
use crate::io::window::{stitch_mask_with_info, stitch_with_info, IfdInfo};
use crate::pixel::PixelData;

pub(crate) struct DecodeWindowJob {
    pub(crate) data_tiles: Vec<Tile>,
    pub(crate) mask_tiles: Option<Vec<Tile>>,
    pub(crate) coords: Vec<(usize, usize)>,
    pub(crate) data_info: IfdInfo,
    pub(crate) mask_info: Option<IfdInfo>,
    pub(crate) col_off: i64,
    pub(crate) row_off: i64,
    pub(crate) width: u64,
    pub(crate) height: u64,
}

pub(crate) async fn decode_window(
    job: DecodeWindowJob,
    semaphore: Arc<Semaphore>,
) -> AgResult<(PixelData, Option<Array2<bool>>)> {
    let permit = semaphore
        .acquire_owned()
        .await
        .map_err(|_| AgError::Tiff("decode semaphore closed".into()))?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        decode_window_sync(job)
    })
    .await
    .map_err(|e| AgError::Tiff(format!("join: {e}")))?
}

fn decode_window_sync(job: DecodeWindowJob) -> AgResult<(PixelData, Option<Array2<bool>>)> {
    let DecodeWindowJob {
        data_tiles,
        mask_tiles,
        coords,
        data_info,
        mask_info,
        col_off,
        row_off,
        width,
        height,
    } = job;

    let registry = async_tiff::decoder::DecoderRegistry::default();
    let data_arrays: Vec<async_tiff::Array> = data_tiles
        .into_iter()
        .map(|t| t.decode(&registry))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| AgError::Tiff(format!("decode: {e}")))?;
    let stitched = stitch_with_info(
        &data_arrays,
        &coords,
        &data_info,
        col_off,
        row_off,
        width,
        height,
    )?;
    let mask = match (mask_tiles, mask_info) {
        (Some(tiles), Some(info)) => {
            let mask_arrays: Vec<async_tiff::Array> = tiles
                .into_iter()
                .map(|t| t.decode(&registry))
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| AgError::Tiff(format!("decode mask: {e}")))?;
            Some(stitch_mask_with_info(
                &mask_arrays,
                &coords,
                &info,
                col_off,
                row_off,
                width,
                height,
            )?)
        }
        _ => None,
    };
    Ok((stitched, mask))
}
