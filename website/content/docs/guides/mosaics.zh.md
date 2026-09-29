---
title: 镶嵌与 STAC
description: 从 MosaicJSON、STAC 或自定义数据源渲染重叠 COG 资产。
weight: 20
---

默认启用的 `mosaic` Cargo feature 提供 `MosaicTiler` 和 `MosaicSource` trait；
不需要镶嵌的应用可以关闭默认 features。由于这也会关闭 `proj`，仍需通用 CRS 转换时
请使用 `--no-default-features --features proj`。数据源将瓦片坐标映射为资产列表；
tiler 打开并重投影这些资产，归并重叠像元，然后统一渲染合成结果。

## MosaicJSON {#mosaicjson}

运行内嵌示例，或传入一个 MosaicJSON 文件：

```bash
cargo run --example mosaic_json
cargo run --example mosaic_json -- /path/to/mosaic.json
```

解析器支持 MosaicJSON 0.0.3。其 quadkey 索引定义在 `WebMercatorQuad` 中；
将 MosaicJSON 数据源用于 `WorldCRS84Quad` 会返回查询错误。

在进程启动时把示例服务连接到文件：

```bash
ASYNC_GEOTIFF_MOSAIC=/path/to/mosaic.json \
  cargo run --release --example serve
```

镶嵌瓦片随后可通过 `/mosaic/tiles/...` 获取。

## Raster Style 2.0

```rust
let style = TileStyle::from_json(r#"{
  "version": "2.0",
  "renderer": {"type": "rgb", "bidx": [3, 2, 1]},
  "stretch": {"method": "linear", "rescale": [[0, 3000]]},
  "mosaic": {"pixel_selection": "highest", "stage": "before_channels", "rank_channel": 1}
}"#)?;
let tile = mosaic.tile(coord, &style).await?;
```

`before_channels` 先合并输入依赖，再执行表达式；`after_channels` 先逐源计算表达式，
再合并输出。上例的依赖表是 `[1,2,3]`，因此 rank_channel=1 比较源波段 1；
改为 after_channels 后比较第一个输出，即源波段 3。并列保留源顺序靠前的完整元组。
mean/median 只对同一组完整有效元组逐通道求值，避免极值相加溢出。

Q2 请求示例：

```text
/mosaic/tiles/WebMercatorQuad/0/0/0.png?version=2.0&type=rgb&bidx=3&bidx=2&bidx=1&method=linear&rescale=0,3000&pixel_selection=mean&stage=after_channels
```

MapLibre 的 Source 选择 mosaic 后可以配置阶段和排序通道。资产地址仍由服务端固定，
JSON/Q2 不携带资产列表。新格式要求显式 mosaic 配置；源 Alpha 波段暂不支持，返回能力错误。
完整支持范围见[样式指南](../styling/)。

## 像元选择 {#selection}

| 策略 | 行为 |
| --- | --- |
| `First` | 第一个有效像元获胜；瓦片被完全覆盖后可提前停止。 |
| `Highest` | 按 rank_channel 选择最大值所在的完整元组。 |
| `Lowest` | 按 rank_channel 选择最小值所在的完整元组。 |
| `Mean` | 对有效样本流式求均值，内存不随资产数量增长。 |
| `Median` | 对有效样本求中位数，完成前需要保留所有参与图像。 |

`bidx` 和 `rank_channel` 都从 1 开始；后者是合并阶段的通道位置。

`MosaicConfig` 默认每批并发处理 8 个资产，每个 `MosaicTiler` 的 reader 池最多保留
256 个已打开 reader，并把每张请求瓦片的资产数硬限制为 64。超过上限时返回错误，
不会截断资产后返回部分结果。三个限制都通过 builder 配置：

```rust
use std::sync::Arc;

use async_geotiff::io::block_cache::BlockCache;
use async_geotiff::io::cache::CacheConfig;
use async_geotiff::mosaic::{HttpAccessPolicy, MosaicConfig, MosaicTiler};

let cache = Arc::new(BlockCache::new(CacheConfig::default()));
let config = MosaicConfig {
    chunk_size: 4,
    reader_capacity: 64,
    max_assets_per_tile: 32,
    ..Default::default()
};
let mosaic = MosaicTiler::builder(source)
    .config(config)
    .block_cache(Arc::clone(&cache))
    .http_policy(HttpAccessPolicy::strict())
    .build();
```

对于不可信或低缩放级别的索引，应保留资产上限以避免请求放大。数据源应提供确定性的资产顺序，
因为 first 和极值并列时依赖该顺序。Median 保留输入上限为 256 MiB，另受共享工作内存预算约束。

## STAC 示例 {#stac}

启用 `stac` 并提供全部三个启动配置：

```bash
ASYNC_GEOTIFF_STAC_URL=https://earth-search.aws.element84.com/v1 \
ASYNC_GEOTIFF_STAC_COLLECTION=sentinel-2-l2a \
ASYNC_GEOTIFF_STAC_ASSET_KEY=visual \
  cargo run --release --example serve --features stac
```

API、集合与资产 key 在进程启动时固定。HTTP 查询参数无法把示例变成任意 STAC
或 COG 代理。

启动后打开 <http://127.0.0.1:8080/maplibre>。配置 STAC 时，MapLibre 查看器默认
选择 mosaic 模式，无需猜测瓦片坐标即可验证目标集合。

## HTTP 资产策略 {#http-policy}

默认策略允许公网和私网 HTTP(S) 资产，但阻止 loopback、link-local 和云元数据地址。
对于完全不可信的文档，请使用 `HttpAccessPolicy::strict()`：它要求 HTTPS，并额外阻止
私有地址范围。`permissive()` 只应服务于运维方完全控制的资产列表。

示例服务使用默认策略，不提供环境变量覆盖。接受不可信 mosaic 或 STAC 文档的服务应
自行构造 `MosaicTiler`，并像上例一样通过 builder 传入 `strict()`。

镶嵌错误和日志会隐藏预签名 URL 的查询字符串。
访问策略是 URL 解析防护，不是完整的出站代理沙箱。应用仍需负责客户端超时、重定向
策略、出站防火墙、文档大小限制，以及自己在库外增加的日志。
