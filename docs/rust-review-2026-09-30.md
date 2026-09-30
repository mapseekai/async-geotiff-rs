# Rust 代码审查：2026-09-30

> 下文保留修复前的审查证据和当时行号。用户随后要求修复全部问题；对应实现、兼容性变化和最终验证见 [修复记录](rust-review-fixes-2026-09-30.md)。

## 范围与结论

按 `rust-async-patterns` 和 `rust-pro` 审查当前工作树，两名子 agent 分别检查异步并发，以及 Rust 内存/正确性/代码组织；主 agent 复核调用链、运行已有检查与最小复现。基线 HEAD 为 `56b8d610fa68e6b3d8998f92f83508737ab35e6d`，包含已有未提交的 `src/style/raster.rs`、`src/warp/reproject.rs` 和 `examples/serve/server.rs` 优化。没有修改实现或提交代码。

重点覆盖 COG 打开与读取、快照一致性、解码与缓存、工作内存/请求/CPU 许可、取消、warp/resampler、Raster Style、MosaicJSON/STAC、示例 HTTP 服务。发现的主要风险是**错误输出、算法复杂度、资源保护边界和公开接口的输入不变量**，而不是已证实的悬垂指针或数据竞争。

P1 表示应优先修复的正确性或资源耗尽问题；P2 表示在明确输入/部署条件下触发的重要问题；P3 是配置健壮性和维护性。下面明确区分静态证据、最小运行复现和端到端验证；不把微基准换算成服务 QPS，也不把缓存保留称为永久内存泄漏。

| 优先级 | 问题 | 影响范围 | 证据 |
|---|---|---|---|
| P1 | STAC 丢弃后续分页，成功返回不完整资产集合 | `stac` 功能、多景渲染 | 源码 + 本机 HTTP 复现 |
| P1 | Mode 高基数输入使计数退化为 O(N²) | 大范围降采样、CPU 许可 | 源码 + 优化编译的内核微基准 |
| P2 | 旋转/剪切网格的 Mode 使用外接矩形，统计了足迹之外的像素 | Mode 分类结果 | 当前库最小复现 |
| P2 | 资产数上限晚于完整发现/响应解析，不能限制发现阶段内存 | STAC、大型 MosaicJSON、并发请求 | 源码 + 已安装依赖源码 |
| P2 | 公开 render 接口没有验证 mask 尺寸，返回类型为 Result 却 panic | 外部构造的 ImageData | 当前库最小复现 |
| P2 | PROJ 线程本地缓存无容量限制 | 多 CRS、长期存活工作线程 | 静态生命周期分析 |
| P2 | Alpha 拆分和 point 投影仍在 async worker 做重计算 | 低层 reader 接口、点查询 | 静态调用链 |
| P2 | 示例 `/data` 响应没有总并发/字节预算 | 直接部署示例服务 | 静态调用链 |

## 1. STAC 分页被忽略（P1）

**位置：** `src/mosaic/stac.rs:107-149`，尤其 `113-121`；消费端 `src/mosaic/mod.rs:322-337`。

`StacSource::assets_for_tile` 只调用一次 `client.search(&params)`，然后遍历当前 `ItemCollection.features`，没有处理 `links.rel="next"`。已安装的 `stac-client 0.3.0/src/client.rs:208-224` 只执行一次 POST 并解析该响应；`search` 本身不会遍历所有页。

**触发与影响：** 一个 tile 匹配的场景超过服务端页大小时，后续资产被静默遗漏；Mean/Median/Highest/Lowest 等会对错误集合计算，First 也可能因为首页不能补全覆盖而留下本可填充的空洞。`max_assets_per_tile` 只看到首页，无法判断完整匹配集合是否超限。返回成功比显式拒绝更危险，因为调用方无法识别结果被截断。

**运行证据：** 临时 harness 对 `/search` 返回一个合法 Item 和 `next` 链接；`assets_for_tile` 返回 `Ok`、资产数 1，不请求 `next`。这证明静默忽略分页；本轮没有把它扩展为真实在线 STAC 像素对拍。

**最小建议：** 在有界页数/资产数/响应字节预算内遍历分页；保留排序与分页方法/请求体语义，检测重复链接，检查取消与 deadline。未支持的分页必须显式报错，不能把首页当完整结果。可先做检测 `next` 后拒绝，关闭静默错误，再补完整支持。

**应补验证：** 两页结果、首页无指定 asset 但下一页有、循环 `next`、分页方法/请求体、超限与取消；多个 reducer 对完整资产集做像素对拍。

## 2. Mode 计数的高基数退化（P1）

**位置：** `src/warp/resampler.rs:448-466`；入口 `src/warp/reproject.rs:374-388`。

`sample_mode` 对每个样本执行 `tally.iter_mut().find`。注释假设分类数量少，但输入类型、公开 `Resampling::Mode` 和实际 warp 路径并没有保证这一点。N 个值几乎都不同时，累计比较次数约 N(N-1)/2，时间 O(N²)，计数向量空间 O(N)。

**触发与影响：** 高基数分类、误选 Mode 的连续栅格或大 source footprint 降采样。CPU limiter 只能限制同时执行的任务数，不能限制单任务的计算量；少数慢任务会长期占住许可并拖高普通请求尾延迟。阻塞任务已经开始后，deadline 不会抢占内层扫描。

**运行证据：** 直接用当前 `resampler.rs`、`rustc -O`，每次仅计算一个目标像素。主 agent 重跑：32² 不同值约 0.153 ms，64² 约 2.219 ms，128² 约 55.883 ms，256² 约 568.102 ms；256² 仅 8 类约 0.151 ms。单次微基准受预热/调度影响，但源码复杂度和数量级差距一致；这不是 HTTP 压测结果。

**最小建议：** 使用排序后连续计数，将最坏情况降至 O(N log N)，或者小 Vec 达到阈值后改为标准 HashMap 计数。处理有限值、`+0/-0` 等价以及“频次相同取最小值”的既有语义。不要通过静默截断 footprint 改变结果；加粗粒度取消检查。

**应补验证：** 几乎全不同的大 footprint、少类别快路径、tie-low、正负零、mask/nodata；同时运行慢 Mode 与小正常 tile，检查正常请求和取消后的 CPU 许可回收。

## 3. 旋转 Mode 统计了足迹之外的像素（P2）

**位置：** `src/warp/reproject.rs:374-388`；矩形采样 `src/warp/resampler.rs:438-466`。

Mode 根据导数的绝对值求 `rx/ry`，得到目标像素在 source pixel space 中的轴对齐外接框，然后对整个框计数。旋转或剪切时，实际足迹是四边形，框角包含不属于目标像素的源像素。Average 已在 `reproject.rs:360-372` 使用四角多边形；Mode 没有。

**运行证据：** 当前库 same-CRS warp：源 3×3 值为 `[9,2,9;3,1,4;9,5,9]`，GeoTransform `[0,1,1,0,-1,1]`，目标 bbox `[2,-1,4,1]`、输出 1×1、Mode。实际足迹的中心成员是菱形中的 `[2,3,1,4,5]`，按 tie-low 应为 1；实际返回 9，因为矩形角上的四个 9 被算入。

**影响：** 分类结果可能取到目标足迹之外的类别；不是单纯速度或插值误差。本例不依赖非线性 CRS，即普通旋转仿射栅格也能触发。

**最小建议：** 复用 Average 的目标像素四角计算；外接框只用于枚举候选，对源像素中心做实际多边形 membership 后才计数，保持 Mode 的中心成员和 tie-low 语义。轴对齐矩形继续保留现有快路径。

**应补验证：** 上述菱形、剪切、部分越界、mask、亚像素回退，以及轴对齐结果与旧实现一致。

## 4. 发现阶段绕过资产数和工作内存限制（P2）

**位置：** `src/mosaic/mod.rs:322-337`、`src/mosaic/stac.rs:113-149`、`src/mosaic/mosaicjson.rs:247-279`。

MosaicTiler 在 `assets_for_tile` 完整返回后才检查 `max_assets_per_tile`。MosaicJSON 先收集所有匹配 URL 引用、建立 HashSet，再为每个唯一 URL 构建两个拥有的字符串；这个阶段没有 WorkBudget 预留。STAC 同样先接收并解析整个响应，再构建 AssetRef。

已安装 `stac-client 0.3.0/src/client.rs:440-449` 在 async 函数中执行 `response.text().await` 后同步 `serde_json::from_str`，没有响应字节上限。因此 tile 的请求限流/deadline 和后续 COG 范围读取预算不能给搜索响应建立字节边界；一个搜索响应可以在拒绝“资产过多”前消耗大量内存和 worker CPU。

**触发与影响：** 大型 catalogue、z=0 前缀命中大量 quadkey、异常 STAC 响应，叠加并发时放大临时堆内存。现有 cap 有效限制资产打开与后续渲染，但不能被描述成完整请求内存保护。这是安全 Rust 的资源耗尽风险，不是越界写。

**最小建议：** 把发现预算传入 source：MosaicJSON 在遍历/首次去重时检查唯一资产 cap，超限立即报错；STAC 使用有界响应读取，在解析前限制字节并计入预算，大 JSON 解析在受控 blocking job 中执行。资产分页预算与第 1 项一并设计，避免通过增加分页再放大这一问题。

**应补验证：** 巨大 STAC body、大量重复/唯一 quadkey 资产、cap+1、低 WorkBudget、并发发现；检查拒绝发生在完整字符串物化前，并保留资产优先级。

## 5. 公开 render 未验证 ImageData 不变量（P2）

**位置：** `src/render/pipeline.rs:24-26`、`src/style/raster.rs:858-883`；公开结构 `src/pixel.rs:143-149`。

`ImageData` 字段是公开的，外部调用方可以独立构造 pixels 和 mask。公开 `pipeline::render` 返回 `AgResult`，但没有核对 mask 的 H/W 与 pixels；gray 路径直接按 pixels 尺寸索引 mask。warp 入口有相应 shape 检查，render 入口没有，造成接口保护不一致。

**运行证据：** pixels `(1,2,2)`、mask `(1,1)`、默认 gray style：当前库在 `raster.rs:883` 触发 ndarray 越界 panic，被 harness 的 `catch_unwind` 捕获。内部正常 reader 不会产生这个输入；风险来自公开 render 接口的外部调用。

**影响：** 应当可恢复的输入错误变成线程 unwind；panic=abort 的下游可能直接终止进程。这不是 unsafe 内存损坏。

**最小建议：** 在 render 共用入口验证非空维度、mask shape 和实际依赖波段，返回现有错误类型；复用已有 warp 校验逻辑中适用部分，不必新增一套 trait/框架。

**应补验证：** 短 mask、额外大 mask、空维度、缺失波段、正确 strided mask；断言返回 Err，而不是仅用 catch_unwind 掩盖 panic。

## 6. PROJ 线程缓存没有容量边界（P2）

**位置：** `src/warp/projection.rs:72-77,85-105`；源 CRS 在 reader 打开时获取，公开 Crs 可持有不同字符串。

每个线程的 `HashMap<(Crs,Crs),Rc<Projector>>` 为遇到的每对 CRS 插入 PROJ context，永不淘汰。ReaderPool 淘汰 reader、block cache 淘汰像素、请求完成都不能释放这份缓存；相同 CRS 的不同字符串也可能产生不同 key。PROJ context 是 FFI 持有的原生资源，未纳入 WorkBudget。

**触发与影响：** 长期运行、持续打开多种 CRS 或不同等价定义的数据源，且相关 blocking worker 持续存活。保留量随该线程处理过的 CRS pair 数增长，多个线程各有副本。线程退出会释放缓存；固定少数 CRS 的服务影响有限，因此不称为永久泄漏，也未声称本轮已测得 RSS 增长量。

**最小建议：** 给现有 thread-local map 加小的容量上限；先用满容量时淘汰/清理的简单策略，已有 Rc 在当前调用期间保持句柄存活。再根据命中率决定是否需要 LRU。避免跨线程共享 `!Send` 的 Projector；可安全规范化的 CRS 标识单独处理。

**应补验证：** 单个持续存活线程依次访问超容量 CRS，缓存长度有界；淘汰后转换结果不变，嵌套 `with_cached` 不破坏 RefCell 借用不变量。

## 7. 部分低层路径仍在 async worker 执行重计算（P2）

**Alpha 拆分位置：** `src/io/cog.rs:886-918,1527-1531`、`src/pixel.rs:112-123`。

大窗口 stitch 已移到受 CPU 许可保护的 blocking job，但 await 之后 `assemble_image` 回到异步线程。含 ExtraSamples Alpha 且无独立 mask IFD 时，`split_off_band` 会映射整张 alpha 并分配/复制所有剩余波段。工作量与整个窗口大小成正比，没有同步小窗口阈值，也没有 CPU permit；大窗口并发时会阻塞 worker。

**范围：** 公开 `CogReader::read_window` 与使用它的低层读取/统计。主 Raster Style `Tiler` 在 `tiler.rs:367-385` 明确拒绝 source alpha，因此不能把这一点写成普通 XYZ tile 已证实的阻塞。

**最小建议：** 把 assemble/alpha split 放入现有大窗口 stitch blocking job，复用该许可和 reservation；保留确实小的 inline stitch 路径，避免多加一次任务调度。

**点查询位置：** `src/io/point.rs:39-43,118-131`。`point` 的 async 函数同步调用 `coord_to_pixel`，每次不同 CRS 都直接 `Projector::new`，没有复用 `convert_batch` 的缓存/3857↔4326 快路径。`!Send` 只解释句柄不能跨 await，不要求在 async worker 上构造。

**最小建议：** 把点坐标转换放入受 CPU 许可保护的 blocking closure，并在 closure 内复用现有转换函数；不让 PROJ 句柄跨 await。原 MAP-26 的 tile planning 症状已由 `plan_tile_async:488-497` 修复，点查询是本次复核发现的剩余路径，不能重复报告旧 tile planning 问题。

**应补验证：** 单 worker runtime 下，大 alpha window/投影点查询与轻量 timer 并发；覆盖取消、CPU 限额、像素一致性。当前证据为调用链，未进行 heartbeat 延迟压测。

## 8. 示例 Range 服务总响应内存不受限制（P2，示例部署条件）

**位置：** `examples/serve/server.rs:313-327,859-892`，单响应上限 `:29`。

请求 admission layer 只包围 tile 路由，之后注册的 `/data/{name}` 不经过它。Range handler 允许单次最多 32 MiB，使用 `vec![0;buf_len]` 读满后返回 `Body::from(buf)`；没有独立的总响应字节预算或请求并发上限。Body 在发送/被丢弃之前持有缓冲区。

**触发与影响：** 服务端同时处理 N 个大 Range，潜在响应缓冲规模约 N×32 MiB，尚未包含网络和库解码缓存。外部客户端或多个 loopback 读取都能经过这条路线；库 WorkBudget 不会约束示例 origin 的这些副本。此项不能推广成使用外部对象存储的库调用必然有同样问题。

**最小建议：** 给 `/data` 单独的并发/字节预算，并让许可随响应 body 生命周期持有，或用受控的分块文件流。不要复用 tile admission semaphore：tile 自己读取 `/data`，共用池会引入自等待死锁。

**应补验证：** 多个被暂缓消费的响应、达到预算后拒绝/背压、Body drop 后释放；同时运行 tile loopback 请求确认无死锁。既有 MAP-29 覆盖示例服务资源加固，应补新细节而非创建同根因重复项。

## 9. 配置健壮性与规范检查（P3）

`src/io/cog.rs:545-548` 把 `max_concurrent_reads.max(1)` 直接传给 `Semaphore::new`，没有限制 MAX_PERMITS。临时 harness 在合法 fixture 上设置 `usize::MAX`，reader open 触发 Tokio panic，而非返回 AgError。这是调用方异常配置，不是远程 TIFF 可直接触发。建议与 CpuLimiter/RequestLimiter 一致 clamp 到许可上限，或在资源构造前显式验证并返回配置错误。

规范检查：`cargo fmt --all --check` 通过；`cargo clippy --all-features --lib --offline -- -D warnings` 通过；全目标 Clippy 有 10 个告警导致失败：

- `tests/concurrency_backpressure.rs:145`：attribute 后空行。
- `tests/review_regressions.rs:966`：needless borrow。
- `examples/load_audit.rs:154`：enum variant 大小差异。
- `src/io/cog.rs:2353,2374,2403,2409,2425,2585,2618`：测试配置的 field_reassign_with_default。

这些不是十个生产 bug。优先修复无争议格式/初始化问题；load-audit enum 只有少量实例时，没有必要为了 lint 强行增加 Box，可以给出窄范围的有理由豁免。已有 MAP-233 跟踪严格 Clippy 阻塞，补充当前行号/完整范围即可。

## 10. 命名、文件和函数划分

总体 snake_case/CamelCase 与 Rust 模块命名一致，`ReaderPool`、`WorkBudget`、`RequestContext` 等表达职责，没有发现需要全面重命名的系统性问题。更值得处理的是职责集中和过时的契约文字。

1. **`src/io/cog.rs` 2770 行。** 核心 reader、builder/兼容构造函数、block fetch/prefetch、装配及多个测试模块集中。可以先把测试搬到子测试模块，再把 block fetch/prefetch 实现移到 `io` 下内部模块；保留 CogReader facade 和共享资源对象，不引入新的 public abstraction。
2. **`src/style/raster.rs` 1684 行，生产代码约前 1016 行。** `RasterPlan::from_raster_style:121-350` 同时做协议适配、能力拒绝、表达式依赖、颜色/effects/image 参数编译；render 又包含 dtype 专用循环。先拆测试；针对一段明确职责提取私有 parse/compile helper，保留热循环局部性，避免每个 renderer 创建一层 trait。
3. **`warp_impl:89-419` 的职责偏多。** 同时承担输入检查、快路径、网格/footprint、有效性、采样与输出。第 3 项说明 Average/Mode 分支已经存在几何语义漂移；应优先共享 footprint 构造，采样 kernel 保持独立，之后才考虑进一步拆文件。
4. **`src/pixel.rs:5-8` 的布局文档过时。** 文档声称进入 render 前“总是”转成 H/W/bands，实际 RasterPlan 直接读取 planar `(bands,H,W)`。这是跨模块不变量说明错误，建议把 PixelData 的 planar 合同、允许非连续 stride，以及编码阶段 interleaved RGBA 分别写清；无需改变数据布局。
5. **`src/mosaic/stac.rs:4-16` 的实现笔记过时。** 仍提到并未直接依赖的 `stac 0.17`；“search 非 stream”不能被理解为“结果已经全部收集”。删除不再适用的依赖叙述，写清搜索分页与预算语义。

文件长本身不是缺陷，不建议为行数配额进行大规模移动。先修正确性与资源边界，再做一项职责明确、可独立验证的拆分；不要与现有性能优化混成一次重构。

## 11. 已复核的保护与验证边界

- 当前源代码仅发现 resampler 的一个 unsafe `get_unchecked`。它在连续布局检查后使用已 clamp 的坐标；未确认越界读写、UAF 或不正确的 Send/Sync 包装。PROJ 句柄保持线程本地。未运行 Miri、ASan、TSan，因此不能给出整个依赖栈绝对内存安全保证。
- `RequestLimiter` fail-fast admission、Context/owned CPU 与 work lease 在阻塞工作期间保留，取消并不简单提前释放许可；block-cache coalescing 和等待前 draining 也有相应保护。没有发现“取消后必然超卖许可”或“所有 cache 同时请求必死锁”的证据。
- 快照读取核对 validator/size/range，bounded response 的 Bytes owner 随最后一个 clone/drop 保留 reservation；不得把旧元数据 clone 问题当作仍然存在。
- `plan_tile_async` 已把 tile planning 放入受控 CPU job；First 顺序短路、MosaicJSON 有序前缀查找、同 reader 内 GSD 预计算均已存在。不能仅依旧 issue 名称重复提旧问题。
- `CpuLimiter` 限制 job 数，不代表 Rayon 内部工作线程数，也不是进程 RSS 上限。已有 MAP-28/MAP-185 记录线程数量/跨实例预算问题；本轮未增加无测量依据的线程池改造建议。

实际运行：

| 检查 | 结果 |
|---|---|
| `rtk cargo fmt --all --check` | 通过 |
| `rtk cargo clippy --all-features --lib --offline -- -D warnings` | 通过 |
| `rtk cargo clippy --all-features --all-targets --offline -- -D warnings` | 失败，以上 10 个测试/示例告警 |
| `rtk cargo test --all-features --lib --tests --offline` | 沙箱外 328 通过、2 ignored，5 suites |
| STAC pagination harness | 确认 next 被忽略并成功返回首页 |
| 当前库 render/rotated Mode harness | 确认 mask panic 和 rotated Mode 返回 9 |
| 当前 resampler `rustc -O` 微基准 | 确认高基数的平方级耗时增长 |
| ReaderConfig 极大 reads harness | 确认 Semaphore 构造 panic |

第一次沙箱内测试 285 通过、6 失败、2 ignored：5 个临时端口绑定被系统拒绝，1 个 DNS 依赖测试失败；在有本机网络能力的沙箱外重跑全部通过。没有把环境失败登记成代码 bug。

本轮没有重新跑生产负载/真实在线 catalogue、RSS 长稳测试、Tokio heartbeat 压测，也未以已有 TiTiler 比较文件替代本轮证据。忽略测试、独立 examples 的 test 以及 doc-tests 不包含在上述 328 计数中。

## 建议执行顺序

1. 先阻止 STAC 静默截断，修复 Mode 的几何语义与最坏复杂度，修复公开 render 校验。
2. 同时完善发现阶段预算、PROJ 缓存容量和 reader 低层 CPU 工作范围；对直接部署的 example 加独立 Range 预算。
3. 清理配置 panic、Clippy 与契约文档，最后按职责做有限拆分。每项修复留下上述最小回归；并发问题再补有明确失败条件的负载验证。

## Multica 登记结果

读取项目全部 53 条既有 issue 后去重，新增 8 条并复用现有 bug/性能优化标签：

| 条目 | 对应发现 |
|---|---|
| MAP-247 | STAC 分页遗漏 |
| MAP-248 | Mode 高基数 O(N²) |
| MAP-249 | 旋转 Mode footprint 错误 |
| MAP-250 | 发现阶段缺少预算 |
| MAP-251 | render mask shape panic |
| MAP-252 | PROJ 无界缓存 |
| MAP-253 | 大窗口 Alpha 拆分阻塞 async worker |
| MAP-254 | max_concurrent_reads 配置 panic |

给 MAP-29（示例 Range 资源限制）、MAP-26（point 投影剩余同步路径，tile planning 已修复）、MAP-233（全目标 Clippy）添加当前证据。保留这些既有 issue 的原状态，不新建重复问题。
