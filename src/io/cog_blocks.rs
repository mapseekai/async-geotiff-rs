//! Bounded foreground block fetch/decode and best-effort ring prefetch.

use super::*;

/// Block coordinates of the `ring`-wide frame around the inclusive tile range
/// `(tx0..=tx1, ty0..=ty1)`, clamped to the IFD's tile grid and excluding the
/// inner (already-read) rectangle.
pub(super) fn ring_coords(
    info: &IfdInfo<ValidatedIfdInfo>,
    (tx0, tx1): (u32, u32),
    (ty0, ty1): (u32, u32),
    ring: i64,
) -> Vec<(usize, usize)> {
    if info.tile_width == 0 || info.tile_height == 0 {
        return Vec::new();
    }
    let max_tx = i64::from((info.image_width.saturating_sub(1)) / info.tile_width);
    let max_ty = i64::from((info.image_height.saturating_sub(1)) / info.tile_height);
    let (itx0, itx1) = (i64::from(tx0), i64::from(tx1));
    let (ity0, ity1) = (i64::from(ty0), i64::from(ty1));
    let x0 = (itx0 - ring).max(0);
    let x1 = (itx1 + ring).min(max_tx);
    let y0 = (ity0 - ring).max(0);
    let y1 = (ity1 + ring).min(max_ty);
    let mut out = Vec::new();
    for x in x0..=x1 {
        for y in y0..=y1 {
            let inside = (itx0..=itx1).contains(&x) && (ity0..=ity1).contains(&y);
            if !inside {
                out.push((x as usize, y as usize));
            }
        }
    }
    out
}

/// Drop the transport error entirely for HTTP readers: its Display and source
/// can contain the complete signed URL. Local/store errors retain diagnostics.
pub(super) fn transport_error(error: &async_tiff::error::AsyncTiffError, redact: bool) -> AgError {
    if let Some(resource) = crate::io::snapshot::resource_error(error) {
        return resource;
    }
    if redact {
        AgError::Tiff("HTTP range request failed".into())
    } else {
        AgError::Tiff(format!("fetch_tiles: {error}"))
    }
}

/// Validate declared compressed ranges before async-tiff indexes their arrays
/// or constructs unchecked offset+length ranges, and before any store fetch.
pub(super) fn validate_compressed_ranges(
    ifd: &ImageFileDirectory,
    coords: &[(usize, usize)],
    max_bytes: u64,
) -> AgResult<u64> {
    let invalid = || AgError::Tiff("invalid or oversized compressed block ranges".into());
    let (cols, rows) = ifd.tile_count().ok_or_else(invalid)?;
    let offsets = ifd.tile_offsets().ok_or_else(invalid)?;
    let counts = ifd.tile_byte_counts().ok_or_else(invalid)?;
    let planar = ifd.planar_configuration() == async_tiff::tags::PlanarConfiguration::Planar;
    let bands = if planar {
        usize::from(ifd.samples_per_pixel())
    } else {
        1
    };
    let tiles_per_band = cols.checked_mul(rows).ok_or_else(invalid)?;
    let mut total = 0u64;
    for &(x, y) in coords {
        if x >= cols || y >= rows {
            return Err(invalid());
        }
        let tile = y
            .checked_mul(cols)
            .and_then(|n| n.checked_add(x))
            .ok_or_else(invalid)?;
        for band in 0..bands {
            let index = band
                .checked_mul(tiles_per_band)
                .and_then(|n| n.checked_add(tile))
                .ok_or_else(invalid)?;
            let offset = *offsets.get(index).ok_or_else(invalid)?;
            let count = *counts.get(index).ok_or_else(invalid)?;
            offset.checked_add(count).ok_or_else(invalid)?;
            total = total.checked_add(count).ok_or_else(invalid)?;
            if total > max_bytes {
                return Err(invalid());
            }
        }
    }
    Ok(total)
}

/// Conservative compressed, decoded and decoder-copy allowance for a batch.
pub(super) fn block_work_bytes(
    ifd: &ImageFileDirectory,
    coords: &[(usize, usize)],
    limit: u64,
) -> AgResult<u64> {
    let compressed = validate_compressed_ranges(ifd, coords, limit)?;
    let sample_bytes =
        u64::from(ifd.bits_per_sample().iter().copied().max().unwrap_or(8)).div_ceil(8);
    let mut width = u64::from(ifd.tile_width().unwrap_or(ifd.image_width()));
    let mut height = u64::from(ifd.tile_height().unwrap_or(ifd.image_height()));
    if ifd.compression() == async_tiff::tags::Compression::ModernJPEG {
        // JPEG coefficient planes are padded to MCUs (up to 32x32), which
        // matters particularly for thin tiles. Header dimensions are checked
        // by the bounded decoder before coefficient allocation.
        width = width.div_ceil(32) * 32;
        height = height.div_ceil(32) * 32;
    }
    let decoded = (coords.len() as u64)
        .checked_mul(width)
        .and_then(|n| n.checked_mul(height))
        .and_then(|n| n.checked_mul(u64::from(ifd.samples_per_pixel())))
        .and_then(|n| n.checked_mul(sample_bytes));
    let workspace = if ifd.compression() == async_tiff::tags::Compression::ZSTD {
        256 * 1024 // single-pass context; no advertised-window allocation
    } else {
        64 * 1024
    };
    decoded
        .and_then(|n| n.checked_mul(8))
        .and_then(|n| n.checked_add((coords.len() as u64).saturating_mul(workspace)))
        .and_then(|n| n.checked_add(compressed.saturating_mul(2)))
        .ok_or_else(|| AgError::ResourceLimit("block work estimate overflow".into()))
}

/// Best-effort fetch + decode of `coords` into the shared block cache.
///
/// Already-cached blocks are skipped via a non-counting [`BlockCache::peek`]
/// (so prefetch probes don't skew hit/miss stats), and blocks another caller is
/// already fetching are skipped via [`BlockCache::claim`] dedup. One coalesced
/// `fetch_tiles` covers all claimed blocks; decodes run one at a time under the
/// shared CPU limiter so speculation never floods the foreground decode budget.
/// All errors are logged at debug and swallowed — a failed prefetch must never
/// surface anywhere; dropped claim guards let foreground waiters retry fresh.
#[allow(clippy::too_many_arguments)]
pub(super) async fn prefetch_blocks(
    ifd: &ImageFileDirectory,
    kind: BlockKind,
    level: usize,
    coords: &[(usize, usize)],
    source: &Arc<str>,
    cache: &Arc<BlockCache>,
    cpu: &CpuLimiter,
    reader: &ObjectReader,
    redact_errors: bool,
    max_compressed_bytes: u64,
    work_budget: &crate::resource::WorkBudget,
    metadata_guard: &Arc<Vec<crate::resource::WorkReservation>>,
) {
    if let Err(error) = validate_compressed_ranges(ifd, coords, max_compressed_bytes) {
        tracing::debug!(%error, "block prefetch ranges rejected");
        return;
    }
    let Ok(estimate) = block_work_bytes(ifd, coords, max_compressed_bytes) else {
        return;
    };
    let Ok(work) = work_budget.try_reserve(estimate) else {
        return;
    };
    let mut owned: Vec<(BlockKey, InflightGuard)> = Vec::new();
    let mut fetch_coords: Vec<(usize, usize)> = Vec::new();
    for &(tx, ty) in coords {
        let key = BlockKey {
            source: Arc::clone(source),
            level: level as u32,
            kind,
            tx: tx as u32,
            ty: ty as u32,
        };
        if cache.peek(&key) {
            continue;
        }
        match cache.claim(&key).await {
            Claim::Owned(guard) => {
                owned.push((key, guard));
                fetch_coords.push((tx, ty));
            }
            // A foreground request (or another prefetch) is on it already.
            Claim::Pending(_) | Claim::Ready(_) => {}
        }
    }
    if owned.is_empty() {
        return;
    }
    cache.record_fetched(fetch_coords.len() as u64);
    let tiles = match ifd.fetch_tiles(&fetch_coords, reader).await {
        Ok(t) => t,
        Err(e) => {
            // Guards drop unresolved → any waiters observe a cancel and retry.
            tracing::debug!(error = %transport_error(&e, redact_errors), kind = ?kind, "block prefetch fetch failed");
            return;
        }
    };
    let decode_dimensions = [
        ifd.tile_width().unwrap_or(ifd.image_width()),
        ifd.tile_height().unwrap_or(ifd.image_height()),
    ];
    for ((key, guard), tile) in owned.into_iter().zip(tiles.into_iter()) {
        let cpu = cpu.clone();
        let work = work.clone();
        let metadata_guard = metadata_guard.clone();
        let res: AgResult<Arc<DecodedBlock>> = cache
            .try_get_with(key, async move {
                let permit = cpu.acquire().await?;
                let arr = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    let _work = work;
                    let _metadata_guard = metadata_guard;
                    crate::io::decode::decode_one(tile, decode_dimensions)
                })
                .await
                .map_err(|e| AgError::Tiff(format!("join: {e}")))??;
                Ok(DecodedBlock::new(arr))
            })
            .await;
        match res {
            Ok(block) => guard.resolve(Ok(block)),
            Err(e) => {
                // Same error-forwarding semantics as the foreground path: a
                // decode error is deterministic for this block, so publishing
                // it to waiters matches what their own decode would produce.
                tracing::debug!(error = %e, kind = ?kind, "block prefetch decode failed");
                guard.resolve(Err(Arc::new(e)));
            }
        }
    }
}

impl CogReader {
    /// Resolve every `(tx, ty)` in `coords` to a decoded block. Probes the
    /// shared cache first, issues ONE coalesced `fetch_tiles` for the misses,
    /// then decodes each miss exactly once via the cache's single-flight.
    pub(super) async fn blocks_for(
        &self,
        level: usize,
        kind: BlockKind,
        ifd: &ImageFileDirectory,
        coords: &[(usize, usize)],
        work: &crate::resource::WorkReservation,
    ) -> AgResult<Vec<Arc<DecodedBlock>>> {
        validate_compressed_ranges(ifd, coords, self.max_window_bytes)?;
        #[cfg(feature = "perf-tracing")]
        let started = Instant::now();
        let mut keys = Vec::with_capacity(coords.len());
        for &(tx, ty) in coords {
            keys.push(BlockKey {
                source: Arc::clone(&self.source_id),
                level: level as u32,
                kind,
                tx: tx as u32,
                ty: ty as u32,
            });
        }
        let mut resolved: Vec<Option<Arc<DecodedBlock>>> = Vec::with_capacity(keys.len());
        let mut miss_idx: Vec<usize> = Vec::with_capacity(keys.len());
        let mut miss_coords: Vec<(usize, usize)> = Vec::with_capacity(keys.len());
        for (i, k) in keys.iter().enumerate() {
            match self.block_cache.get(k).await {
                Some(b) => resolved.push(Some(b)),
                None => {
                    resolved.push(None);
                    miss_idx.push(i);
                    miss_coords.push(coords[i]);
                }
            }
        }
        #[cfg(feature = "perf-tracing")]
        let cache_probe_done = started.elapsed();
        #[cfg(feature = "perf-tracing")]
        let cache_hits = keys.len().saturating_sub(miss_idx.len());
        #[cfg(feature = "perf-tracing")]
        let mut owned_count = 0usize;
        #[cfg(feature = "perf-tracing")]
        let mut pending_count = 0usize;
        #[cfg(feature = "perf-tracing")]
        let mut fetch_ms = Duration::ZERO;
        #[cfg(feature = "perf-tracing")]
        let mut decode_ms = Duration::ZERO;
        #[cfg(feature = "perf-tracing")]
        let mut pending_wait_ms = Duration::ZERO;

        // Retry loop: claim → fetch+resolve owned → await pending.
        // Pending waiters that observe Ok(None) (owner aborted) re-enter the
        // next pass with a fresh claim instead of propagating a 500 to the caller.
        // Each pass preserves the deadlock-freedom invariant: all owned guards are
        // resolved (JoinSet drained) before any pending awaits begin.
        const MAX_CANCEL_RETRIES: u8 = 3;
        let mut retry: Vec<(usize, (usize, usize))> = miss_idx
            .iter()
            .zip(miss_coords.iter())
            .map(|(&mi, &coord)| (mi, coord))
            .collect();
        let mut cancel_attempts: u8 = 0;

        while !retry.is_empty() {
            let mut to_fetch: Vec<(usize, (usize, usize), InflightGuard)> = Vec::new();
            let mut pending: Vec<(usize, (usize, usize), PendingBlock)> = Vec::new();
            for &(mi, coord) in &retry {
                match self.block_cache.claim(&keys[mi]).await {
                    Claim::Ready(block) => resolved[mi] = Some(block),
                    Claim::Owned(guard) => to_fetch.push((mi, coord, guard)),
                    Claim::Pending(rx) => pending.push((mi, coord, rx)),
                }
            }
            crate::perf_trace! {
                owned_count += to_fetch.len();
                pending_count += pending.len();
            }

            if !to_fetch.is_empty() {
                // One coalesced fetch for only the blocks we own (object_store
                // merges adjacent ranges). On error, the guards drop → waiters
                // observe Ok(None) and retry.
                let fetch_coords: Vec<(usize, usize)> =
                    to_fetch.iter().map(|(_, c, _)| *c).collect();
                self.block_cache.record_fetched(fetch_coords.len() as u64);
                #[cfg(feature = "perf-tracing")]
                let fetch_started = Instant::now();
                let tiles = ifd
                    .fetch_tiles(&fetch_coords, &self.reader)
                    .await
                    .map_err(|e| transport_error(&e, self.redact_transport_errors))?;
                crate::perf_trace! {
                    fetch_ms += fetch_started.elapsed();
                }
                // Decode owned misses CONCURRENTLY (bounded by the CPU limiter).
                // `try_get_with` single-flights each block's decode + insert;
                // the guard then publishes the result to any waiters.
                let mut set = tokio::task::JoinSet::new();
                for ((mi, _coord, guard), tile) in to_fetch.into_iter().zip(tiles.into_iter()) {
                    let key = keys[mi].clone();
                    #[cfg(feature = "perf-tracing")]
                    let log_level = key.level;
                    #[cfg(feature = "perf-tracing")]
                    let log_kind = key.kind;
                    #[cfg(feature = "perf-tracing")]
                    let log_tx = key.tx;
                    #[cfg(feature = "perf-tracing")]
                    let log_ty = key.ty;
                    let decode_dimensions = [
                        ifd.tile_width().unwrap_or(ifd.image_width()),
                        ifd.tile_height().unwrap_or(ifd.image_height()),
                    ];
                    let sem = self.cpu.clone();
                    let work = work.clone();
                    let metadata_guard = self.metadata_reservations.clone();
                    let cache = Arc::clone(&self.block_cache);
                    // Capture before JoinSet::spawn; its child task does not
                    // inherit the request's task-local scope.
                    let request = crate::request::RequestContext::current();
                    set.spawn(async move {
                        let owner_request = request.clone();
                        let block_res: AgResult<Arc<DecodedBlock>> = cache
                            .try_get_with(key, async move {
                                #[cfg(feature = "perf-tracing")]
                                let decode_started = Instant::now();
                                #[cfg(feature = "perf-tracing")]
                                let wait_started = Instant::now();
                                let permit = sem.acquire().await?;
                                #[cfg(feature = "perf-tracing")]
                                let cpu_wait_ms = wait_started.elapsed();
                                #[cfg(feature = "perf-tracing")]
                                let job_started = Instant::now();
                                let arr = tokio::task::spawn_blocking(move || {
                                    let _permit = permit;
                                    let _work = work;
                                    let _metadata_guard = metadata_guard;
                                    crate::request::RequestContext::run_blocking(request, || {
                                        crate::io::decode::decode_one(tile, decode_dimensions)
                                    })
                                })
                                .await
                                .map_err(|e| AgError::Tiff(format!("join: {e}")))??;
                                #[cfg(feature = "perf-tracing")]
                                let decode_job_ms = job_started.elapsed();
                                crate::perf_trace! {
                                    tracing::debug!(
                                        level = log_level,
                                        kind = ?log_kind,
                                        tx = log_tx,
                                        ty = log_ty,
                                        cpu_wait_ms = duration_ms(cpu_wait_ms),
                                        decode_job_ms = duration_ms(decode_job_ms),
                                        total_ms = duration_ms(decode_started.elapsed()),
                                        "decode_block timing"
                                    );
                                }
                                Ok(DecodedBlock::new(arr))
                            })
                            .await;
                        // An expired owner must relinquish its claim, not
                        // publish its cancellation as a shared decode failure.
                        // Dropping the guard lets healthy waiters claim/retry.
                        if let Some(owner) = &owner_request {
                            owner.check()?;
                        }
                        // Publish to concurrent waiters (Arc-wrap the error since
                        // AgError isn't Clone), then return to this caller.
                        let forward: BlockResult = match &block_res {
                            Ok(b) => Ok(Arc::clone(b)),
                            Err(e) => Err(Arc::new(AgError::Tiff(format!("block fetch: {e}")))),
                        };
                        guard.resolve(forward);
                        block_res.map(|b| (mi, b))
                    });
                }
                #[cfg(feature = "perf-tracing")]
                let decode_started = Instant::now();
                while let Some(res) = set.join_next().await {
                    let (mi, block) =
                        res.map_err(|e| AgError::Tiff(format!("decode task: {e}")))??;
                    resolved[mi] = Some(block);
                }
                crate::perf_trace! {
                    decode_ms += decode_started.elapsed();
                }
            }

            // INVARIANT (deadlock-freedom): every owned guard above is resolved
            // (inside the drained JoinSet) BEFORE we await any pending block.
            // This breaks cross-request wait cycles — e.g. request A owns {1,2}
            // and waits on {3} while request B owns {3} and waits on {1,2}: both
            // publish their owned results before blocking on the other's, so
            // neither stalls. Do NOT move pending awaits before the JoinSet drain.
            #[cfg(feature = "perf-tracing")]
            let pending_started = Instant::now();
            let mut next_retry: Vec<(usize, (usize, usize))> = Vec::new();
            for (mi, coord, rx) in pending {
                match BlockCache::await_pending(rx).await {
                    Ok(Some(block)) => {
                        resolved[mi] = Some(block);
                    }
                    Ok(None) => {
                        // Owner aborted (disconnect/cancellation): re-claim next pass.
                        next_retry.push((mi, coord));
                    }
                    Err(e) => return Err(e),
                }
            }
            crate::perf_trace! {
                pending_wait_ms += pending_started.elapsed();
            }

            if !next_retry.is_empty() {
                cancel_attempts += 1;
                if cancel_attempts >= MAX_CANCEL_RETRIES {
                    return Err(AgError::Tiff(
                        "in-flight block fetch canceled after retries".into(),
                    ));
                }
            }
            retry = next_retry;
        }
        crate::perf_trace! {
            tracing::debug!(
                level,
                kind = ?kind,
                requested = keys.len(),
                cache_hits,
                cache_misses = miss_idx.len(),
                owned = owned_count,
                pending = pending_count,
                cache_probe_ms = duration_ms(cache_probe_done),
                fetch_ms = duration_ms(fetch_ms),
                decode_ms = duration_ms(decode_ms),
                pending_wait_ms = duration_ms(pending_wait_ms),
                total_ms = duration_ms(started.elapsed()),
                "blocks_for timing"
            );
        }
        resolved
            .into_iter()
            .map(|b| b.ok_or_else(|| AgError::Tiff("block unresolved after fetch".into())))
            .collect()
    }

    /// Spawn (at most [`MAX_PREFETCH_TASKS`] concurrently) a detached task that
    /// fetches + decodes the block ring around the window just read. Best-effort
    /// by design: it never blocks or fails the foreground request, dedups with
    /// in-flight foreground fetches via [`BlockCache::claim`], decodes one block
    /// at a time (gentle on the shared CPU budget), and skips entirely when the
    /// task budget is exhausted.
    pub(super) fn maybe_spawn_prefetch(
        &self,
        level: usize,
        tx_range: (u32, u32),
        ty_range: (u32, u32),
    ) {
        use std::sync::atomic::AtomicUsize;
        if self.prefetch_ring == 0 {
            return;
        }
        // Speculation never queues ahead of a waiting foreground window.
        let Ok(read_permit) = Arc::clone(&self.reads).try_acquire_owned() else {
            return;
        };
        let ring_width = u64::from(self.prefetch_ring) * 2;
        let cols = u64::from(tx_range.1 - tx_range.0 + 1) + ring_width;
        let rows = u64::from(ty_range.1 - ty_range.0 + 1) + ring_width;
        if cols.saturating_mul(rows) > self.max_window_blocks as u64 {
            return;
        }
        if self
            .prefetch_inflight
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < MAX_PREFETCH_TASKS).then_some(n + 1)
            })
            .is_err()
        {
            return;
        }
        /// Decrements the in-flight counter however the task ends.
        struct Slot(Arc<AtomicUsize>);
        impl Drop for Slot {
            fn drop(&mut self) {
                self.0.fetch_sub(1, Ordering::Relaxed);
            }
        }
        let slot = Slot(Arc::clone(&self.prefetch_inflight));

        let ring = i64::from(self.prefetch_ring);
        let mut targets = Vec::with_capacity(2);
        match self.data_ifd_index(level) {
            Ok(idx) => targets.push((BlockKind::Data, idx)),
            Err(_) => return, // slot decrements via drop
        }
        if let Some(Some(idx)) = self.mask_ifds.get(level) {
            targets.push((BlockKind::Mask, *idx));
        }
        let tiff = Arc::clone(&self.tiff);
        let reader = self.reader.clone();
        let cache = Arc::clone(&self.block_cache);
        let cpu = self.cpu.clone();
        let source = Arc::clone(&self.source_id);
        let redact_errors = self.redact_transport_errors;
        let max_compressed_bytes = self.max_window_bytes;
        let work_budget = self.work_budget.clone();
        let metadata_guard = self.metadata_reservations.clone();
        let mut tasks = self.prefetch_tasks.lock().unwrap();
        while tasks.try_join_next().is_some() {}
        tasks.spawn(async move {
            let _read_permit = read_permit;
            let _slot = slot;
            for (kind, idx) in targets {
                let Some(ifd) = tiff.ifds().get(idx) else {
                    continue;
                };
                let Ok(info) = IfdInfo::from(ifd).validate() else {
                    continue;
                };
                let coords = ring_coords(&info, tx_range, ty_range, ring);
                if coords.is_empty() {
                    continue;
                }
                prefetch_blocks(
                    ifd,
                    kind,
                    level,
                    &coords,
                    &source,
                    &cache,
                    &cpu,
                    &reader,
                    redact_errors,
                    max_compressed_bytes,
                    &work_budget,
                    &metadata_guard,
                )
                .await;
            }
        });
    }
}
