# Rust 四轮对抗审查

审查基线：`1b6e48e6536df7606300a60dbe0841bd9a2a3205`（2026-10-22）。本次是全库审查，问题不一定由最近的测试迁移引入。

依据：[本地 Rust Code Style](/Users/zhang/code/code-style/rust-code-style.md)，该文件最近提交为 `8fe5deb31042e07b1224f68632b2d4c3baedf873`，读取内容的 SHA-256 为 `f1fb3f772f16d96468beadc0e24f520404c9ad666062fccf8080e803cc30d934`。同时核对仓库 AGENTS、公开 API、运行约束及现有测试。

## 结论

确认三类非网络功能问题：掩膜分块处理、极区重投影覆盖、瓦片坐标校验。另有两项低优先级数值边界问题，以及测试组织与指定规范不一致。停止网络安全专项前取得的两项 STAC 证据单独保留，不继续扩展验证。

审查阶段只记录问题；随后按用户要求修复全部确认项，结果见下节。原有 Cloudflare 删除、性能报告和基准数据改动在修复阶段保持原样，随后按用户追加授权一并纳入提交，并清理引用已删除 demo 的部署配置。下文问题正文保留审查时的描述，修复状态以“修复结果”表为准；审查证据的行号对应基线提交，当前源码位置可能已变化。

## 修复结果（2026-10-22）

| 项目 | 已完成的修复 | 回归覆盖 |
| --- | --- | --- |
| F1 掩膜网格 | 数据与掩膜分别计算块坐标、预算及拼接；异构网格不复用数据预取环 | 三种数据/掩膜块大小组合、全图、子窗口和边界窗口 |
| F2 极区覆盖 | 不再把投影坐标夹取到瓦片世界范围；Mercator 按输出行精确投影；极点读窗结合真实源范围；面积采样只为 ±90° 边界使用有限域极限 | ±πR 源极区透明、覆盖 ±89.9° 的源极区有效；公开 Tiler 六种支持的核及公共 warp 全部七种核 |
| F3 瓦片坐标 | 根据实际 TMS 矩阵统一校验 z/x/y，在资产发现前拒绝越界，并保留完整原坐标 | 两种 TMS 的合法边界和非法 x/y/z；Tiler、Mosaic、MosaicJSON 入口 |
| N1 Average 数值 | 通常保留直接加权和精度，仅在溢出时按二的幂同步缩放和与权重；限制有限输入最终商的溢出 | ±F64 最大值、分数面积、混合符号抵消、U8/U16 半整数舍入、非有限输入 |
| N2 窗口算术 | 检查投影坐标、i64 转换和窗口端点加法，返回明确范围错误 | 极小像素尺度的公开 Tiler 调用；NaN、Inf 和超范围边界 |
| S1 错误 URL | 对 STAC 的发送、HTTP 状态和响应体错误移除 URL | 本地回环的 500、截断响应和连接失败；Display/Debug 不含虚构凭据 |
| S2 重定向 | 默认及自定义 builder 强制同源重定向；发送前拒绝跨源目标 | 初次搜索与分页的跨源 307 不触达目标；同源跳转保留自定义头 |
| 测试组织 | 三个独立单元测试文件内联，三个多测试模块文件合并为唯一 tests 模块；同步 AGENTS 和背压指南 | 保留既有用例与 feature gates；新增 fixture 可再生成，原 fixture 内容不变 |

**STAC 调用方迁移：**`StacSource::new` 用法不变。已构建的 `reqwest_stac::Client` 无法重新设置重定向策略，因此旧 `with_http_client` 已弃用并直接返回错误；自定义配置应迁移到 `with_http_client_builder`，把 headers、超时、代理等设置在 builder 上。README 和中英文 Mosaic 指南已同步。仓库外调用方必须迁移，不将此兼容性变化隐藏为纯内部修复。

修复后的独立复核额外发现并关闭了极区读窗裁切、面积核极点无效角点、Mode 窄 footprint 的最近邻中心、整数均值舍入和分数面积极值商溢出五个边界。没有扩展生产网络安全扫描；STAC 回归仅使用本地回环和虚构凭据。

### 修复验证

最终源代码完成五组 `cargo +stable test --locked --all-targets` 和对应 `--doc` 测试；不把前一版修复的通过结果代替最终代码验证。

| 最终源码配置 | 全目标测试通过 | 忽略 | 文档测试通过 |
| --- | ---: | ---: | ---: |
| 默认 | 363 | 2 | 5 |
| `--no-default-features` | 266 | 2 | 4 |
| `--no-default-features --features mosaic` | 353 | 2 | 5 |
| `--features stac,webp,jpeg` | 381 | 2 | 5 |
| `--all-features` | 382 | 2 | 5 |

所有配置均为零失败；每组 2 项忽略均为需要大型本地影像的既有测试。另已通过：

- `rtk proxy cargo +stable clippy --locked --workspace --all-targets --all-features -- -D warnings`
- `rtk proxy cargo +stable fmt --all -- --check`
- `rtk proxy git diff --check`
- `rtk proxy node --test examples/*.test.cjs`（提交前补跑，三组全部通过）

I/O、Mosaic、STAC 和渲染/数值两个独立复核均为 READY。渲染复核额外以直接 rustc 探针检查 39,601 组分数面积 F64 最大值，以及所有采样核在纬度 91–92° 的无效域行为。源码指纹与最终工作区一致。

本地完整日志、配置命令、范围清单和复核记录位于 `/tmp/async-geotiff-fixes-20261022/`，最终测试结果在 `final-validation.json`。GitNexus 以当前工作区重新索引，`detect_changes(scope=all)` 完成；该结果包括原有无关改动，人工复核按 32 个授权路径限定范围。调用解析、流程截断和部分 CDG 缺失仍是图分析限制，不能据此声称完整路径证明。

修复验证结束时，原有 31 个无关路径的内容、模式和删除状态与修复前快照一致，暂存区为空，HEAD 为 `1b6e48e6536df7606300a60dbe0841bd9a2a3205`；这是追加“全部提交、公开仓库并重跑 CI”授权之前的快照。`CLAUDE.md` 是可读的相对链接 `AGENTS.md`，Git 模式为 `120000`。

以上修复验证不包含生产网络、浏览器视觉、release 配置、长时间压测、专项性能对比或远端 CI；后续远端结果以对应提交的 GitHub Actions 运行记录为准。Multica 连接不可用，修复证据保留在本地报告，未同步外部工单。

### 公开仓库后的 CI 补充

全部工作区变更提交为 `b51a1cc` 后，首轮 Linux CI 使用 Rust 1.99.1，在 `load_audit` 示例的 async 类型布局检查中超过默认递归深度，导致 `cargo check --all-targets` 失败。[失败运行](https://github.com/mapseekai/async-geotiff-rs/actions/runs/39792981110)。本机 `stable` 为 Rust 1.92.0，因此前述本地通过不能覆盖此编译器差异。

修复仅为该示例设置 `recursion_limit = "256"`，保持运行逻辑与最低 Rust 版本不变；本机 Rust 1.92 的对应 mosaic 配置检查通过。Rust 1.99 的三处 `fetch_update` 弃用提示保留，以兼容当前 1.92 最低版本。后续完整矩阵与文档构建结果见修复提交的 Actions 记录。

随后 `3bf97af` 的全 feature 编译暴露出 `tests/tiler.rs` 和 `tests/style/statistics.rs` 两个独立测试 crate 的相同深度限制；其他四组配置已通过测试。[全 feature 日志](https://github.com/mapseekai/async-geotiff-rs/actions/runs/39795008951/job/119880021573)。为这两个目标补充相同编译设置，保留全部测试逻辑。运行中还观察到 Ubuntu Azure 软件源的大包下载中断和长时间重试，不能将这种环境耗时计为源码失败。

补齐设置后，本机使用与 CI 相同的 Rust 1.99.1 执行 `cargo +1.99.1 check --locked --all-targets --all-features`、`test --locked --all-targets --all-features` 和 `test --locked --doc --all-features`，均通过：382 项全目标测试、5 项文档测试，零失败，2 项既有大型影像测试忽略。日志保存在上述临时目录的 `rust-1.99.1-*.log` 中。


## 审查过程与覆盖

| 轮次 | 方法 | 结果 |
| --- | --- | --- |
| 1：全库初审 | 按 I/O、异步资源、渲染/几何、Mosaic、服务入口分域检查，并运行严格 Clippy | 建立候选问题、调用链、规范对应项 |
| 2：交叉反证 | 交换候选，检查已有防护、文档例外、公开入口可达性和严重程度 | 极区问题补充真实 TIFF → Tiler → PNG 复现；排除文档明确允许的运行约束 |
| 3：实际复现 | 主审重跑隔离的像素、坐标和掩膜探针；保留匹配布局对照 | 复现下列 F1–F3、N1；N2 由独立审查探针验证 |
| 4：报告复核 | 独立复查证据、反例、误报、重复项及修复建议 | 确认问题保留；修正多边形累加行号，并明确不同 TMS 的复现结果 |

范围覆盖 `src/` 的 49 个 Rust 文件、Rust examples、tests、benches，以及 Cargo/CI 和相关运行文档。65 个 Rust 文件与固定提交快照逐字节一致。此处“全库”指按模块和风险覆盖，不代表逐路径形式化证明；浏览器、非 Rust 前端和外部服务未作完整验收。

## 确认的非网络功能问题

### F1 · P2：掩膜复用数据块坐标，会把无效像素标成有效

位置：[cog.rs:794](/Users/zhang/code/async-geotiff-rs/src/io/cog.rs:794)、[cog.rs:829](/Users/zhang/code/async-geotiff-rs/src/io/cog.rs:829)、[window.rs:441](/Users/zhang/code/async-geotiff-rs/src/io/window.rs:441)。

`classify_ifds` 按整幅图像尺寸匹配内部掩膜；`read_window` 却用数据 IFD 的块坐标获取、拼接掩膜，没有按掩膜自身的 tile width/height 重新计算。未填充的掩膜输出初始为 `true`。

复现使用已被 reader 接受的 32×32 TIFF、值为 200 的数据和全零 1-bit 内部掩膜：

| 数据块 / 掩膜块 | 读取列 0 | 读取列 20 |
| --- | --- | --- |
| 16×16 / 16×16（对照） | `valid=false` | `valid=false` |
| 32×32 / 16×16 | `valid=false` | **`valid=true`** |
| 16×16 / 32×32 | `valid=false` | **compressed block ranges 错误** |

影响是掩膜语义错误或可读窗口失败；没有声称这些布局符合所有 COG profile。即使项目决定不支持异构分块，也应在打开时明确拒绝，不能成功返回与全零掩膜相反的有效性。现有 alpha/mask 用例未覆盖数据与掩膜分块不同的情况。

建议分别计算掩膜块范围、预算及拼接坐标，或在入口拒绝不支持的布局。仅把默认掩膜改成 `false` 仍会漏读真实有效的掩膜块。对应规范：API 约束、正确性及重要边界测试。

### F2 · P2：Mercator 裁剪把数据边缘延伸到极区

位置：[mercator.rs:41](/Users/zhang/code/async-geotiff-rs/src/warp/mercator.rs:41)、[projection.rs:129](/Users/zhang/code/async-geotiff-rs/src/warp/projection.rs:129)。

EPSG:4326 → EPSG:3857 的快捷转换将纬度裁剪到约 ±85.0511°。当目标为覆盖 ±90° 的 `WorldCRS84Quad` 时，源范围外的极区坐标因此映射到源数据边缘，后续采样把它们判为有效。

两种独立复现均成立：低层 `warp`，以及正常的 TIFF → `CogReader` → `Tiler::tile` → PNG。输入为 EPSG:3857、范围恰好 ±πR 的 64×64 常量 200 TIFF。输出 `WorldCRS84Quad/0/0/0` 时，纬度约 +89.6484° 和 −89.6484° 的像素都是 `[200,200,200,255]`，应位于源影像覆盖范围之外。

现有裁剪单元测试说明该快捷路径有意裁剪，但不能证明渲染覆盖正确。启用 `proj` 仍优先使用这条快捷转换，不能消除问题。建议区分坐标投影与瓦片世界范围裁剪，让真实源范围之外的目标像素保持无效，并新增公开 Tiler 的极区透明性回归测试。对应规范：优化不得破坏正确性/API 契约。

### F3 · P2：瓦片 z/x/y 校验不完整且不同入口行为不一致

位置：[tms.rs:114](/Users/zhang/code/async-geotiff-rs/src/tms.rs:114)、[mosaicjson.rs:277](/Users/zhang/code/async-geotiff-rs/src/mosaic/mosaicjson.rs:277)、[quadkey.rs:8](/Users/zhang/code/async-geotiff-rs/src/mosaic/quadkey.rs:8)。

`Tms::tile_bbox` 校验高度方向，却未校验 `x < matrix_width`。例如 WebMercatorQuad 的 `z=0,x=2,y=0` 返回移出世界范围的 bbox；WorldCRS84Quad 同样接受超界列。使用 F2 的 Mercator TIFF 调用公开 `Tiler::tile`：WebMercatorQuad 返回 `Ok(None)`，WorldCRS84Quad 返回 `Ok(Some(PNG))`，均不是 `TileOutOfBounds`。此处没有据此声称错误 PNG 一定包含可见像素。没有发现规范化环绕的契约或实现。

MosaicJSON 又在校验之前做 quadkey 查找：`z=1,x=2,y=0` 会命中合法 `qk=0` 的资产；`z=33` 在 debug 探针中触发工作线程移位溢出。该 panic 被 JoinError 捕获并转换为错误，**不是进程崩溃**。没有验证 release 的该边界行为。

建议在公开瓦片/资产发现入口统一验证完整坐标，再进入查找、I/O 和投影；错误保留原始 z/x/y。测试应覆盖每个 TMS 的有效边界、超界 x/y 和超界 z。对应规范：类型/API 约束、可恢复输入使用 Result、同类接口错误语义一致。

## 低优先级数值边界

### N1 · P3：有限 F64 的 Average 中间求和溢出

位置：[resampler.rs:278](/Users/zhang/code/async-geotiff-rs/src/warp/resampler.rs:278)、[resampler.rs:395](/Users/zhang/code/async-geotiff-rs/src/warp/resampler.rs:395)。

矩形及多边形平均先累计未经缩放的加权和。2×2、每个值为 `1e308` 的 F64 图像缩成 1×1 时，数学均值仍是有限 `1e308`，实际返回 `0, valid=false`。这是中间值溢出，被后续非有限值处理转成 nodata。

主审已复现；普通地理数据数值范围通常远小于这个量级，故不与 F1/F2 同级。建议采用稳定的加权平均并覆盖常量及混合符号的大有限值。

### N2 · P3：极小像素尺度触发窗口加法溢出

位置：[bounds.rs:103](/Users/zhang/code/async-geotiff-rs/src/bounds.rs:103)、[tiler.rs:608](/Users/zhang/code/async-geotiff-rs/src/tiler.rs:608)。

将小型 fixture 的像素尺度设为 `1e-18`、原点设为 0 后，reader 接受元数据；公开 `Tiler::tile` 的窗口裁剪执行未经检查的 `col + w as i64`，debug 工作线程 panic，最终返回 `AgError::Warp`。该结果由独立审查探针验证，主审核对了元数据入口和算术路径；不是进程崩溃，也不是常规尺度下的性能问题。

建议对投影后的窗口转换及整数加法使用可检查的算术，返回明确的范围/元数据错误。未验证此用例的 release 行为。

## 测试组织规范

[指定规范第 158 行](/Users/zhang/code/code-style/rust-code-style.md:158)要求：每个源码文件的单元测试位于唯一的内联 `#[cfg(test)] mod tests`。

当前有三处多模块组织：

- [snapshot.rs:292](/Users/zhang/code/async-geotiff-rs/src/io/snapshot.rs:292)：`tests`、`http_tests`。
- [ssrf.rs:212](/Users/zhang/code/async-geotiff-rs/src/mosaic/ssrf.rs:212)：`tests`、`audit_tests`、`dns_limit_tests`。
- [reader_pool.rs:226](/Users/zhang/code/async-geotiff-rs/src/mosaic/reader_pool.rs:226)：五个独立测试模块。

这些场景可放入一个 `mod tests`，必要时在内部再分组，保留全部用例和 feature gates。

另有**规范冲突**：当前 AGENTS 允许相邻 `<source>_test.rs`，而本次指定的本地 Rust 规范要求内联。因此 `resource_test.rs`、`mosaic/stac_test.rs`、`warp/reproject_test.rs` 按仓库指南合规，按本次指定规范则需要内联。这不是运行时缺陷，也不能表述为此前重命名丢失了测试；后续统一时应同时更新指南和组织方式。

## 停止网络专项前已确认的证据

审查阶段按用户指示停止网络专项扩展；以下保留当时本地回环、虚构 token 的证据。后续仅修复 S1/S2 并补充本地回归，没有访问生产端点或使用真实凭据。

- **S1 · P1，有 URL 凭据时成立**：[stac.rs:294](/Users/zhang/code/async-geotiff-rs/src/mosaic/stac.rs:294)将 reqwest 错误直接格式化进 `AgError::Mosaic`。配置 URL 的 `api_key` 查询参数在上游 500 时出现在错误字符串中，[server.rs:955](/Users/zhang/code/async-geotiff-rs/examples/serve/server.rs:955)将该字符串返回给瓦片调用者。仅使用请求头认证时不触发这个 URL 泄露场景。建议统一移除传输错误中的完整 URL，保留安全上下文。
- **S2 · P2，需上游重定向**：[stac.rs:34](/Users/zhang/code/async-geotiff-rs/src/mosaic/stac.rs:34)的默认客户端允许自动重定向；分页只验证声明的 next href，同源 `/page2` 仍可通过 307 跳转到另一个 origin。默认构造器及文档推荐的自定义默认头客户端均已验证；后者的 `x-api-key` 会被送到跨源目标。标准 Authorization 头不能等同于任意自定义认证头。应在发送前限制重定向目标，而不是只检查最终响应 URL。此处不声称普通瓦片请求者可以任意指定 STAC 服务器。

## 排除或限定的候选

- 预取预算初版探针通过直接 `CogReader::read_window` 启动后台工作；[背压文档](/Users/zhang/code/async-geotiff-rs/docs/concurrency-backpressure.md:9)明确不保证与此类非请求入口竞争时自动恢复。当前证据不足以认定公开请求背压契约被破坏，未计入确认缺陷。
- 默认 COG、懒加载数据集、Mosaic 使用不同 CPU limiter 的行为已在运行配置文档说明，没有认定为单一全局上限失效。
- U16 RGB 值 400 正确饱和到 255；没有保留颜色整数回绕的初始怀疑。
- 资源计账不是 RSS 上限；没有将缺乏长时间压测泛化为内存泄漏。

## 原审查阶段的验证记录与局限

审查阶段执行通过：

- `rtk proxy cargo +stable clippy --locked --workspace --all-targets --all-features -- -D warnings`
- `rtk proxy cargo +stable fmt --all -- --check`
- 隔离掩膜、低层 warp/坐标、公开 Tiler 探针，均依赖固定提交快照。探针证明的是问题可复现，不是回归测试通过。

上一任务在相同 Rust 源码上已通过五种 feature 配置的全目标测试和文档测试，以及三组 viewer 启动测试。审查阶段没有重复整套矩阵，也不把这些已有测试通过当作边界问题不存在的证据。

本地复现材料位于 `/tmp/async-geotiff-rust-review-1b6e48e/`：

```sh
rtk proxy env CARGO_TARGET_DIR=/Users/zhang/code/async-geotiff-rs/target cargo +stable run --offline --locked --manifest-path /tmp/async-geotiff-rust-review-1b6e48e/io_mask_probe/Cargo.toml
rtk proxy env CARGO_TARGET_DIR=/Users/zhang/code/async-geotiff-rs/target cargo +stable run --offline --locked --manifest-path /tmp/async-geotiff-rust-review-1b6e48e/render_probe/Cargo.toml --bin render-review-probe
rtk proxy env CARGO_TARGET_DIR=/Users/zhang/code/async-geotiff-rs/target cargo +stable run --offline --locked --manifest-path /tmp/async-geotiff-rust-review-1b6e48e/mosaic_probe/Cargo.toml --bin render_challenge
```

这些是本机临时路径；复现输入、观察值和代码锚点已记录在本文。GitNexus 用已安装 CLI 执行 `analyze --index-only --pdg`，本地索引已刷新到基线；部分 MCP 连接仍返回旧 freshness 信息，因此补用 CLI。分析器还报告 Rust 调用解析和流程数量截断，以及两处 CDG 缺失。图中的空结果不能视为安全或完整性证明，结论以当前源码和可执行反例为准。

Multica 项目查询在沙箱内外均无法连接，未能核对/登记问题；本文保留证据待后续同步。未作浏览器视觉验收、生产网络验证、长时间压测或远端 CI 验证。
