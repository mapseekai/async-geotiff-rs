//! A read-only, conditional view of one object, including metadata reads.
use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures_util::{
    stream::{self, BoxStream},
    StreamExt,
};
use object_store::{path::Path, *};
use std::ops::Range;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};

/// Consistency contract for source bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SnapshotPolicy {
    /// Require a strong ETag or an immutable version ID; fail on mutation.
    #[default]
    RequireValidator,
    /// Caller guarantees the source never changes. Needed for opaque readers
    /// or stores without a validator; this deliberately waives verification.
    AssumeImmutable,
}

pub(crate) struct SnapshotStore {
    inner: Arc<dyn ObjectStore>,
    budget: crate::resource::WorkBudget,
    meta: ObjectMeta,
    max_range_bytes: u64,
    max_metadata_bytes: u64,
    metadata_bytes: AtomicU64,
    reading_metadata: AtomicBool,
}
impl std::fmt::Debug for SnapshotStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SnapshotStore")
    }
}
impl std::fmt::Display for SnapshotStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SnapshotStore")
    }
}
fn invalid(message: &'static str) -> Error {
    Error::Generic {
        store: "snapshot",
        source: message.into(),
    }
}
/// Preserve request-control errors through object_store/async-tiff wrappers.
pub(crate) fn resource_error(
    error: &(dyn std::error::Error + 'static),
) -> Option<crate::error::AgError> {
    use crate::error::AgError;

    let mut current = Some(error);
    while let Some(error) = current {
        match error.downcast_ref::<AgError>() {
            Some(AgError::ResourceLimit(message)) => {
                return Some(AgError::ResourceLimit(message.clone()));
            }
            Some(AgError::Overloaded) => return Some(AgError::Overloaded),
            Some(AgError::DeadlineExceeded) => return Some(AgError::DeadlineExceeded),
            Some(AgError::Cancelled) => return Some(AgError::Cancelled),
            _ => {}
        }
        current = error.source();
    }
    None
}
pub(crate) fn into_ag_error(error: Error) -> crate::error::AgError {
    resource_error(&error)
        .unwrap_or_else(|| crate::error::AgError::Tiff(format!("snapshot: {error}")))
}

fn unsupported() -> Error {
    invalid("snapshot store is read-only")
}

impl SnapshotStore {
    pub(crate) async fn open(
        inner: Arc<dyn ObjectStore>,
        path: Path,
        policy: SnapshotPolicy,
        max_range_bytes: u64,
        max_metadata_bytes: u64,
        budget: crate::resource::WorkBudget,
    ) -> Result<Arc<Self>> {
        if max_range_bytes == 0 || max_metadata_bytes == 0 {
            return Err(invalid("snapshot byte limits must be positive"));
        }
        // A presigned GET URL may reject HEAD, so discover its validator using
        // one bounded byte of the same request method used for all later reads.
        let reservation = budget.try_reserve(1).map_err(|error| Error::Generic {
            store: "snapshot",
            source: Box::new(error),
        })?;
        let first = inner
            .get_opts(
                &path,
                GetOptions {
                    range: Some(GetRange::Bounded(0..1)),
                    ..Default::default()
                },
            )
            .await?;
        let meta = &first.meta;
        let strong = meta
            .e_tag
            .as_deref()
            .is_some_and(|e| !e.trim().is_empty() && !e.starts_with("W/") && e != "*");
        if policy == SnapshotPolicy::RequireValidator
            && !strong
            && meta.version.as_deref().is_none_or(str::is_empty)
        {
            return Err(invalid("source lacks a strong ETag/version; explicitly choose AssumeImmutable only for immutable sources"));
        }
        if first.range != (0..1) || meta.size == 0 {
            return Err(invalid("snapshot probe returned an invalid range"));
        }
        let meta = collect_bounded(first, 1, reservation).await?.meta;
        Ok(Arc::new(Self {
            inner,
            budget,
            meta,
            max_range_bytes,
            max_metadata_bytes,
            metadata_bytes: AtomicU64::new(1),
            reading_metadata: AtomicBool::new(true),
        }))
    }
    pub(crate) fn size(&self) -> u64 {
        self.meta.size
    }
    pub(crate) fn finish_metadata(&self) {
        self.reading_metadata.store(false, Ordering::Release);
    }
    pub(crate) fn cache_identity(&self, source: &str) -> String {
        serde_json::to_string(&(source, &self.meta.e_tag, &self.meta.version, self.meta.size))
            .expect("strings are serializable")
    }
    async fn read(&self, location: &Path, mut options: GetOptions) -> Result<GetResult> {
        if location != &self.meta.location {
            return Err(invalid("snapshot path mismatch"));
        }
        let requested = match options.range.take() {
            Some(GetRange::Bounded(r)) if r.start < r.end => r,
            _ => return Err(invalid("snapshot reads must use a nonempty bounded range")),
        };
        let requested = requested.start..requested.end.min(self.meta.size);
        let range_limit = if self.reading_metadata.load(Ordering::Acquire) {
            self.max_metadata_bytes
        } else {
            self.max_range_bytes
        };
        let len = requested
            .end
            .checked_sub(requested.start)
            .filter(|&n| n > 0 && n <= range_limit)
            .ok_or_else(|| invalid("source range exceeds snapshot byte limit or extent"))?;
        if self.reading_metadata.load(Ordering::Acquire) {
            self.metadata_bytes
                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                    n.checked_add(len).filter(|&n| n <= self.max_metadata_bytes)
                })
                .map_err(|_| invalid("metadata fetch budget exceeded"))?;
        }
        options.range = Some(GetRange::Bounded(requested.clone()));
        options.if_match = self.meta.e_tag.clone().filter(|e| !e.starts_with("W/"));
        options.version = self.meta.version.clone();
        let reservation = self
            .budget
            .try_reserve(len)
            .map_err(|error| Error::Generic {
                store: "snapshot",
                source: Box::new(error),
            })?;
        let result = self.inner.get_opts(location, options).await?;
        // Check the response as well, catching servers that ignore If-Match.
        if result.meta.size != self.meta.size
            || result.meta.e_tag != self.meta.e_tag
            || result.meta.version != self.meta.version
        {
            return Err(invalid("source changed since metadata was opened"));
        }
        if result.range != requested {
            return Err(invalid("source returned a mismatched byte range"));
        }
        collect_bounded(result, len, reservation).await
    }
}
async fn collect_bounded(
    mut result: GetResult,
    limit: u64,
    reservation: crate::resource::WorkReservation,
) -> Result<GetResult> {
    let capacity =
        usize::try_from(limit).map_err(|_| invalid("range too large for this platform"))?;
    let mut out = BytesMut::with_capacity(capacity);
    // into_stream consumes the response, but only uses its payload and range.
    // Move heap-backed metadata out first; keep file range handling upstream.
    let meta = ObjectMeta {
        location: std::mem::take(&mut result.meta.location),
        e_tag: result.meta.e_tag.take(),
        version: result.meta.version.take(),
        ..result.meta
    };
    let attributes = std::mem::take(&mut result.attributes);
    let extensions = std::mem::take(&mut result.extensions);
    let range = result.range.clone();
    let mut chunks = result.into_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = chunk?;
        if chunk.len() > capacity - out.len() {
            return Err(invalid("response body exceeds requested range"));
        }
        out.extend_from_slice(&chunk);
    }
    if out.len() != capacity {
        return Err(invalid("response body is shorter than requested range"));
    }
    struct AccountedBytes {
        bytes: Bytes,
        _reservation: crate::resource::WorkReservation,
    }
    impl AsRef<[u8]> for AccountedBytes {
        fn as_ref(&self) -> &[u8] {
            &self.bytes
        }
    }
    // Every Bytes clone/slice keeps the backing reservation, including cached
    // metadata and compressed data retained by a cancelled blocking decoder.
    let bytes = Bytes::from_owner(AccountedBytes {
        bytes: out.freeze(),
        _reservation: reservation,
    });
    Ok(GetResult {
        meta,
        attributes,
        extensions,
        range,
        payload: GetResultPayload::Stream(stream::once(async move { Ok(bytes) }).boxed()),
    })
}
#[async_trait]
impl ObjectStore for SnapshotStore {
    async fn get_opts(&self, p: &Path, o: GetOptions) -> Result<GetResult> {
        self.read(p, o).await
    }
    async fn get_ranges(&self, p: &Path, ranges: &[Range<u64>]) -> Result<Vec<Bytes>> {
        let total = ranges
            .iter()
            .try_fold(0u64, |sum, r| {
                r.end.checked_sub(r.start).and_then(|n| sum.checked_add(n))
            })
            .ok_or_else(|| invalid("range batch overflow"))?;
        if total > self.max_range_bytes {
            return Err(invalid("range batch exceeds snapshot byte limit"));
        }
        // Preserve range coalescing, while routing every fetch through the
        // conditional and bounded get_opts implementation above.
        coalesce_ranges(
            ranges,
            |range| self.get_range(p, range),
            OBJECT_STORE_COALESCE_DEFAULT,
        )
        .await
    }
    async fn put_opts(&self, _: &Path, _: PutPayload, _: PutOptions) -> Result<PutResult> {
        Err(unsupported())
    }
    async fn put_multipart_opts(
        &self,
        _: &Path,
        _: PutMultipartOptions,
    ) -> Result<Box<dyn MultipartUpload>> {
        Err(unsupported())
    }
    fn delete_stream(
        &self,
        _: BoxStream<'static, Result<Path>>,
    ) -> BoxStream<'static, Result<Path>> {
        stream::once(async { Err(unsupported()) }).boxed()
    }
    fn list(&self, _: Option<&Path>) -> BoxStream<'static, Result<ObjectMeta>> {
        stream::once(async { Err(unsupported()) }).boxed()
    }
    async fn list_with_delimiter(&self, _: Option<&Path>) -> Result<ListResult> {
        Err(unsupported())
    }
    async fn copy_opts(&self, _: &Path, _: &Path, _: CopyOptions) -> Result<()> {
        Err(unsupported())
    }
}

/// Inspect declared counts before async-tiff allocates tag maps/value vectors.
pub(crate) async fn preflight_ifd(
    fetch: &impl async_tiff::metadata::MetadataFetch,
    offset: u64,
    bigtiff: bool,
    endian: async_tiff::reader::Endianness,
    config: &crate::io::ReaderConfig,
    remaining: u64,
) -> crate::error::AgResult<u64> {
    use crate::error::AgError;
    let malformed = || AgError::Tiff("invalid or oversized TIFF directory".into());
    let integer = |bytes: &[u8]| -> u64 {
        match endian {
            async_tiff::reader::Endianness::LittleEndian => {
                bytes.iter().rev().fold(0, |n, &b| (n << 8) | u64::from(b))
            }
            async_tiff::reader::Endianness::BigEndian => {
                bytes.iter().fold(0, |n, &b| (n << 8) | u64::from(b))
            }
        }
    };
    let header_size = if bigtiff { 8 } else { 2 };
    let stride = if bigtiff { 20 } else { 12 };
    let inline = if bigtiff { 8 } else { 4 };
    let header_end = offset.checked_add(header_size).ok_or_else(malformed)?;
    let size = config.snapshot_size.unwrap_or(u64::MAX);
    if header_end > size {
        return Err(malformed());
    }
    let header = fetch
        .fetch(offset..header_end)
        .await
        .map_err(|e| resource_error(&e).unwrap_or_else(malformed))?;
    if header.len() != header_size as usize {
        return Err(malformed());
    }
    let entries = integer(&header);
    if entries > config.max_ifd_entries {
        return Err(malformed());
    }
    let table_size = entries
        .checked_mul(stride)
        .and_then(|n| n.checked_add(inline))
        .ok_or_else(malformed)?;
    let table_end = header_end.checked_add(table_size).ok_or_else(malformed)?;
    if table_end > size || table_size > config.max_metadata_bytes {
        return Err(malformed());
    }
    let table = fetch
        .fetch(header_end..table_end)
        .await
        .map_err(|e| resource_error(&e).unwrap_or_else(malformed))?;
    if table.len() != table_size as usize {
        return Err(malformed());
    }
    let mut allocation = entries
        .checked_mul(
            (std::mem::size_of::<(async_tiff::tags::Tag, async_tiff::TagValue)>() * 2) as u64,
        )
        .ok_or_else(malformed)?;
    for entry in table[..(entries * stride) as usize].chunks_exact(stride as usize) {
        let kind = integer(&entry[2..4]);
        let element_bytes = match kind {
            1 | 2 | 6 | 7 => 1,
            3 | 8 => 2,
            4 | 9 | 11 | 13 => 4,
            5 | 10 | 12 | 16 | 17 | 18 => 8,
            _ => return Err(malformed()),
        };
        let count = integer(&entry[4..(4 + inline) as usize]);
        let bytes = count.checked_mul(element_bytes).ok_or_else(malformed)?;
        if bytes > inline {
            let pointer = integer(&entry[(4 + inline) as usize..]);
            if pointer.checked_add(bytes).is_none_or(|end| end > size) {
                return Err(malformed());
            }
        }
        // Raw vector, TagValue list, and converted IFD arrays can coexist.
        let values = count
            .checked_mul((std::mem::size_of::<async_tiff::TagValue>() * 3) as u64)
            .and_then(|n| n.checked_add(bytes))
            .ok_or_else(malformed)?;
        allocation = allocation.checked_add(values).ok_or_else(malformed)?;
        if allocation > remaining {
            return Err(malformed());
        }
    }
    Ok(allocation)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_control_errors_survive_object_store_wrappers() {
        use crate::error::AgError;

        for error in [
            AgError::ResourceLimit("budget exhausted".into()),
            AgError::Overloaded,
            AgError::DeadlineExceeded,
            AgError::Cancelled,
        ] {
            let kind = std::mem::discriminant(&error);
            let message = error.to_string();
            let wrapped = Error::Generic {
                store: "test",
                source: Box::new(error),
            };
            let recovered = resource_error(&wrapped).expect("request classification was lost");
            assert_eq!(std::mem::discriminant(&recovered), kind);
            assert_eq!(recovered.to_string(), message);
            let recovered = into_ag_error(wrapped);
            assert_eq!(std::mem::discriminant(&recovered), kind);
            assert_eq!(recovered.to_string(), message);
        }
    }

    #[tokio::test]
    async fn bounded_response_moves_extensions_and_preserves_file_range() {
        #[derive(Debug)]
        struct NeverClone;
        impl Clone for NeverClone {
            fn clone(&self) -> Self {
                panic!("response extensions must be moved, not cloned");
            }
        }
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), b"0123456789").unwrap();
        let path = Path::from_filesystem_path(file.path()).unwrap();
        let store = object_store::local::LocalFileSystem::new();
        let mut response = store
            .get_opts(
                &path,
                GetOptions {
                    range: Some(GetRange::Bounded(3..7)),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(matches!(response.payload, GetResultPayload::File(..)));
        response.extensions.insert(NeverClone);
        response
            .attributes
            .insert(object_store::Attribute::ContentType, "image/tiff".into());
        let budget = crate::resource::WorkBudget::new(4);
        let response = collect_bounded(response, 4, budget.try_reserve(4).unwrap())
            .await
            .unwrap();
        assert_eq!(response.meta.location, path);
        assert_eq!(response.meta.size, 10);
        assert_eq!(response.range, 3..7);
        assert!(response.extensions.get::<NeverClone>().is_some());
        assert_eq!(
            response
                .attributes
                .get(&object_store::Attribute::ContentType)
                .unwrap()
                .as_ref(),
            "image/tiff"
        );
        let bytes = response.bytes().await.unwrap();
        assert_eq!(bytes.as_ref(), b"3456");
        assert_eq!(budget.used_bytes(), 4);
        drop(bytes);
        assert_eq!(budget.used_bytes(), 0);
    }

    #[tokio::test]
    async fn snapshot_buffers_hold_budget_until_last_bytes_clone_drops() {
        let memory = Arc::new(object_store::memory::InMemory::new());
        let path = Path::from("x");
        memory
            .put(&path, Bytes::from_static(b"abcd").into())
            .await
            .unwrap();
        let budget = crate::resource::WorkBudget::new(16);
        let snapshot = SnapshotStore::open(
            memory,
            path.clone(),
            SnapshotPolicy::RequireValidator,
            16,
            16,
            budget.clone(),
        )
        .await
        .unwrap();
        snapshot.finish_metadata();
        let bytes = snapshot.get_range(&path, 0..4).await.unwrap();
        let clone = bytes.clone();
        assert_eq!(budget.used_bytes(), 4);
        drop(bytes);
        assert_eq!(budget.used_bytes(), 4);
        drop(clone);
        assert_eq!(budget.used_bytes(), 0);
    }
    #[tokio::test]
    async fn snapshot_metadata_budget_and_response_length_are_enforced() {
        let memory = Arc::new(object_store::memory::InMemory::new());
        let path = Path::from("x");
        memory
            .put(&path, Bytes::from_static(b"abcd").into())
            .await
            .unwrap();
        let budget = crate::resource::WorkBudget::new(16);
        let snapshot = SnapshotStore::open(
            memory.clone(),
            path.clone(),
            SnapshotPolicy::RequireValidator,
            16,
            2,
            budget.clone(),
        )
        .await
        .unwrap();
        assert!(snapshot.get_range(&path, 0..2).await.is_err()); // probe already spent one byte
        let mut result = memory
            .get_opts(
                &path,
                GetOptions {
                    range: Some(GetRange::Bounded(0..1)),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        result.payload = GetResultPayload::Stream(
            stream::once(async { Ok(Bytes::from_static(b"too long")) }).boxed(),
        );
        assert!(collect_bounded(result, 1, budget.try_reserve(1).unwrap())
            .await
            .is_err());
        assert_eq!(budget.used_bytes(), 0);
    }

    mod http {
        use super::*;
        #[tokio::test]
        async fn snapshot_requires_strong_validator_and_checks_ignored_conditionals() {
            use axum::{
                http::{HeaderMap, StatusCode},
                routing::get,
                Router,
            };
            use std::sync::atomic::AtomicUsize;
            let mode = Arc::new(AtomicUsize::new(0));
            let state = mode.clone();
            let app = Router::new().route(
                "/bytes",
                get(move |headers: HeaderMap| {
                    let state = state.clone();
                    async move {
                        let mode = state.load(Ordering::SeqCst);
                        let mut out = HeaderMap::new();
                        out.insert("content-range", "bytes 0-0/4".parse().unwrap());
                        match mode {
                            1 => {
                                out.insert("etag", "W/\"v1\"".parse().unwrap());
                            }
                            2 => {
                                out.insert("etag", "\"v1\"".parse().unwrap());
                            }
                            3 => {
                                assert_eq!(headers["if-match"], "\"v1\"");
                                out.insert("etag", "\"v2\"".parse().unwrap());
                            }
                            _ => {}
                        }
                        (StatusCode::PARTIAL_CONTENT, out, "a")
                    }
                }),
            );
            let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = socket.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(socket, app).await.unwrap() });
            let store = Arc::new(
                object_store::http::HttpBuilder::new()
                    .with_url(format!("http://{addr}/bytes"))
                    .with_client_options(ClientOptions::new().with_allow_http(true))
                    .build()
                    .unwrap(),
            );
            let path = Path::from("");
            let budget = crate::resource::WorkBudget::new(100);
            assert!(SnapshotStore::open(
                store.clone(),
                path.clone(),
                SnapshotPolicy::RequireValidator,
                16,
                16,
                budget.clone()
            )
            .await
            .is_err());
            assert!(SnapshotStore::open(
                store.clone(),
                path.clone(),
                SnapshotPolicy::AssumeImmutable,
                16,
                16,
                budget.clone()
            )
            .await
            .is_ok());
            mode.store(1, Ordering::SeqCst);
            assert!(SnapshotStore::open(
                store.clone(),
                path.clone(),
                SnapshotPolicy::RequireValidator,
                16,
                16,
                budget.clone()
            )
            .await
            .is_err());
            mode.store(2, Ordering::SeqCst);
            let snapshot = SnapshotStore::open(
                store,
                path.clone(),
                SnapshotPolicy::RequireValidator,
                16,
                16,
                budget,
            )
            .await
            .unwrap();
            snapshot.finish_metadata();
            mode.store(3, Ordering::SeqCst);
            assert!(
                snapshot.get_range(&path, 0..1).await.is_err(),
                "response validator mismatch must fail even when the server ignores If-Match"
            );
            server.abort();
        }
    }
}
