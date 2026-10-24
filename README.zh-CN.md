# async-geotiff

[English](https://github.com/mapseekai/async-geotiff-rs/blob/main/README.md) | 中文 | [文档站](https://async-geotiff-rs.pages.dev/zh/) | [API 文档](https://docs.rs/async-geotiff)

一个纯 Rust 异步库，用于读取云端托管的 Cloud-Optimized GeoTIFF（COG），并以 XYZ 地图瓦片形式提供数据，支持 Raster Style 3.0 输入。

**不依赖 GDAL。** 栅格 IO 使用 [`async-tiff`](https://docs.rs/async-tiff) 与 [`object_store`](https://docs.rs/object_store)；重投影使用可通过 feature 控制、依赖系统 `libproj` 的 [`proj`](https://docs.rs/proj)；仿射计算使用 [`raffine`](https://docs.rs/raffine)；色带使用 [`colorous`](https://docs.rs/colorous)；重采样使用 [`fast_image_resize`](https://docs.rs/fast_image_resize)；PNG 编码使用 [`image`](https://docs.rs/image)。

## 快速开始

在应用的 `Cargo.toml` 中添加：

```toml
[dependencies]
async-geotiff = "0.13.0"
```

默认启用 mosaic 和依赖系统 `libproj` 的通用重投影。如不需要原生 PROJ，可使用
`async-geotiff = { version = "0.13.0", default-features = false, features = ["mosaic"] }`。
完整应用示例见[作为库使用](https://async-geotiff-rs.pages.dev/zh/docs/get-started/library-usage/)。

以下命令从 Git 仓库运行示例服务。

读取远程 COG：

```bash
cargo run --release --example serve -- https://example.com/cog.tif
```

不传 URL 时，服务会浏览本地 `data/*.tif` 文件，并提供数据集选择器：

```bash
cargo run --release --example serve
```

然后打开以下任一页面：

- `http://127.0.0.1:8080/maplibre`
- `http://127.0.0.1:8080/openlayers`
- `http://127.0.0.1:8080/leaflet`

默认监听 `127.0.0.1:8080`。如需暴露到其他网络接口：

```bash
ASYNC_GEOTIFF_BIND=0.0.0.0:8080 cargo run --release --example serve
```

请求瓦片：

```bash
curl 'http://127.0.0.1:8080/tiles/WebMercatorQuad/0/0/0.png?version=3.0&type=pseudocolor&bidx=1&method=linear&rescale=0,3000&colormap_name=viridis'
```

## Raster Style 3.0 输入

采用 `raster-style = "=0.4.0"`，Rust API 支持 `TileStyle::from_json`、
`from_raster_query` 和 `from_raster_style`。三个查看器默认生成 Q3。
新格式必须声明 `version=3.0&type=...`，省略拉伸使用固定 `[0,255]`。
HTTP 只接受完整 Q3；不支持的规范能力明确报错。
详见[迁移指南与能力范围](https://async-geotiff-rs.pages.dev/zh/docs/guides/styling/)。

## 波段计算

`renderer.expression` 使用从 1 开始的 `bN` 波段引用，支持括号、加减乘除和一元负号。
gray/pseudocolor 使用一个表达式，RGB 使用三个以分号分隔的表达式。
表达式与 `bidx` 互斥；引用波段的 NoData 和非有限计算结果输出透明像元。

```bash
curl 'http://127.0.0.1:8080/tiles/WebMercatorQuad/0/0/0.png?version=3.0&type=gray&expression=(b5-b4)%2F(b5%2Bb4)&method=linear&rescale=-1,1'
```

## 作为库使用

HTTP 服务只位于 `examples/serve.rs`，库本身不强制依赖 axum。下游项目通常直接使用 `CogReader`、`Tiler` 或 `MosaicTiler` 读取 COG 并生成瓦片。

读取多个数据源时，建议复用同一个 `Arc<BlockCache>`，并为每个 reader 设置不同的 `source_id`，使所有数据源共享同一个解码块缓存预算。

## 可选 STAC Demo

启用已有的 `stac` feature 后，可以通过环境变量将 `/mosaic/tiles/...` 连接到固定的 STAC API、集合和 COG 资产字段：

```bash
ASYNC_GEOTIFF_STAC_URL=https://earth-search.aws.element84.com/v1 \
ASYNC_GEOTIFF_STAC_COLLECTION=sentinel-2-l2a \
ASYNC_GEOTIFF_STAC_ASSET_KEY=visual \
cargo run --example serve --features stac
```

三个变量必须同时提供，且与 `ASYNC_GEOTIFF_MOSAIC` 互斥。它们只在进程启动时读取，HTTP 请求参数不能选择 STAC 地址、集合或资产字段。STAC 资产仍使用默认 HTTP 访问策略，并拒绝环回和 link-local 地址，避免 Demo 成为客户端可驱动的公网代理。

STAC 按服务端顺序跟随同源 GET/POST 分页；限制单响应 1 MiB、累计响应 8 MiB、
128 页及 8 MiB 资产元数据，超限返回错误，不输出不完整结果。资产数量使用
`MosaicConfig::max_assets_per_tile`；设置为 0 只关闭数量上限。

自定义认证、显式代理或 timeout 使用
`StacSource::with_http_client_builder(api_url, builder, collections, asset_key)?`，
传入尚未 `build()` 的 **reqwest 0.11** `ClientBuilder`。它保留配置，并替换重定向
策略为最多十次同源跳转；跨源跳转在发送凭据前拒绝。旧 `with_http_client`
入口返回错误，`with_client(stac_client::Client, ...)` 已弃用并在 discovery 时失败。
迁移时将认证放入 builder 的默认 headers。

## 样式与迁移

样式统一使用 Raster Style 3.0 JSON/Q3：

```rust
let style = async_geotiff::style::TileStyle::from_json(r#"{
  "version": "3.0",
  "renderer": {"type": "rgb", "bidx": [1, 2, 3]},
  "stretch": {"method": "linear", "rescale": [[0, 3000]]}
}"#)?;
let bytes = tiler.tile(coord, &style).await?;
```

Mosaic 在同一个文档中声明 Mosaic 1.0.0 Schema，并增加 `raster:mosaic` 配置，调用 `mosaic.tile(coord, &style)`。
支持五种选择方法、两个合并阶段和排序通道。资产列表由 MosaicJSON/STAC 提供。

这是破坏性迁移：移除 `from_query`、`from_pairs`、公开可变样式字段、旧 `Stretch`/
`ColourDefinition` / `Colour` / `lut_from_stops` API，以及 mosaic 的独立 selection 参数。默认样式为灰度波段 1，
`method=none` 固定映射 0–255；高位深数据应显式配置线性范围。
PNG 和无损 WebP 可用于样式输出，后者需要 `webp` feature。
未知字段、旧参数和暂未实现的能力均报错。

完整字段、迁移映射和能力边界见[样式指南](https://async-geotiff-rs.pages.dev/zh/docs/guides/styling/)；
请求路径和编码见[瓦片 API](https://async-geotiff-rs.pages.dev/zh/docs/reference/tile-api/)。

## 支持的 TileMatrixSet

- `WebMercatorQuad`（EPSG:3857）：常见 slippy XYZ 客户端的默认选择。
- `WorldCRS84Quad`（EPSG:4326）。

## TMS 支持的像素类型

源 COG 支持 U8、U16、I16、U32、I32、F32 和 F64；在读取与重投影边界处统一为 F64。

## 构建要求

- Rust ≥ 1.92，与 `Cargo.toml` 中的 `rust-version` 一致。
- 默认 `proj` feature 需要系统安装 `libproj` 9.x。
- macOS：`brew install proj`。
- Debian/Ubuntu：`apt install libproj-dev libsqlite3-dev libtiff-dev clang`。
- 如不需要重投影，可使用 `cargo build --no-default-features`。

## 架构与性能

处理流程为：HTTP 请求 → TMS 瓦片范围 → `CogReader` 读取 → 可选重投影 → 拉伸、着色和合成 → PNG/无损 WebP 编码。

```text
HTTP 请求（axum，`examples/serve.rs`）
  ├─ TMS.tile_bbox(z, x, y)            瓦片网格
  ├─ CogReader.read_window              async-tiff + object_store
  │    └─ 共享 BlockCache 中的原生解码块；未命中时合并获取，
  │       并通过受信号量限制的 spawn_blocking 解码
  ├─ warp::warp（src_crs ≠ dst_crs）    proj 粗网格 + 双线性重投影
  │    └─ 在受信号量限制的 spawn_blocking 中执行（Proj 不实现 Send/Sync）
  ├─ 渲染管线：                         ndarray + raster-style
  │    ├─ 每波段拉伸
  │    ├─ Gamma / opacity
  │    └─ Raster Style 灰度 / RGB / 原生色表
  └─ PNG / lossless WebP 编码             image（+ image-webp）
```

超出数据范围的瓦片返回 `Ok(None)`（示例服务响应 HTTP 204）；无损 WebP 输出需要启用 `webp` Cargo feature。

模块布局：

```text
src/
  error.rs            AgError + AgResult
  bounds.rs           Bbox + GeoTransform（封装 raffine::Affine）
  crs.rs              Crs 标识
  pixel.rs            PixelData + ImageData
  tms.rs              WebMercatorQuad + WorldCRS84Quad
  io/
    cog.rs            CogReader 打开、元数据和块读取编排
    block_cache.rs    共享解码块缓存（BlockKey/BlockCache，单一预算）
    cache.rs          CacheConfig（块缓存字节预算和可选 TTL）
    decode.rs         受信号量限制的单块 spawn_blocking 解码辅助函数
    window.rs         源瓦片范围和拼接辅助函数
  warp/               Projector + Resampling + 粗网格重投影
  render/             colormap、expression、encode、pipeline
  style/              Raster Style JSON/Q3 校验与渲染计划
  tiler.rs            编排器
examples/
  serve.rs            Demo 入口和 viewer wiring
  serve/server.rs     Demo 服务的 axum 路由
  maplibre.html       MapLibre viewer
  openlayers.html     OpenLayers viewer
  leaflet.html        Leaflet viewer
```

库本身不暴露 axum 服务模块；HTTP 服务位于示例中，因此下游库用户不会被迫引入 Web 框架依赖。

CPU 密集型解码、拼接、重投影和渲染通过受信号量限制的 `spawn_blocking` 执行。可以使用 `CogReader::builder()`（例如 `.max_decode_tasks(n)`、`.block_cache(cache)`，再调用 `.open_http(url)` / `.open_object_store(store, key)` / `.open(reader)`）调整解码并发；使用 `Tiler::with_config` 与 `async_geotiff::tiler::TilerConfig` 调整重投影和渲染并发。默认使用主机可用并行度，并将 0 限制为至少一个 permit。

解码后的原生瓦片块使用共享 `Arc<BlockCache>` 缓存，默认预算为 **512 MB**。`ReaderConfig` 携带调用方提供的 `source_id`（缓存键前缀）和 `block_cache`；多个 reader 应共享同一个 `block_cache`，但使用不同的 `source_id`，从而把所有数据集的解码内存限制在一个预算内。`MosaicTiler` 会通过 `ReaderPool` 自动完成这件事。

### 性能说明

缓存调优见[生产运行](https://async-geotiff-rs.pages.dev/zh/docs/guides/production/)，
可复现测量见[本地负载测量](https://async-geotiff-rs.pages.dev/zh/docs/guides/local-workload/)。
历史机器上的分析报告和原始测量保留在 Git 历史中，不随当前源代码版本分发。

- **缓存容量是最重要的调节项。** 512 MB 通常足够大型 RGB COG 的浏览视口；多数据集服务或大范围平移缩放可以使用 1024 MB。
- **使用自己的影像比较 ZSTD 与 DEFLATE。** 压缩方式、块大小和存储延迟都会影响成本，应先测量有代表性的请求，再选择 COG 创建参数。
- **`MAX_TILE_TASKS` 保持默认值**（`available_parallelism()`）。只有 profiling 证明默认值造成 CPU 争用时才覆盖它。
- **并发 single-flight。** 多个请求同时访问同一个未缓存块时，只有第一个请求负责获取和解码，其余请求等待结果；如果所有者因客户端断开而取消，等待者会重新发起 claim，而不是收到 500。

## 可观测性

库只发出 [`tracing`](https://docs.rs/tracing) 信息，不直接依赖 OpenTelemetry 或其他指标后端，因此由应用负责导出管线。主要有两个接入点：

**Span → trace。** `Tiler::tile` 和 `CogReader::read_window` 带有 `#[tracing::instrument]`，包含 source id、瓦片坐标、level 和 format 字段。可以通过 `tracing-opentelemetry` layer 接入 OpenTelemetry：

```rust
use tracing_subscriber::prelude::*;
tracing_subscriber::registry()
    .with(tracing_subscriber::EnvFilter::new("async_geotiff=info"))
    .with(tracing_opentelemetry::layer().with_tracer(tracer)) // 你的 OTLP tracer
    .init();
```

**缓存指标 → gauges/counters。** 应用创建共享的 `Arc<BlockCache>`，可以定期读取 `BlockCache::stats()` 并送入指标后端：

```rust
let s = block_cache.stats(); // BlockCacheStats { hits, misses, decoded, fetched, entries, resident_bytes }
// 命中率             = hits / (hits + misses)
// 获取放大系数        = fetched / decoded（1.0 表示没有重复冷启动获取）
// 内存压力            = resident_bytes / configured budget
```

请求速率、延迟百分位数、HTTP 204“无瓦片”比例等库外指标，应在调用 `Tiler::tile` / `MosaicTiler::tile` 的位置记录，并根据 `Ok(Some)` / `Ok(None)` / `Err` 分别统计。

## 许可证

MIT，详见 [LICENSE-MIT](https://github.com/mapseekai/async-geotiff-rs/blob/main/LICENSE-MIT)。
