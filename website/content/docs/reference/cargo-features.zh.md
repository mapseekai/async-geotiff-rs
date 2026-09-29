---
title: Cargo features
description: 编译期能力及其依赖影响。
weight: 20
---

| Feature | 默认 | 作用 |
| --- | ---: | --- |
| `proj` | 是 | 通过系统 `libproj` 启用通用 CRS 转换。 |
| `mosaic` | 是 | 启用 `MosaicTiler`、`MosaicSource`、MosaicJSON 和异步资产扇出。 |
| `webp` | 否 | 通过 `image` 启用无损 WebP 编码。 |
| `jpeg` | 否 | 启用底层 JPEG 编码器；Raster Style 样式请求暂不支持 JPEG。 |
| `stac` | 否 | 添加 STAC 客户端，并隐含启用 `mosaic`。 |
| `perf-tracing` | 否 | 发出更详细的内部耗时事件。 |
| `tokio-console` | 否 | 启用 Tokio Console，并隐含启用 `perf-tracing`。 |

示例：

```bash
# 默认：投影 + 镶嵌 + PNG
cargo build --release

# 不包含通用 PROJ 与镶嵌的最小库
cargo build --release --no-default-features

# 包含全部图像格式的示例服务
cargo run --release --example serve --features webp

# STAC 镶嵌示例
cargo run --release --example serve --features stac

# 完整验证面
cargo test --all-features --locked
```

`serve`、`mosaic_source` 和 `mosaic_json` 示例要求启用 `mosaic`。PNG 始终可用。
