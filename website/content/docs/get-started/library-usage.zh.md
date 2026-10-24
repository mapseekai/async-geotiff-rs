---
title: 作为库使用
description: 直接从 Rust 打开 COG 并渲染瓦片。
weight: 30
---

仓库当前设置了 `publish = false`，因此其他本地项目在开发期需要使用路径依赖。先在
仓库旁创建应用；如果目录布局不同，请调整相对路径：

```bash
# 从 async-geotiff-rs 检出目录开始：
cd ..
cargo new async-geotiff-demo
cd async-geotiff-demo
```

```toml
[dependencies]
async-geotiff = { path = "../async-geotiff-rs" }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
```

如果要避免系统 `libproj`，同时保留示例服务使用的 mosaic API，可以在路径依赖上设置
`default-features = false, features = ["mosaic"]`；此时无法执行通用 CRS 转换。

## 渲染一张瓦片 {#render-one-tile}

```rust
use std::sync::Arc;

use async_geotiff::io::cog::CogReader;
use async_geotiff::style::TileStyle;
use async_geotiff::tiler::Tiler;
use async_geotiff::tms::{TileCoord, TileMatrixSet};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = "https://raw.githubusercontent.com/cogeotiff/rio-tiler/0b08b7f35a8b639cee2f35a0cb565034f7b55bfd/tests/fixtures/cog.tif";
    let reader = Arc::new(
        CogReader::builder()
            .source_id(url)
            .open_http(url)
            .await?,
    );
    let tiler = Tiler::new(reader);
    let style = TileStyle::from_raster_query("version=3.0&type=pseudocolor&bidx=1&colormap_name=viridis&method=linear&rescale=0,3000")?;
    let coord = TileCoord {
        tms: TileMatrixSet::WebMercatorQuad,
        z: 0,
        x: 0,
        y: 0,
    };

    if let Some(bytes) = tiler.tile(coord, &style).await? {
        std::fs::write("tile.png", bytes)?;
    } else {
        eprintln!("tile is outside the dataset or projection domain");
    }
    Ok(())
}
```

`TileStyle::from_raster_query` 接受与示例 HTTP 服务相同的查询字符串，但不包含开头的 `?`。
将示例保存为 `src/main.rs`，然后执行 `cargo run`。固定 URL 是一个支持 Range 的
小型测试 COG；确认集成成功后再替换为自己的数据。

## 点查询与地理范围 {#point-and-bounds}

`CogReader::point(x, y, &coord_crs, bidx)` 返回最近像素的值，不执行渲染。坐标使用
`coord_crs`；影像外的点返回 `None`。波段从 1 开始编号，对应 `read_window` 输出的
波段；空选择按现有顺序返回全部输出波段。

没有独立 TIFF mask 时，首个 Alpha 样本会转为有效性 mask 并从输出波段中移除，
因此普通 RGBA 的点查询返回 RGB。有独立 mask 时，reader 保留原生波段并使用该
mask。无效像素返回 `Some(PointValue { valid: false, .. })`，不会被误判为波段
选择错误。Raster Style 渲染对 Alpha 源的独立限制仍然适用。

`wgs84_bounds()` 返回用于数据集统计的地理包络。投影足迹包含极点时，包络包括该
极点，经度范围为 `[-180, 180]`。包络可能包含影像之外的区域；实际覆盖仍应通过
像素查询或有效性 mask 判断。

低层 `io::stats::Histogram::quantile` 接受公开的 `u64` 分桶计数，在累计时避免 `u64`
溢出，也适用于直接构造的 histogram。100% 分位返回最后一个非空桶的上边界，
即使前面的计数非常大，也不会丢掉末尾的小计数桶；尾部空桶不参与该端点。
Raster Style 统计不使用这些嵌入式直方图。

## 共享内存与 CPU 预算 {#shared-budgets}

处理多个 COG 时，请创建一个块缓存，并为每个 reader 指定稳定且互不相同的源 ID：

```rust
use std::sync::Arc;

use async_geotiff::io::block_cache::BlockCache;
use async_geotiff::io::cache::CacheConfig;
use async_geotiff::io::cog::CogReader;

let cache = Arc::new(BlockCache::new(CacheConfig::default()));

let first = CogReader::builder()
    .source_id("scene-a")
    .block_cache(Arc::clone(&cache))
    .open_http("https://example.com/a.tif")
    .await?;

let second = CogReader::builder()
    .source_id("scene-b")
    .block_cache(Arc::clone(&cache))
    .open_http("https://example.com/b.tif")
    .await?;
```

共享同一缓存的 reader 共用默认 512 MiB 预算。源 ID 是缓存键的一部分，只有相同逻辑
COG 才应复用同一个 ID。
