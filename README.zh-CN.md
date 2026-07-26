# async-geotiff

[English](README.md) | 中文

一个纯 Rust 异步库，用于读取云端托管的 Cloud-Optimized GeoTIFF（COG），并以 XYZ 地图瓦片形式提供数据，支持类似 titiler 的 URL 样式参数。

**不依赖 GDAL。** 栅格 IO 使用 [`async-tiff`] 与 [`object_store`]；重投影使用可通过 feature 控制、依赖系统 `libproj` 的 [`proj`]；仿射计算使用 [`raffine`]；色带使用 [`colorous`] 与 [`enterpolation`]；重采样使用 [`fast_image_resize`]；PNG 编码使用 [`image`]。

## 快速开始

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
curl 'http://127.0.0.1:8080/tiles/WebMercatorQuad/0/0/0.png?colormap_name=viridis&rescale=0,3000'
```

## 波段计算

使用 `expression` 计算派生单波段。`bN` 表示从 1 开始计数的源波段；表达式支持数值、括号、一元负号以及 `+`、`-`、`*`、`/`：

```bash
curl 'http://127.0.0.1:8080/tiles/WebMercatorQuad/0/0/0.png?expression=(b5-b4)/(b5%2Bb4)&rescale=-1,1'
```

`expression` 不能与 `bidx` 或 `color_formula` 同时使用。引用波段中的 nodata、除零和其他非有限计算结果会输出透明像元，不会被替换为 0。

表达式的完整语法如下：

- 波段引用：`b1`、`b4`、`b5` 等，从 1 开始计数；大写的 `B` 也支持。
- 有限十进制数字：例如 `2.5`、`0.5`。
- 括号、二元运算 `+`、`-`、`*`、`/`，以及一元负号 `-`。

乘法和除法的优先级高于加法和减法；同一优先级的运算从左到右计算。不支持科学计数法，例如 `1e3` 无法使用。

常见的 NDVI 表达式是 `(b5-b4)/(b5+b4)`。在 URL 查询参数中，`+` 和 `/` 最好进行百分号编码：

```bash
curl 'http://127.0.0.1:8080/tiles/WebMercatorQuad/0/0/0.png?expression=(b5-b4)%2F(b5%2Bb4)&rescale=-1,1'
```

其他有效示例：

```text
b1/2
-b1+3
(b2-b1)/(b2+b1)
```

`expression` 始终生成一个派生波段，因此不能与 `bidx` 或 `color_formula` 同时使用，且最多只能传入一组 `rescale` 范围。表达式引用的源波段如果是 nodata，输出透明像元；除零和其他非有限结果也会输出透明像元，不会替换成 0。表达式长度最多 4096 字节、256 个 token，嵌套深度最多 128 层。

默认支持 `png`；启用 feature 后也支持 `webp`、`jpg` 和 `jpeg`：

```bash
cargo run --release --features webp,jpeg --example serve -- https://example.com/cog.tif
```

常用参数：

- `bidx=1&bidx=2&bidx=3`：选择波段。
- `rescale=0,3000`：按指定最小值和最大值拉伸，可重复传入。
- `stretch=percent&pc=2,98`：百分位拉伸。
- `stretch=stddev&sigma=2.5`：按均值和标准差拉伸。
- `colormap_name=viridis`：为单波段数据应用色带。
- `resampling=average`：读取或缩放时的重采样方式，默认 `nearest`。
- `reproject=bilinear`：重投影时的重采样方式。
- `tilesize=512`：输出瓦片大小，默认 `256`。

支持的 TileMatrixSet：`WebMercatorQuad`（EPSG:3857）和 `WorldCRS84Quad`（EPSG:4326）。

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

## URL 接口

瓦片路径格式为 `/tiles/{tms}/{z}/{x}/{y}.{ext}`，其中 `ext` 为 `png`、`webp`、`jpg` 或 `jpeg`；后面三种格式需要启用对应的 Cargo feature。

| 参数 | 可重复 | 示例 | 说明 |
|---|---:|---|---|
| `bidx` | 是 | `?bidx=1&bidx=2&bidx=3` | 从 1 开始的波段选择 |
| `rescale` | 是 | `?rescale=0,3000` | 每个波段的 `min,max` 拉伸，优先级最高 |
| `stretch` | 否 | `?stretch=percent&pc=2,98` | `minmax`、`percent` 或 `stddev` |
| `pc` | 否 | `?pc=2,98` | 百分位裁剪，默认 `2,98` |
| `sigma` | 否 | `?sigma=2.5` | 标准差倍数，默认 `2.0` |
| `nodata` | 否 | `?nodata=nan` | 覆盖数据集 nodata 值 |
| `colormap_name` | 否 | `?colormap_name=viridis` | `viridis`、`magma`、`inferno` 等色带 |
| `color_formula` | 否 | `?color_formula=gamma%20RGB%201.5` | rio-color 子集 |
| `resampling` | 否 | `?resampling=average` | 读取和缩放时的重采样方式 |
| `reproject` | 否 | `?reproject=bilinear` | 重投影时的重采样方式 |
| `tilesize` | 否 | `?tilesize=512` | 默认 256，可用 64、128、256、512、1024 |

默认瓦片大小是 **256**，而不是 titiler 的 512，以兼容更多 XYZ 客户端。

### 数据拉伸

UInt16、Int16 和 Float COG 需要值域，否则可能渲染为饱和白色。优先级为：显式 `rescale` → `stretch` 模式 → 未指定参数时的 `stretch=stddev&sigma=2.0`。U8 RGB 保留 0–255 快速路径，不会自动拉伸。

- `?rescale=470,660`：使用指定范围。
- `?stretch=minmax`：使用数据集最小值和最大值。
- `?stretch=percent&pc=2,98`：使用第 2 至第 98 百分位。
- `?stretch=stddev&sigma=2.5`：使用 `mean ± 2.5·σ`，并限制在最小值和最大值内。

统计信息优先读取 COG 内嵌的 `STATISTICS_*` GDAL 标签，否则从最高级概览读取一次并缓存。无效参数返回 HTTP 400。

## 支持的数据类型与构建要求

源 COG 支持 U8、U16、I16、U32、I32、F32 和 F64，读取和重投影边界处统一为 F64。

- Rust ≥ 1.88，与 `Cargo.toml` 中的 `rust-version` 一致。
- 默认 `proj` feature 需要系统安装 `libproj` 9.x。
- macOS：`brew install proj`。
- Debian/Ubuntu：`apt install libproj-dev libsqlite3-dev libtiff-dev clang`。
- 如不需要重投影，可使用 `cargo build --no-default-features`。

## 架构与性能

处理流程为：HTTP 请求 → TMS 瓦片范围 → `CogReader` 读取 → 可选重投影 → 拉伸、着色和合成 → PNG/WebP/JPEG 编码。

CPU 密集型解码、拼接、重投影和渲染通过受信号量限制的 `spawn_blocking` 执行。默认共享解码块缓存预算为 **512 MB**；多数据集服务可根据内存情况调大。`MosaicTiler` 会通过 `ReaderPool` 自动共享缓存。

性能调优建议：

- 缓存容量通常是最有效的调节项。
- 创建 COG 时优先使用 ZSTD；基准测试显示其块解码速度明显快于 DEFLATE。
- `MAX_TILE_TASKS` 默认使用主机可用并行度，只有在 profiling 证明存在 CPU 争用时才覆盖。
- 相同未缓存块的并发请求使用 single-flight，避免重复下载和解码。

## 可观测性

库只发出 [`tracing`](https://docs.rs/tracing) 信息，不直接依赖 OpenTelemetry 或其他指标后端。`Tiler::tile` 和 `CogReader::read_window` 提供 tracing span；应用可以定期读取 `BlockCache::stats()` 并接入自己的指标系统。

## 许可证

MIT OR Apache-2.0
