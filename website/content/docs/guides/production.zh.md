---
title: 生产运行
description: 配置缓存、限制 CPU、保护远程资产并导出可观测性数据。
weight: 30
---

## 共享一个缓存 {#cache}

解码块缓存的默认预算是 512 MiB。拥有多个 reader 的服务应把同一个
`Arc<BlockCache>` 注入全部 reader 和 mosaic，并为每个不同 COG 使用稳定的源 ID，
避免缓存键冲突。

这个预算只覆盖缓存中常驻的解码块，不等于进程总 RSS。飞行中的压缩字节、解码和
重投影缓冲区、编码响应、reader 元数据与 mosaic reducer 状态会额外占用内存；其中
`Median` 会保留全部参与图像直到归并完成。

默认瓦片入口共享最多准入 64 个请求的 `RequestLimiter`，以及独立于解码缓存、
用于活跃工作的 512 MiB `WorkBudget`，也可以注入自定义实例。这些计账上限不等于
严格 RSS 上限；仍需限制单瓦片资产扇出和调用方保留的输出，并用有代表性的负载确定
容器内存。默认值、内存竞争下的有界重试、取消和源快照说明见
[运行限制与源一致性](/zh/docs/guides/runtime-limits/)。

通过 `BlockCache::stats()` 可观察：

- `hits` 与 `misses`
- `decoded` 与 `fetched`
- `entries`
- `resident_bytes`

可进一步计算命中率、fetch/decode 放大比，以及常驻字节占配置预算的比例。

## 限制 CPU 工作 {#cpu}

解码、拼接、重投影、镶嵌归并、渲染与编码都是 CPU 密集型工作。它们通过受
`CpuLimiter` 限制的 `spawn_blocking` 运行。非 mosaic `CogReader` 与 `Tiler` 接受
调用方提供的 limiter；`MosaicTiler` 在资产与最终渲染之间共享一个内部 limiter，
但不能接入非 mosaic limiter。默认值取主机可用并行度，并把 0 修正为至少一个 permit。

`Tiler::new` 会复用其 reader 携带的 limiter：

```rust
use std::sync::Arc;

use async_geotiff::CpuLimiter;
use async_geotiff::io::cog::CogReader;
use async_geotiff::tiler::Tiler;

let cpu = CpuLimiter::new(8);
let reader = Arc::new(
    CogReader::builder()
        .source_id("scene-a")
        .cpu_limiter(cpu.clone())
        .open_http("https://example.com/a.tif")
        .await?,
);
let tiler = Tiler::new(reader);
```

示例服务的 CPU 计数方式取决于数据路径。命令行默认 COG 使用相互独立的 reader 解码
与 tiler 渲染 limiter，两阶段可以重叠；延迟选择的本地数据集复用一个 `AppState`
limiter，其大小取两项配置中的较大值；`MosaicTiler` 还拥有自己的共享 limiter。因此
两个环境变量是组件调优项，不是通用的进程级 CPU 上限。需要单一上限时，应在库级别把
同一个 `CpuLimiter` 注入所有支持注入的非 mosaic reader 与 tiler。

当前 `MosaicTiler` 拥有内部共享 limiter，但 builder 不支持注入非 mosaic 路径的
limiter。因此，目前不支持对混合 mosaic 与单 COG 流量设置严格的数值型 CPU-job
上限。库外请求并发限制只能提供粗粒度背压，并不等同于共享 permit 预算。

不要在没有测量时主动降低瓦片并发。过小的限制会给每次瓦片请求增加排队时间；应先
观察 P95 延迟与 CPU 竞争。

## 调整访问模式 {#access-patterns}

- 多数据集服务或大范围平移/缩放场景可增加缓存容量。
- 读取延迟敏感时优先使用 ZSTD 压缩 COG，而不是 DEFLATE。
- 示例服务默认预取一圈原生块；不希望产生额外对象存储 GET 时，设置
  `ASYNC_GEOTIFF_PREFETCH_RING=0`。
- 对不可信索引保持 `MosaicConfig::max_assets_per_tile` 有界。

## 导出 trace 与指标 {#observability}

`Tiler::tile` 和 `CogReader::read_window` 会发出 `tracing` span。需要 OTLP trace 时，
在应用中连接 `tracing-opentelemetry` layer；库本身有意不依赖 OpenTelemetry。

在 HTTP 或应用边界记录请求量、延迟、错误量以及 `Ok(None)` 比例；另行轮询缓存统计，
观察内存与远程读取行为。

## 保护远程访问 {#security}

当 mosaic 或 STAC 文档不可信时使用严格的 `HttpAccessPolicy`。凭据应配置在
`object_store` 实例上，而不是嵌入资产标识。应用边界也应避免记录原始预签名 URL。
