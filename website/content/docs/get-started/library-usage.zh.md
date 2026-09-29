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
    let style = TileStyle::from_raster_query("version=2.0&type=pseudocolor&bidx=1&colormap_name=viridis&method=linear&rescale=0,3000")?;
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
