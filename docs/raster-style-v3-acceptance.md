# Raster Style 0.4.0 / Q3 浏览器验收与性能回归检查

验收日期：2026-10-19。本机真实 Chromium 中 MapLibre、Leaflet、OpenLayers 均通过验收。发现并修复三个示例问题：Mosaic 自托管资产被默认 HTTP 策略拦截、Leaflet 缩放中切换数据集时定位被旧动画覆盖，以及三个查看器的窄屏溢出。

性能测试完成 **368,332 次正式成功 HTTP 请求**，配对负载的对应响应编码图像 SHA-256、尺寸和次数一致；持续压力逐请求核对预热响应哈希。整体持续压力吞吐与 P95 持平；更强控制的稳态复核未显著复现 D1 初测的局部退化信号。初测信号仍保留，不能把结果解释为零开销或生产容量保证。

## 对照与运行条件

- 升级前：`5d4f6554ddb0bd0998acf954f8924f8be502a6e2`，SDK 0.3.0、Q2。
- 升级后：`a820f1268f8d58e96fca893353d23334faf3ce02` 加本次示例修复。完整压测在窄屏 CSS 补修前完成，测量版与交付版的 Rust 源码一致，仅嵌入 HTML/CSS 不同；测量版与最终版的源码补丁和二进制哈希分别归档，最终二进制另做920次瓦片/样式哈希对照，与测量版 PNG/WebP 字节完全一致（该对照不另作性能结论）。渲染核心未追加推测性优化。
- macOS 15.6.1 / ARM64，10 逻辑 CPU；Rust 1.92.0，两版均为 release、fat LTO、codegen-units=1、`default + webp`。构建与浏览器负载不和压测重叠。
- 共享同一组现有本地文件，以 device/inode/size 核实；测量结束再次核对 size/mtime。没有重新下载数据。
- `ASYNC_GEOTIFF_BLOCK_CACHE_MB=256`，`MAX_TILE_TASKS=4`、`MAX_DECODE_TASKS=4`（对应完整前缀 `ASYNC_GEOTIFF_`，分别作用于 tiler/reader），`PREFETCH_RING=0`。显式 rescale、nearest 读取/重投影、PNG/256 的语义保持一致，只调整 Q2/Q3 传输。
- “冷请求”是新进程开始监听后的第一次瓦片请求，不含进程启动/编译；没有清空 OS 文件缓存。交替顺序与 ABBA/BAAB 用于减小桌面环境漂移。

| 数据集 | 文件 | 大小（十进制） | 像元类型 / CRS |
| --- | --- | ---: | --- |
| D0 | `SVN3-01_20250814_L2D0000203064_01_6112500050210001-FUS_cog.tif` | 9.03 GB | uint16 / EPSG:32651 |
| D1 | `SVN3-01_20250814_L2D0000203064_01_6112500050210001-FUS_3857_cog.tif` | 9.02 GB | uint16 / EPSG:3857 |
| D2 | `cog.qiw8xj3u9nvgmzxzrfzt04mr.tif` | 1.04 GB | uint8 / EPSG:3857 |
| D3 | `JL1GF04A_PMS_20230301124138_3857_cog.tif` | 0.95 GB | uint8 / EPSG:3857 |
| D4 | `JL1KF01B_PMSR5_20230517113518_3857_cog.tif` | 0.44 GB | uint8 / EPSG:3857 |

## 真实浏览器

Ego Chromium 154，最终桌面 viewport 1355×695、DPR=2（前轮为1343×683）；窄屏覆盖 390×844 和 320×844、DPR=1。三种查看器各验证初始 RGB、WebP/512、含 `+` 的表达式、七种拉伸（linear、none、minmax、percentile、stddev、histogram_equalization、curve）、窄屏、UInt16/UTM 数据切换、平移缩放和 Q3 扩展链接恢复。MapLibre 另验证 Mosaic 的 first 和 highest/after_channels；Leaflet 另验证在 zoomstart 时切换影像。最终合计 48 个有效渲染状态，以及三种查看器各一次非法 `extension_fields=null` 的可见错误提示。

记录检查实际瓦片响应、浏览器解码后的尺寸与非透明像元、原始 Q3 查询、页面异常和横向溢出，并人工查看截图。初次只查 body.scrollWidth 漏掉了长 select 控件（Leaflet 实测根宽604px，OpenLayers 同类控件也越界）及 MapLibre 状态栏 padding（根宽394px），最终验收改查 documentElement.scrollWidth 与每个可见控件的实际左右边界。未把网络完成视为绘制完成；OpenLayers 大影像截图等待 rendercomplete。该部分是功能验收，没有测浏览器 FPS。

| 查看器 | 桌面截图 | 窄屏截图 |
| --- | --- | --- |
| MapLibre | [RGB 世界影像](benchmarks/raster-style-v3-acceptance/screenshots/maplibre.png) | [390px](benchmarks/raster-style-v3-acceptance/screenshots/maplibre-390.png) / [320px](benchmarks/raster-style-v3-acceptance/screenshots/maplibre-320.png) |
| Leaflet | [缩放中切换后的 UInt16 影像](benchmarks/raster-style-v3-acceptance/screenshots/leaflet.png) | [390px](benchmarks/raster-style-v3-acceptance/screenshots/leaflet-390.png) / [320px](benchmarks/raster-style-v3-acceptance/screenshots/leaflet-320.png) |
| OpenLayers | [完成绘制的 UInt16 影像](benchmarks/raster-style-v3-acceptance/screenshots/openlayers.png) | [390px](benchmarks/raster-style-v3-acceptance/screenshots/openlayers-390.png) / [320px](benchmarks/raster-style-v3-acceptance/screenshots/openlayers-320.png) |

修复说明：

1. 示例 MosaicJSON/STAC 只显式允许 `local_http_base` 生成的自身 HTTP **hostname**，使配置中的 `/data` 资产可读；这是 hostname 例外，不是仅限同一端口的 origin 例外。其余主机仍用默认策略，库默认策略不变。旧基线也能复现原来的 500，故不把该问题归因于 SDK 升级。真实浏览器 [Mosaic 截图](benchmarks/raster-style-v3-acceptance/screenshots/mosaic.png) 与 STAC 定向回归均通过。
2. Leaflet 使用公开 zoomstart/zoomend 事件等待现有缩放完成，读取 JSON 后再次核对所选 dataset。回归同时覆盖旧响应过期及动画中切换，防止旧定位覆盖新选择。
3. Leaflet/OpenLayers 约束 flex field 与 input/select 的最大宽度并使用 border-box；MapLibre 的状态栏也使用 border-box，使其计算宽度包含 padding；窄屏工具栏右侧保留56px，避免覆盖地图导航按钮。没有用 overflow:hidden 掩盖问题，保留了控件交互与地图滚动。

## 配对负载与结果

五份数据各 6 组成对运行。每个进程依次执行首次单瓦片、64 次串行热瓦片、16 瓦片/并发6首次视口、相同热视口、32坐标×4/并发32突发，正式请求 13,500 次。下表为跨五份数据的 30 个配对耗时比的中位数，正数表示升级后更慢；各数据的原始值和置信区间在 `results.json`，不能由汇总中位数推断每份数据都改善。

| 场景 | 配对数 | 耗时变化 |
| --- | ---: | ---: |
| `cold_single` | 30 | -1.57% |
| `warm_single` | 30 | +0.37% |
| `viewport_first` | 30 | -3.81% |
| `viewport_warm` | 30 | -2.23% |
| `burst32` | 30 | +1.57% |

持续压力使用五份数据共40个坐标，32 条持续 HTTP/1.1 连接；每个版本两轮、每轮至少20秒，顺序 B→C / C→B。119,808 个正式请求全部成功且与预热响应哈希一致。吞吐平均 1492.3→1498.4 请求/秒（约 +0.42%），两轮 P95 平均 35.96→35.82 ms。RSS 为约0.5秒间隔采样峰值，非系统精确峰值。

| 轮次 | 版本 | 成功请求 | 请求/秒 | P95 ms | 采样峰值 RSS MiB |
| --- | --- | ---: | ---: | ---: | ---: |
| 1 | baseline | 29,324 | 1462.0 | 36.29 | 184.7 |
| 1 | current | 29,336 | 1465.3 | 36.43 | 168.0 |
| 2 | current | 30,665 | 1531.6 | 35.20 | 191.2 |
| 2 | baseline | 30,483 | 1522.5 | 35.64 | 188.0 |

统计拉伸/表达式/WebP 另在 D0 UInt16 和 D2 UInt8 上各做6组配对：每种先请求一次，再24次/并发4热请求，合计3,600次。参数显式使用 percentile=2,98、stddev=2、无损 WebP/512；表达式按两输入 rescale 之和设置范围。所有对应 PNG/WebP 编码 SHA-256 相同。以下为短热批次的描述性配对中位数，不能单独作为稳定退化判据。

| 样式 | D0 耗时变化 | D2 耗时变化 |
| --- | ---: | ---: |
| `minmax` | +2.37% | +10.92% |
| `percentile` | -6.95% | +3.73% |
| `stddev` | +2.81% | -1.94% |
| `histogram_equalization` | +4.49% | +0.69% |
| `expression-plus` | -1.61% | -3.42% |
| `webp-512` | -4.41% | +1.10% |

## 局部偏慢信号与复核

对短批次中波动较大的 D0/D1/D2 加长为10组成对运行：单瓦片512次串行、热视口512次/并发6、突发1024次/并发32，D2另加 minmax 512次/并发4，共133,120次正式请求。表中区间为对配对比重采样5000次的中位数 bootstrap 95%区间；桌面环境与小样本仍有限制。

| 数据集 | 场景 | 配对耗时变化 | 95%区间 |
| --- | --- | ---: | --- |
| D0 | `burst32_1024` | -0.20% | [-9.57%, +6.46%] |
| D0 | `viewport_warm_512` | +0.49% | [-4.91%, +5.53%] |
| D0 | `warm_single_512` | +1.26% | [-2.31%, +3.75%] |
| D1 | `burst32_1024` | +3.53% | [+0.77%, +18.42%] |
| D1 | `viewport_warm_512` | -5.21% | [-12.75%, +3.16%] |
| D1 | `warm_single_512` | -0.71% | [-8.38%, +1.37%] |
| D2 | `burst32_1024` | -7.40% | [-12.78%, +4.34%] |
| D2 | `minmax_512` | -6.94% | [-30.89%, +12.02%] |
| D2 | `viewport_warm_512` | -8.47% | [-19.60%, -1.95%] |
| D2 | `warm_single_512` | -4.09% | [-8.90%, +2.19%] |

**保留的信号：** D1 `burst32_1024` 中9/10配对变慢，中位 +3.53%，95%区间 [+0.77%, +18.42%]；不能直接归为噪声。D2 minmax 的短批次 +10.92% 在加长后为 -6.94%，没有相同方向的复现。

随后仅对 D1 做独立平衡的 ABBA/BAAB：6块、24个独立进程，每次先额外预热1024请求，再测4×1024请求，正式98,304次。逐块先平均同版的两次耗时，再算比值；中位变化 **+1.01%**，bootstrap 95% **[-1.76%, +3.25%]**。每4096请求服务 CPU 均值 **11.115→11.194 秒（+0.71%）**，客户端 CPU **0.877→0.900 秒（+2.65%）**。

这轮更强控制的**稳态**负载没有显著复现前述退化；它增加了预热与批次长度，不能据此推翻较短 burst 的原始信号，或宣称所有场景零开销。结合其他 D1 热负载和整体压力结果，没有足够依据改核心热点，因此没有添加缓存等推测性优化。

## 资源回归与最终检查

复用既有 `load_audit`，两版各运行128 samples、并发4/16。取消、deadline、冷 Mosaic 取消和内存预算拒绝场景均通过。

| 并发 | 基线进程峰值 RSS | 升级后进程峰值 RSS |
| --- | ---: | ---: |
| 4 | 27.22 MiB | 28.53 MiB |
| 16 | 40.98 MiB | 40.56 MiB |

这里的进程峰值由 wait4/ru_maxrss 获取，但使用的是64×64合成 Deflate fixture，不能代替大影像容量评估，也不能证明长期无泄漏。并发4时增加约1.31 MiB，应与真实压力的采样峰值一起看，不能描述为完全无内存开销。

最终检查：serve/all-features 21项、查看器 bootstrap 27场景（含新增竞态断言）、strict Clippy/all-targets/all-features、rustfmt、Hugo 中英文严格构建均通过。原升级的四轮独立对抗审查和第五轮复审已完成；本轮新增修复又由原最终审查 agent 独立复核，定向 STAC 与查看器测试通过。提交前 GitNexus detect_changes 完整返回36个变更符号（含原有文档脏改动），没有 partial/truncated；入口 main/STAC 的13条受影响流程使结果评级 HIGH，相关启动、HTTP资产、取消和统计路径已结合源码与上述检查复核。HTML 未索引部分结合调用搜索、浏览器和回归测试确认，图分析不替代实际测试。

## 证据与复现

[完整归档目录](benchmarks/raster-style-v3-acceptance/) 包含主测试、样式、加长、ABBA 的 JSON 结果与 gzip 原始记录，源码/二进制/fixture 哈希、浏览器记录及截图。[procedure.tar.gz](benchmarks/raster-style-v3-acceptance/procedure.tar.gz) 保存脚本、精确修复补丁、配置与复现说明；不包含大 COG 或二进制。`SHA256SUMS` 覆盖证据文件。首次工具中止（端口耗尽、参考键冲突及缺少显式样式参数）已修正，并与正式数据分开保存。

结论仅覆盖本机、这些 COG/坐标/参数、短时32并发 HTTP 负载和 Chromium 功能验收；未覆盖生产网络、其他浏览器、浏览器 FPS 或长期内存行为。之前的远端 GitHub Actions 因账户预算未启动，本机通过不等同于远端 CI/部署成功。原有 TiTiler 报告、数据和其他任务脏改动不纳入本次交付。
