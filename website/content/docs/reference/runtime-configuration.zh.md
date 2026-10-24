---
title: 示例服务配置
description: 内置 Axum 服务接受的环境变量。
weight: 30
---

以下变量配置 `cargo run --example serve`。它们是示例服务约定，不是库的全局配置。

| 变量 | 默认值 | 含义 |
| --- | --- | --- |
| `ASYNC_GEOTIFF_BIND` | `127.0.0.1:8080` | HTTP 服务监听地址。 |
| `ASYNC_GEOTIFF_BLOCK_CACHE_MB` | `512` | 共享解码块缓存容量（MiB），必须为正数。 |
| `ASYNC_GEOTIFF_MAX_DECODE_TASKS` | 可用并行度 | 最大并发解码/拼接 CPU 任务数，必须为正数。 |
| `ASYNC_GEOTIFF_MAX_TILE_TASKS` | 可用并行度 | 最大并发重投影/渲染 CPU 任务数，必须为正数。 |
| `ASYNC_GEOTIFF_MAX_IN_FLIGHT_REQUESTS` | `64` | 瓦片请求共享准入上限，必须为正数。 |
| `ASYNC_GEOTIFF_REQUEST_TIMEOUT_MS` | `30000` | 含数据集查找/打开的瓦片请求超时，单位毫秒，必须为正数。 |
| `ASYNC_GEOTIFF_PREFETCH_RING` | `1` | 默认/本地单 COG reader 的原生块预取圈数；`0` 为关闭。 |
| `ASYNC_GEOTIFF_SLOW_TILE_MS` | 未设置 | 超过此正整数毫秒阈值时发出慢瓦片警告。 |
| `ASYNC_GEOTIFF_MOSAIC` | 未设置 | 启动时加载的 MosaicJSON 文件路径。 |
| `ASYNC_GEOTIFF_STAC_URL` | 未设置 | 固定 STAC API URL；需要 `stac` 及后两个变量。 |
| `ASYNC_GEOTIFF_STAC_COLLECTION` | 未设置 | 固定 STAC collection ID。 |
| `ASYNC_GEOTIFF_STAC_ASSET_KEY` | 未设置 | 固定 STAC COG 资产 key。 |
| `ASYNC_GEOTIFF_TOKIO_CONSOLE` | 关闭 | 使用 `tokio-console` 编译时，以 `1`、`true`、`yes` 或 `on` 启用 Tokio Console。 |
| `RUST_LOG` | `async_geotiff=debug,warn` | 示例服务的标准 tracing 过滤器。 |

`ASYNC_GEOTIFF_MOSAIC` 与 STAC 配置互斥；三个 STAC 变量必须一起提供。

解码与瓦片任务变量用于调优单 COG 工作，但不构成一个进程级 CPU 上限：命令行默认
COG 使用独立 limiter；延迟选择的本地数据集共享一个取两者较大值的 limiter；mosaic
拥有自己的 limiter。示例服务没有为 `max_assets_per_tile` 等 `MosaicConfig` 字段
提供环境变量。

请求准入与超时配置同时作用于命令行默认 COG、延迟加载的 `?dataset=`、MosaicJSON
和 STAC 瓦片。这些路径共享配置后的库准入 limiter；HTTP 请求取消后仍在运行的 CPU
工作也受其约束。调高 HTTP 配置时，库层配置同步生效，不再额外保留固定的 64 请求准入
上限或 30 秒总请求时限。传输层连接/读取超时仍独立生效。超额请求返回 503，超时请求返回 504。
请求到达截止时间后，即使响应或传输错误已就绪，也优先返回 504；首次延迟打开数据集
同样适用。截止时间前观察到的传输错误保留原有状态码。

预取配置对两种单 COG 入口均生效。Mosaic 资产 reader 保留原有的零预取默认值，
服务没有提供 Mosaic 预取配置。

HTTP Mosaic 资产错误的诊断标签不包含 URL 用户名/密码、查询串或 fragment。
Store backend 的对象 key 保持既有语义。源凭据保留在服务端配置中；瓦片调用者无需
提供原始资产 URL。

## 示例 {#example}

```bash
ASYNC_GEOTIFF_BIND=0.0.0.0:8080 \
ASYNC_GEOTIFF_BLOCK_CACHE_MB=1024 \
ASYNC_GEOTIFF_MAX_DECODE_TASKS=8 \
ASYNC_GEOTIFF_MAX_TILE_TASKS=8 \
ASYNC_GEOTIFF_PREFETCH_RING=0 \
ASYNC_GEOTIFF_SLOW_TILE_MS=500 \
RUST_LOG=async_geotiff=info \
  cargo run --release --example serve -- https://example.com/cog.tif
```

无效数值会使进程在启动时失败，而不会被静默修正。
