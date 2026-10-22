# Rust 审查问题修复：2026-09-30

修复 [审查报告](rust-review-2026-09-30.md) 中的全部可执行问题，包括正确性、资源保护、异常配置、Clippy 和职责划分。三个 agent 分别拥有互不重叠的 warp、reader 和 mosaic 文件；主 agent 负责 Raster Style、示例服务、合并验证及文档。没有提交或发布，保留开始前已有的性能优化及 benchmark 文件。

## 实现与回归

| 问题 | 修复 | 验证 |
|---|---|---|
| MAP-247 STAC 分页 | 遍历 GET/POST next，支持 POST merge；保留服务端顺序及首次 href 优先级。循环、多个 next、跨源链接、未支持的分页语义显式失败，不返回部分成功结果 | 多页、首批无目标 asset、GET/POST/merge、自定义认证、循环及页数上限 |
| MAP-248 Mode O(N²) | 至多 32 类直接计数；超出后转排序计数，最坏 O(N log N)。保留既有重复次数、有限值过滤、正负零等价和 tie-low | 高基数 256²、早期重复、正负零、mask/nodata |
| MAP-249 旋转 Mode | Average/Mode 共享 source pixel 四角；Mode 按真实 footprint 内源像素中心计数，保留轴对齐和薄足迹回退。同时修复完全位于左/上之外时误读边缘像素 | 菱形旧复现从 9 改为 1；剪切、mask、薄足迹、四方向越界 |
| MAP-250 发现预算 | 复用 WorkBudget/CpuLimiter；MosaicJSON 按顺序遍历并在唯一资产物化前检查 cap。STAC 有界读取后在 CPU job 解析，只保留所需字段；资产、响应、解析和分页状态持有预算。MosaicTiler 持有资产 lease 至消费结束 | 唯一 cap+1、重复资产、统计集合、单页/总字节、低 work 预算、取消及 lease 释放 |
| MAP-251 render panic | 共用 render 入口校验非空 pixels、mask 的精确 H/W 及既有依赖波段校验，返回 AgError | gray/RGB 小/大 mask、空维度、缺失波段、合法 strided mask |
| MAP-252 PROJ 缓存 | 每线程最多 32 项；简单淘汰。调用期间保留 Rc，执行回调前释放 RefCell borrow | 超容量、重用和嵌套回调；淘汰后投影仍正确 |
| MAP-253 Alpha CPU | Alpha 拆分并入现有 stitch CPU job；小窗口 inline 阈值计入额外拷贝，不再于 await 后复制整张大窗口 | 预热 512² RGBA 仍等待 CPU permit，队列取消后许可释放 |
| MAP-26 point 同步投影 | 同 CRS 保留 inline；跨 CRS 在共享 CPU permit 下 spawn_blocking，携带 RequestContext，复用缓存而不改变原投影域错误语义 | 投影与同 CRS 像素一致、CPU 队列 deadline、许可释放 |
| MAP-29 示例 Range | `/data` 独立 64 个响应 slot 和 128 MiB 缓冲预算，保留原单响应 32 MiB 上限；用 Bytes owner 保留 lease 至最后一个 clone/drop，不与 tile 池共用 | 未消费响应达到预算后 503；Body→Bytes、clone/drop、HEAD 释放；占满 tile slot 后 data 仍可响应；已有 HTTP tile loopback 通过 |
| MAP-254 reads 配置 | 钳制到 `1..=Semaphore::MAX_PERMITS` | 0、usize::MAX 打开不 panic |
| MAP-233 Clippy | 清理测试初始化、空行和 needless borrow；只有少量 workload 对象的 load-audit enum 采用有理由的窄豁免 | 全 feature、全目标严格 Clippy 通过 |

## 规范与职责划分

- `io/cog.rs` 保留 reader facade；block/prefetch 放入私有 `cog_blocks.rs`。测试归属整理后，私有测试位于 `io/cog.rs` 内，公开 API 测试位于 `tests/io/cog.rs`。搬迁 helper 的函数体保持原语义。
- `style/raster.rs` 保留执行和热循环；协议适配放入私有 `raster/compiler.rs`，提取输入及颜色编译 helper；私有测试位于源码内，协议与渲染集成测试分别位于 `tests/style/params.rs` 和 `tests/render/pipeline.rs`。
- warp 提取共享控制网格及精确足迹角点计算，测试放入 `reproject_test.rs`；保留原有 native pixel 性能优化及对应测试。
- 修正 PixelData 的 planar `(bands,H,W)`、非连续 stride 说明；更新 STAC 分页和预算文档。没有进行无依据的全局重命名或增加 renderer trait。

## STAC 兼容性

`StacSource::new` 调用形式不变，默认关闭自动代理发现。自定义认证、代理、timeout 使用新增 `with_http_client(api_url, original_reqwest_0_11_client, collections, asset_key)?`。

旧 `with_client(stac_client::Client, ...)` 保留签名但标记 deprecated，发现请求显式拒绝。stac-client 0.3 不公开其 HTTP/auth transport，公开 search 会无界缓冲；不能通过创建匿名替代客户端修复而丢失认证。迁移时传入原 reqwest 0.11 客户端，将额外认证层表示为其 default headers。README 已同步说明。

默认发现限制为单响应 1 MiB、总响应 8 MiB、128 页和资产元数据 8 MiB；asset 数由 MosaicConfig 传入（默认 64），count=0 不关闭其它边界。直接 source 调用可通过公开 DiscoveryOptions 调整。第三方 MosaicSource 的兼容默认实现只能验证已经返回的资产，需覆写两个 `_with_options` 方法才能在其自有 IO/分配前施加限制。

## 验证边界

工作预算采用保守估算和 lease，不是 allocator 的逐字节 RSS 计量。PROJ 缓存容量也不等同于原生堆字节上限。公开同步 warp 没有硬抢占机制；Mode 修复最坏算法复杂度，不声称可强制停止任意同步 CPU 工作。未进行生产 catalogue、长稳 RSS、完整服务吞吐或 Miri/ASan 测量。

最小 Mode 内核微基准直接编译当前 `resampler.rs`（`rustc -O`），256² 唯一值一个目标像素：修复前主 agent 记录约 568.102 ms，修复后重跑约 3.358 ms；8 类约 0.188 ms。单次内核数据受环境影响，不能换算成服务 QPS。

最终统一验证完成于 2026-10-01（本次审查从 2026-09-30 延续）：

| 命令 | 结果 |
|---|---|
| `rtk cargo test --all-features --lib --tests --examples --offline` | 365 passed、2 既有 ignored，9 suites |
| `rtk cargo test --all-features --doc --offline` | 5 passed |
| `rtk cargo test --no-default-features --lib --tests --offline` | 255 passed、2 既有 ignored，5 suites |
| `rtk cargo test --no-default-features --features stac --lib mosaic::stac --offline` | 8 passed |
| `rtk cargo test --all-features --lib mosaic::stac --offline` | 8 passed |
| `rtk cargo clippy --all-features --all-targets --offline -- -D warnings` | 通过 |
| `rtk cargo clippy --no-default-features --features stac --all-targets --offline -- -D warnings` | 通过 |
| `rtk cargo fmt --all --check`、`rtk git diff --check` | 通过 |

需要临时 HTTP listener 的测试在支持 localhost 的沙箱外运行；没有把沙箱端口/DNS 拒绝当成代码回归。GitNexus impact 在修改既有符号前执行并报告；detect_changes 复核了读取、warp、渲染、mosaic 和服务调用链。索引结果包含工作开始前已有优化，不能把整个工作树 diff 归为本轮修改；新增/迁移文件另由源代码和 Cargo 检查覆盖。

MAP-247 至 MAP-254 已添加修复/验证记录并标记 done；MAP-26、MAP-29、MAP-233 添加了本轮处理结果，保留各自既有状态。没有新建重复 issue。
