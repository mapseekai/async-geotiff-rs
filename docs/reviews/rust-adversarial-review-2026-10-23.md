# Rust 四轮对抗审查 — 2026-10-23

> 更新：8 项发现已在 2026-10-24 完成本地修复。修复前记录保留如下；当前结果见文末“全部修复与验证”。

基线：`4fccf2f09aaa2c8cd96bd0545d44183fac7feafa`，`main`，初始工作区干净。审查当前完整代码，不限于最近提交。此次只审查和记录，不修改生产代码。

## 结论

确认 5 项 P2 行为问题（其中一项为有条件的凭据脱敏缺口）、2 项限定输入下的 P3 边界问题，以及 1 类公共 API 文档规范缺口。没有确认 P0/P1。现有测试矩阵全部通过，但缺少下述复现形状的覆盖。

## 依据、范围与四轮过程

- 指定 [rust-skills](/Users/zhang/.agents/skills/rust-skills/SKILL.md)，按所有权、错误、内存、API、异步、优化、命名、类型、测试、文档、性能、结构、lint、反模式检查适用项，并阅读相关规则文件。
- [本地 Rust Code Style](/Users/zhang/code/code-style/rust-code-style.md)，文件最近提交 `8fe5deb31042e07b1224f68632b2d4c3baedf873`；内容 SHA-256 `f1fb3f772f16d96468beadc0e24f520404c9ad666062fccf8080e803cc30d934`。
- [ast-grep skill](/Users/zhang/.codex/skills/ast-grep/SKILL.md)及仓库 AGENTS。未找到本项目独立的 Rust ast-grep 规则包；本次从上述规范构造临时结构规则，用正例、反例和断言先验证，再扫描。没有安装规则、依赖或 hooks。
- 覆盖 `src/` 的 46 个 Rust 文件；连同 tests、Rust examples、bench，共 62 个 Rust 文件，另核对 Cargo/CI、公共文档和夹具辅助代码。按模块和风险覆盖，不是逐路径形式化证明。

| 轮次 | 工作 | 证据及处理 |
| --- | --- | --- |
| 1 | 三个分工审查 I/O/资源并发、渲染/样式/投影、Mosaic/HTTP/API；主审检查规范、AST、Cargo/CI及测试结构 | 固定源码快照，完成格式和严格 Clippy 检查 |
| 2 | 构造合法 Alpha TIFF、极点影像、精确对齐网格、有限极值和异常 histogram | 临时程序 path-depend 当前源码，确认公开入口行为 |
| 3 | 交换候选，主动查契约例外、已有 guard、实际调用及严重程度 | 保留有证据项；排除低层 quadkey、两处锁/await、借用容器回调等误报 |
| 4 | 主审重跑探针、合并根因、独立审查报告、核对测试日志及源码指纹 | 三个分工审查报告，独立重算 10 份测试日志；收紧影响表述；新增脱敏项经执行、调用链与信任边界交叉复核 |

GitNexus 使用已安装 CLI `analyze --index-only --pdg` 刷新至基线，保留既有 PDG；资源 context 返回当前 SHA。部分 MCP query 仍提示旧 freshness，分析器明确报告调用候选截断、流程遗漏及两处 CDG 缺失。图用于定位和调用线索，结论由当前源码及执行证明。CodeGraph 按此 checkout 查询当前源码；没有把图的空结果视作完整性证明。

## F1 · P2：精确网格快捷路径跳过 CubicSpline

位置：[tiler.rs:206](/Users/zhang/code/async-geotiff-rs/src/tiler.rs:206)、[tiler.rs:464](/Users/zhang/code/async-geotiff-rs/src/tiler.rs:464)。

`plan_tile` 只按 CRS、分辨率及像素对齐设置 `passthrough`，在核与 halo 计算前返回；`tile_in_request` 随后直接 render。CubicSpline 在整数像素中心仍使用中心 `2/3` 和相邻权重，不具有“整数点原样复制”的性质。`warp_tile` 则仍执行该核，导致同一数据/样式有不同结果。

复现：现有 `world_fixture(0, None)`、`WorldCRS84Quad/0/0/0`、64px、gray band 1，read/reproject 均为 `cubic_spline`。`tile()` 与 `warp_tile()+render()` 有 124 个像素不同；`(0,48)` 分别为 `[0,0,0,0]` 与 `[142,142,142,255]`，`(0,0)` 灰度分别为 1 与 2。主审已重跑。无非法输入、解码器差异或文档身份映射例外。

独立核探针对 3×3 邻域（中心 90，其余 0）的整数内部中心采样，Cubic 为 90，CubicSpline 为 `40.00000000000001`，说明差异不只存在于边缘或 mask。该检查也由主审重跑。

建议在规划阶段对 CubicSpline 禁用 passthrough，走正常带 2px halo 的路径；只改最终 render 分支仍会缺 halo。补充精确对齐、nodata、瓦片边缘及单源/warp 等价回归。现有 passthrough 测试只检查网格容差。对应本地“优化不得破坏正确性/API 契约”，`test-proptest-properties`、性能规则。

## F2 · P2：RGBA 默认点查询选中已移除的 Alpha 波段

位置：[point.rs:94](/Users/zhang/code/async-geotiff-rs/src/io/point.rs:94)、[cog.rs:921](/Users/zhang/code/async-geotiff-rs/src/io/cog.rs:921)、[point.rs:143](/Users/zhang/code/async-geotiff-rs/src/io/point.rs:143)。

空 `bidx` 先按 TIFF `samples_per_pixel=4` 生成四个索引；合法 RGBA 通过 `read_window` 时，Alpha 转成 mask 并从 pixel bands 移除。随后 point 将这项正常转换误判为解码波段数不一致，返回 `band 4 out of range for 3 decoded bands`。

复现使用本库测试构造器相同形状的 RGBA TIFF。`read_window(0,0,0,1,1)` 成功并返回 3 bands + mask；同一像素的 `point(..., &[])` 失败，而 `[1,2,3]` 成功。该公开接口承诺空选择返回全部波段，没有拒绝 Alpha 源的契约；Raster Style 对源 Alpha 的限制不适用于这里。现有 point 回归主要覆盖 RGB/浮点数据。

主审重跑结果相同。该像素 Alpha 为 0，显式 RGB 查询正常返回 `values=[3,7,11], valid=false`；默认查询却返回错误，连合法的无效像素结果也无法表达。

随后对不透明像素重跑：`read_window(0,1,0,1,1)` 得到 3 bands、mask=true；`point(1.5,-0.5,src_crs,&[1,2,3])` 返回相同 RGB、valid=true，而空 bidx 仍报相同错误。故问题也影响普通有效像素，不局限于透明像素。

建议统一 point 与 reader 的可见波段语义，明确原生采样波段和 Alpha→mask 后波段的映射，再据此选择/验证。不能只吞掉末波段错误；需覆盖无 Alpha、Alpha、额外样本顺序及独立 mask 分支，并同步公开文档。对应 `api-parse-dont-validate`、`type-newtype-validated` 和本地 API 一致性规范。

## F3 · P2：包含极点的影像返回不完整的 WGS84 extent

位置：[projection.rs:171](/Users/zhang/code/async-geotiff-rs/src/warp/projection.rs:171)、[cog.rs:656](/Users/zhang/code/async-geotiff-rs/src/io/cog.rs:656)、[statistics.rs:149](/Users/zhang/code/async-geotiff-rs/src/style/statistics.rs:149)。

`reproject_boundary` 只密集采样四条边。极点位于源影像内部时，地理纬度的极值不在边界；调用方却把返回值作为整幅影像 extent。

复现：EPSG:3413、64×64、源范围 `[-1000000,-1000000,1000000,1000000]`。公开 `wgs84_bounds()` 返回 `[-180,76.99881553168267,179.09061955080085,80.78781297112508]`，但源中心 `(0,0)` 投影为 `(-45,90)`，明显位于返回范围之外。进一步用公开 `point(-45,89,EPSG:4326,[1])` 得到 `values=[200], valid=true`，排除了极点经度不唯一或该点实际无数据的解释。主审已重跑两个探针。

影响包括公开 bounds 错误，以及使用该 bounds 定位视口的调用方得到错误范围；本轮没有测量浏览器显示。统计代码直接使用该 bbox 构建采样网格，因而排除实际北侧覆盖；本轮未对最终统计拉伸的色值差异做单独测量。建议检测源 footprint 是否包含投影后的南/北极，必要时将纬度扩到 ±90、经度扩到完整范围；保留普通边界加密，并增加“内部有效点应落在返回 extent 内”的测试。不要只增加边界采样密度。对应本地类型/API、正确性和边界测试规范。

## F4 · P2：本地 dataset 懒加载丢失服务器运行配置

位置：[server.rs:273](/Users/zhang/code/async-geotiff-rs/examples/serve/server.rs:273)、[server.rs:289](/Users/zhang/code/async-geotiff-rs/examples/serve/server.rs:289)。

服务器将 `ASYNC_GEOTIFF_REQUEST_TIMEOUT_MS` 和 `ASYNC_GEOTIFF_MAX_IN_FLIGHT_REQUESTS` 应用于外层 HTTP 和默认 URL 的 TilerConfig，但 `?dataset=` 懒加载使用 `ReaderConfig::default()` 与 `Tiler::new`，保留内层 30 秒 deadline 与 64 个并发的默认值。设置 60000ms、128 后，本地 dataset 仍在 30 秒超时，并在第 65 个并发请求被拒绝。运行文档未声明此额外上限；CPU limiter 分域说明不构成 timeout/admission 例外。

执行探针直接引用原始 `server.rs`，通过公开 router 和本地 HTTP Range fixture，先打开 reader 元数据，再在可观测的像素 GET 处暂停源响应。HTTP 配置为 128/60s，发出 65 个请求；Tokio 虚拟时钟推进 31 秒后，得到 1 个 503（`request admission limit reached`）和 64 个 504（`request deadline exceeded`）。没有真实等待 30 秒，也没有替换被测代码。

主审已独立重跑上述探针，结果相同。相同构造路径也未接收 `ASYNC_GEOTIFF_PREFETCH_RING`：默认 URL reader 使用 server 默认 1 圈或显式覆盖，本地 dataset 保留 reader 默认 0。这是启用配置未传递；设置 0 恰好有效，不是“无法禁用”。预取结论来自配置构造源码，本轮没有测量额外 GET 数量。`serve.rs:299-302` 与 `serve.rs:315-318` 的 MosaicJSON/STAC builder 也保留默认 request 配置，`Mosaic::tile` 包装未下传外层 deadline；这两条路径为静态确认，65 请求探针只覆盖本地 dataset。

建议将有效 reader/tiler 配置传入懒加载构造，并统一请求 limiter；也可以在 handler 下传剩余请求 deadline 的 RequestOptions，但这只能解决 deadline，不能同时修复并发和预取配置。保持现有共享 CPU limiter，审查 Mosaic 包装的同类下传路径。测试自定义 timeout/并发、本地/default URL 一致性和 prefetch 透传。对应本地单一事实来源/API 契约和异步生命周期规范。

## F5 · P2：Mosaic 错误脱敏保留 HTTP URL 的用户名/密码

位置：[reader_pool.rs:40](/Users/zhang/code/async-geotiff-rs/src/mosaic/reader_pool.rs:40)、[reader_pool.rs:216](/Users/zhang/code/async-geotiff-rs/src/mosaic/reader_pool.rs:216)、[server.rs:955](/Users/zhang/code/async-geotiff-rs/examples/serve/server.rs:955)。

`redact_query` 仅截掉 `?` 及其后内容，错误标签仍保留 URL userinfo。服务器从管理员指定的本地 MosaicJSON 文件加载资产；瓦片接口没有要求调用者提供或已知源 URL，却把 `MosaicTiler` 错误原样返回。

合成探针将 `https://audit-user:audit-password@127.0.0.1/data.tif?token=audit-query` 配置为服务端资产。默认资产策略拒绝 loopback 后，库错误保留 `audit-password`、隐藏 `audit-query`。直接引用当前 `server.rs` 的公开 router，发送仅有坐标与 Q3 样式、没有源 URL 或 Authorization 的请求，实际 500 正文仍含合成密码。主审已重跑，未使用真实凭据，也未发起网络连接。

这是“服务端私有资产配置 → 瓦片客户端”的错误输出边界。实际泄露的前提是资产 URL 使用需保密的 userinfo、打开/重开失败、服务可被其他调用者访问。本例通过 AppState/默认库 policy 复现，不等同于完整 `serve` CLI 的 `demo_http_policy`；默认 CLI 监听 loopback，不能据此声称默认公网或生产已发生泄露。现有路由没有正常公开原始 MosaicJSON/asset URL 的端点。

建议 HTTP 错误标签按 URL 组件移除 userinfo、query、fragment，避免内层错误再次嵌入完整/规范化 URL；保持 Store backend 的对象 key 语义。补充纯 userinfo、userinfo+query、编码字符、规范化错误及非 URL key 回归。只隐藏 HTTP 正文而保留可记录的库错误不够。对应本地错误边界与敏感信息处理原则。

## N1 · P3：极大有限 F64 常量在三种核下变成透明

位置：[resampler.rs:148](/Users/zhang/code/async-geotiff-rs/src/warp/resampler.rs:148)。

Cubic/CubicSpline/Lanczos 直接累计 `k * value` 并除以权重和；中间或最终浮点舍入溢出后回退 fill。输入为 4×4 单波段 F64，全为 `f64::MAX`，同 CRS 单位像素，warp 到 1×1。正确常量结果仍是有限 MAX；实测三种核均返回 `mask=false`、render 后 `[0,0,0,0]`。Bilinear 对照为 `mask=true`、白色不透明。主审已重跑 public warp→render 链。

只影响极端 F64 边界，本轮不声称普通 U8/U16 数据受影响。此前 Average 的稳定累加修复没有覆盖这段核。建议稳定缩放/归一化且保留有符号权重、零权重和真实非有限结果的语义；补正负极值常量及有效性回归。对应数值正确性、`test-proptest-properties`。

## N2 · P3：外部 histogram 计数总和未检查溢出

位置：[stats.rs:30](/Users/zhang/code/async-geotiff-rs/src/io/stats.rs:30)、[stats.rs:38](/Users/zhang/code/async-geotiff-rs/src/io/stats.rs:38)。

公开 `parse_embedded_stats` 接受 `STATISTICS_HISTOBINS=18446744073709551615,1`，随后 `Histogram::quantile(50)` 在 debug 构建中因 u64 求和溢出 panic。两项计数分别可解析，合计却越界。独立探针及主审重跑均用 `catch_unwind` 捕获该 panic，程序正常退出。

当前没有确认生产瓦片路径调用 `quantile`；这是公开低层 API 对异常元数据的稳健性问题，不是已证明的远程瓦片进程崩溃。release 行为未执行验证。`Histogram` 还允许外部直接构造，只修改 parser 不足以修复所有入口；建议在 quantile 中使用不溢出的累计表示，或在可强制执行的构造边界校验并明确错误语义。覆盖空、零、大计数和正常百分位。对应 `err-result-over-panic`、`api-parse-dont-validate`。

## S1 · P3 规范项：公共 API 文档缺口

显式启用 `-W missing_docs` 得到 219 项诊断，包括 [WorkBudget](/Users/zhang/code/async-geotiff-rs/src/resource.rs:26)、[RequestLimiter::new](/Users/zhang/code/async-geotiff-rs/src/request.rs:36)、[RequestOptions 字段](/Users/zhang/code/async-geotiff-rs/src/request.rs:124)、[MosaicTilerBuilder::request_limiter](/Users/zhang/code/async-geotiff-rs/src/mosaic/mod.rs:145)。

这是 `doc-all-public` 及本地公共 API 文档要求的缺口，独立于上述行为缺陷。优先补资源预算、共享范围、clamp、取消、timeout 默认值及与全局默认实例的关系，再补字段/枚举文档；不需要为所有简单 getter 写冗长模板。

额外的 `clippy::undocumented_unsafe_blocks` 警告位于 resampler.rs:95，但 match arm 上方已有完整 `// SAFETY:` 与有限值/下标/layout 论证。它是注释位置不被 lint 识别，未判定为缺少安全保证或 UB。可在维护时将说明靠近 unsafe block。

## 排除与规范取舍

- AST `&String` 和 `&Vec<u64>` 分别是 Moka weigher、Option::filter 要求的回调签名，不是可以直接替换为 `&str`/slice 的公开 API 违规。
- block_cache::claim 在 await 前释放标准 MutexGuard；maybe_spawn_prefetch 的 await 位于未捕获该 guard 的子 future。没有证据支持锁跨 await 缺陷。
- quadkey helper 为 crate 内部可见，正式调用入口先验证瓦片坐标；不复报已修复的高层越界。
- 唯一 unsafe 的边界、布局与索引不变量经人工复核。没有用一条 AST/lint 命中推断内存不安全。
- 单元测试为源码内唯一外层 tests 模块；cog.rs 的多个场景是其内部子模块。未发现需再次搬迁的同源独立单测文件。
- 默认 CPU limiter 的分域及非请求 read_window 的预算限制已有明确运行文档，未将它们作为新缺陷。
- 不机械套用 panic=abort、额外集合依赖、native CPU、PGO、所有函数 must_use 或拆文件建议；这些需要具体收益及兼容性依据。
- 昨日报告中标记已修复的掩膜网格、Mercator 极区夹取、瓦片坐标、Average 和 STAC 项，本次未重复当成仍未修复的发现。

## 实际验证

本轮环境：macOS arm64，`cargo +stable` 对应 Rust 1.92.0。每组实际执行 `cargo +stable test --locked --all-targets <flags>` 及 `--doc`。

| Features | 全目标通过 | 失败 | 忽略 | 文档测试通过 |
| --- | ---: | ---: | ---: | ---: |
| 默认 | 363 | 0 | 2 | 5 |
| `--no-default-features` | 266 | 0 | 2 | 4 |
| `--no-default-features --features mosaic` | 353 | 0 | 2 | 5 |
| `--features stac,webp,jpeg` | 381 | 0 | 2 | 5 |
| `--all-features` | 382 | 0 | 2 | 5 |

每组 2 个忽略项需要大型本地影像。第一次沙箱运行出现回环监听 EPERM/系统代理不可用，已在允许本地监听的环境完整重跑，表中为成功重跑结果。没有将环境失败登记为库回归。

另通过：`cargo +stable fmt --all -- --check`；`cargo +stable clippy --locked --all-targets --all-features -- -D warnings`；三组 `node --test examples/*.test.cjs`。专门启用文档/unsafe 注释 lint 的结果单独如 S1，不与默认严格 Clippy 混称为“所有规范零警告”。

收尾复核：87 个基线文件的 SHA-256 均未变化；源码、既有测试、Cargo 与配置未被修改。

临时证据：`/tmp/async-geotiff-rust-review-20261023/` 中 `validation.json`、逐配置日志、AST 规则及 `scan.py`、lint JSON、基线文件 SHA-256 和探针日志。AST 排除 tests 后命中 panic/invariant 候选 86、task spawn 19、锁/await 2、借用容器 2、unsafe 1；计数不是缺陷数。

复现命令：

```sh
rtk proxy cargo +stable run --offline --locked --manifest-path /tmp/async-geotiff-render-probe/Cargo.toml --target-dir /Users/zhang/code/async-geotiff-rs/target
rtk proxy cargo +stable run --offline --locked --manifest-path /tmp/async-geotiff-io-probe/Cargo.toml --target-dir /Users/zhang/code/async-geotiff-rs/target
rtk proxy cargo +stable run --offline --locked --manifest-path /tmp/async-geotiff-review-20261023-server-probe/Cargo.toml --target-dir /Users/zhang/code/async-geotiff-rs/target --bin review-server-timeout
rtk proxy cargo +stable run --offline --locked --manifest-path /tmp/async-geotiff-review-20261023-server-probe/Cargo.toml --target-dir /Users/zhang/code/async-geotiff-rs/target --bin mosaic_userinfo
```

上述探针与日志暂存于 `/tmp`，不是新增的仓库测试；目录若被清理，命令将需要重建探针。server timeout 探针需允许本地回环监听，其余无需真实远程数据。

## 跟踪与局限

已在 Multica `async-geotiff` 项目读取全部 61 项既有问题并去重。历史 MAP-211 是网格元数据规范化，不是此次 CubicSpline 核被绕过；MAP-32 是预取测量缺口，不是此次配置未传递。已登记并回读验证以下 8 项，均为 todo、未指派执行者；行为项复用 bug/安全标签。未改变历史问题状态。

| 审查项 | Multica | 内容 |
| --- | --- | --- |
| F1 | MAP-261 | CubicSpline 快捷路径 |
| F2 | MAP-262 | RGBA 默认点查询 |
| F3 | MAP-263 | 极点 extent |
| F4 | MAP-264 | 服务配置未下传 |
| F5 | MAP-265 | URL userinfo 脱敏 |
| N1 | MAP-266 | F64 极值重采样 |
| N2 | MAP-267 | Histogram 计数溢出 |
| S1 | MAP-268 | 公共 API 文档 |

本轮未修复或提交代码，未发布/部署。未执行生产服务验证、浏览器视觉验收、长时间压力/性能测量、release 数值探针、跨平台测试或远端 CI；本地测试通过不能覆盖这些边界。没有声称“全库没有其它问题”。

## 2026-10-24：全部修复与验证

按后续“修复全部问题”的要求，F1–F5、N1–N2、S1 已在上述基线上修复。此前章节保留修复前的发现、行号和验证结果；本节记录提交前完成的修复与验证。没有变更 Cargo 依赖；发布和部署不在本次交付范围。

| 问题 / 跟踪 | 修复 | 正式回归证据 |
| --- | --- | --- |
| F1 / MAP-261 | [plan_tile](/Users/zhang/code/async-geotiff-rs/src/tiler.rs:215) 对 CubicSpline 禁用精确网格 passthrough，走正常卷积与 2px halo | 原 64px 网格，以及 U16 影像 64px 子瓦片对照 128px 完整邻域；比较 tile、warp、参考卷积的像素与 mask |
| F2 / MAP-262 | [point](/Users/zhang/code/async-geotiff-rs/src/io/point.rs:95) 与 read_window 共用 Alpha 提升判定，按输出波段验证与选择 | 透明/不透明、非末尾 Alpha、其它 ExtraSamples、重排/重复/越界 bidx；独立 mask 与 Alpha 相反，验证独立 mask 优先并保留原生波段 |
| F3 / MAP-263 | [极点检查](/Users/zhang/code/async-geotiff-rs/src/warp/projection.rs:213) 验证不同经线收敛及实际凸四边形包含，再扩至极点与完整经度 | EPSG:3413/3031、UPS32661/32761 偏移、旋转足迹、边/角极点；外部极点、CRS84 反例；公开 Tiler/CogReader bounds |
| F4 / MAP-264 | [请求配置](/Users/zhang/code/async-geotiff-rs/examples/serve/server.rs:256) 下传到默认、lazy、MosaicJSON、STAC；lazy reader 接收 prefetch，重新配置时失效旧缓存 | 6 种来源/绑定顺序各 65 请求；128/60s 配置下 31s 仍在等待，61s 全部 504 后回收 128 名额；实际 GET 和解码数量验证 prefetch 0/1 |
| F5 / MAP-265 | [HTTP 错误](/Users/zhang/code/async-geotiff-rs/src/mosaic/reader_pool.rs:36) 按组件移除 userinfo/query/fragment，不外泄任意内层错误；Store key 和资源错误类型保留 | 纯 userinfo、query、组合/fragment、编码和规范化 URL；匿名 HTTP 响应无合成秘密；默认 policy 拒绝后 listener 零连接 |
| N1 / MAP-266 | [卷积累计](/Users/zhang/code/async-geotiff-rs/src/warp/resampler.rs:544) 按需缩放并在舍入溢出时归一化重算，保留 signed support 与真实越界 | 三种核正负 MAX、掩膜、边缘、相消、公开 warp 有效性，以及真实 signed overshoot 仍无效；debug/release 均通过 |
| N2 / MAP-267 | [quantile](/Users/zhang/code/async-geotiff-rs/src/io/stats.rs:39) 用 u128 累计；100% 分位定位最后非空桶，避免浮点舍入丢掉末尾小计数 | 直接构造 4×MAX、公开 GDAL XML [MAX,1]、不对称计数和尾零桶；普通/空/退化情形；debug/release 均通过 |
| S1 / MAP-268 | 补齐公共文档并启用 [missing_docs](/Users/zhang/code/async-geotiff-rs/src/lib.rs:5)，说明预算/共享/取消等契约；修正旧 Rustdoc 链接/HTML 和 SAFETY 位置 | 严格 missing_docs/undocumented_unsafe_blocks 零警告，Rustdoc 构建及五组文档测试通过 |

N2 的 100% 端点问题在修复交叉复核中发现并一起处理：即使 u128 求和不再溢出，MAX 和 MAX+1 转 f64 仍可能相同，导致旧算法在前一桶提前返回；新增端点分支保留原有“最后非空桶右边界”语义。

三个分工完成修改后交换检查，主审复核生产差异与测试。未遗留已确认的阻断项。正式测试留在原源码唯一 tests 模块或镜像集成测试中，合法 Alpha TIFF 构造器放入 tests/support/fixtures.rs。中英文库使用、样式、运行配置文档已同步。

### 修复后的验证

环境仍为 macOS arm64、Rust stable 1.92.0；以下各组均执行 locked 全目标测试和文档测试。

| Features | 全目标通过 | 失败 | 忽略 | 文档测试通过 |
| --- | ---: | ---: | ---: | ---: |
| 默认 | 379 | 0 | 2 | 5 |
| --no-default-features | 275 | 0 | 2 | 4 |
| --no-default-features --features mosaic | 366 | 0 | 2 | 5 |
| --features stac,webp,jpeg | 398 | 0 | 2 | 5 |
| --all-features | 399 | 0 | 2 | 5 |

额外通过：

- cargo +stable fmt --all -- --check；git diff --check。
- cargo +stable clippy --locked --all-targets --all-features -- -D warnings -D missing_docs -D clippy::undocumented_unsafe_blocks。
- RUSTDOCFLAGS="-D warnings" cargo +stable doc --locked --all-features --no-deps。首轮发现 HttpAccessPolicy 文档指向私有 ReaderPool，修正一行文案后通过；该后续改动不涉及可执行代码。
- cargo +stable test --locked --release --lib convolution_：4 项。
- cargo +stable test --locked --release --lib io::stats::tests::quantile：6 项。
- node --test examples/*.test.cjs：3 项；Hugo 中英文构建通过。

每组的 2 个既有忽略项仍是需要本地 SVN3 COG 和 904 MiB JL1 影像的测试。未把这些项计为通过。

ast-grep 规则的正反例断言及复扫通过，排除 tests 后候选仍为 panic/invariant 86、spawn 19、锁/await 2、借用容器 2、unsafe 1，没有新增候选；这些不是缺陷计数。

GitNexus 按确切 checkout 使用已安装 CLI analyze --index-only --pdg 刷新；context 指向验证基线 HEAD。修复后的一次 detect_changes 返回 149 个变更符号、169 个受影响流程、44 个已跟踪变更文件，未标记 partial/truncated；risk_level=critical 表示共享基础路径的影响范围，已据此覆盖单源/Mosaic、掩膜、数值和配置调用链。分析器仍明确报告候选上限、流程遗漏及两处 CDG 缺失，因此图结果不是全路径安全证明。源码/执行证据仍是验收依据。

修复证据目录：/tmp/async-geotiff-rust-fixes-20261024/。包含 validation.json、逐配置日志、release 日志、AST 规则/结果、GitNexus 输出，以及验证前/最终 76 个源码、夹具、Cargo 输入指纹。矩阵后的 Rust 源码改动仅为上述 ssrf.rs 文档链接；最终指纹复核无漂移。

已向原问题回写 8 条修复说明，并回读确认 MAP-261–MAP-268 均为 in_review。用户随后授权提交并推送这些修复；交付提交与远端状态以 Git 记录为准。未执行跨平台、生产、长期负载、浏览器视觉验收或远端 CI；此次通过不代表不存在其它未覆盖的问题。
