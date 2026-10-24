---
title: 运行限制与源一致性
description: 工作内存、请求准入、取消和固定对象快照的契约。
weight: 40
---

## 工作内存与缓存 {#memory}

默认 reader、tiler 和 mosaic pool 共享 `WorkBudget::global()` 的 512 MiB
活跃工作预算。`WorkBudget::new(bytes)` 创建独立预算；通过
`ReaderConfig::work_budget`、`Tiler::with_work_budget` 和
`MosaicTiler::builder(...).work_budget(...)` 注入同一个实例，才能共享应用上限。
`used_bytes()`、`peak_bytes()` 和 `limit_bytes()` 提供计账数据。

低层预留失败立即返回 `AgError::ResourceLimit`。公开 tile、warp、mosaic 请求会先
退出失败的一轮，再等待独占准入，最多重试一次；等待和重试都计入原 deadline。
单个请求本身超预算时仍失败。直接调用 `CogReader` 不提供这种请求级重试。
阻塞任务和受管理的响应缓冲区会保留预留，直到最后一个持有者释放。

这不是操作系统 RSS 上限。解码缓存、调用方持有的完成图像、分配器、TLS 和应用自身
分配另有生命周期。通过 `CacheConfig::max_bytes` 限制缓存，并限制输出保留数量。

| Reader 设置 | 默认值 |
| --- | ---: |
| 源窗口像素 | 16 × 1024 × 1024 |
| 窗口字节／压缩抓取批次 | 256 MiB |
| 每窗口块数 | 4096 |
| 单块声明的解码字节 | 64 MiB |
| 打开时累计元数据 | 8 MiB |
| IFD 数量 | 256 |
| 每 IFD 条目 | 4096 |
| 并发前台窗口 | 8 |

使用 `ReaderConfig::default()` 并明确覆盖字段。超限、算术溢出和非法元数据在相应
抓取／分配前失败。后台预取即时尝试获取预算，前台占用时跳过。内置解码器检查
未压缩、Deflate/OldDeflate、LZW、ZSTD 和 JPEG 输出形状；调用方自行提供的解码器
仍由调用方负责。

## 请求准入与取消 {#requests}

默认共享 `RequestLimiter` 准入 64 个瓦片请求，超出立即返回 `AgError::Overloaded`。
使用 `Tiler::with_request_limiter` 或 `MosaicTilerBuilder::request_limiter`
共享自定义实例；mosaic 内部资产任务不会再次占用请求名额。

默认 timeout 为 30 秒，包含 I/O 和 permit 等待。`TilerConfig`、`MosaicConfig`
提供 `max_in_flight_requests` 与 `request_timeout`，构造时可使用
`..Default::default()`。`tile_with_options` 和单源 `warp_tile_with_options`
接受含单次 timeout、`RequestCancellation` 的 `RequestOptions`。
`cancel()` 返回 `AgError::Cancelled`；超时返回 `AgError::DeadlineExceeded`。
到达或超过截止时间后才观察到的结果统一返回 `DeadlineExceeded`，即使底层操作
已返回成功或传输错误。截止时间前观察到的传输错误保留原有错误。

取消会停止后续阶段并丢弃待完成的 I/O，但不会抢占已执行的 CPU 指令，也不会强制
终止远端处理。仍在运行的阻塞任务保留准入和内存计账，直至结束。直接打开 reader
发生在 tile 请求之外，需要包含打开时间时，应由应用增加外层 timeout。

示例服务将 `ASYNC_GEOTIFF_MAX_IN_FLIGHT_REQUESTS` 与
`ASYNC_GEOTIFF_REQUEST_TIMEOUT_MS` 传到默认 COG、本地懒加载、MosaicJSON 和 STAC
入口，HTTP deadline 包含数据集查找／打开；增大设置后不会残留固定的 64 请求或
30 秒总 deadline。传输层连接／读取 timeout 独立。单 COG 支持
`ASYNC_GEOTIFF_PREFETCH_RING`，mosaic reader 默认不预取。

HTTP mosaic 错误标签移除 URL userinfo、query 和 fragment；对象存储 key 保持原语义。

## Mosaic 顺序与波段 {#mosaic}

MosaicJSON 去重保留资产列表中的首次出现顺序。前缀查找按 quadkey 字典序遍历，
保留各列表顺序。样式的 `raster:mosaic` 设置 `pixel_selection`、`stage` 和从 1
开始的 `rank_channel`。`before_channels` 使用按波段号排序并去重的源依赖；
`after_channels` 使用计算后的输出通道。整个 tuple 有效才能参与，Highest/Lowest
保留获胜资产的完整 tuple，同值时按资产顺序决定。

源依赖分别 warp，避免无关波段的 nodata 影响插值。Gamma、色带和透明度在归并后
执行一次。仅支持 calibration `none`；旧 `unscale` 参数已移除。`warp_tile`
返回紧凑依赖或计算通道，不再返回全部源波段。

## 固定对象快照 {#snapshots}

受管理的 `open_http` 和 `open_object_store` 默认使用
`SnapshotPolicy::RequireValidator`，需要 strong ETag 或不可变对象版本。
条件读取和缓存身份将元数据与像素固定到同一快照；源发生变化时返回错误，避免
混合版本。支持版本的存储可继续提供固定的旧版本，本地受管理打开也提供 validator。

不透明 `ObjectReader` 无法提供底层条件请求，因此 `.open(reader)` 要求显式设置
`.snapshot_policy(SnapshotPolicy::AssumeImmutable)`。这由调用方保证源不可变，
不是库验证出的快照一致性。

一致性依赖真实 validator 和后端原子写入契约，无法检测复用旧 validator 的字节
变更或违反契约的原地文件写入。没有自动刷新：直接 reader 应重新打开；mosaic 可用
`invalidate_source(key, source_id)` 或新的源版本身份触发重开。已持有的 reader
`Arc` 仍指向原快照；旧版本不可获取时，其未缓存读取会失败。
