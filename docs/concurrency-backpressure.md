# 并发工作内存背压

## 问题与修复

多个瓦片同时为读取、解码、拼接、重投影和渲染预留工作内存时，单个任务能完成，但合计可能超过共享的 512 MiB 预算。原来的 `try_reserve` 立即返回 `ResourceLimit`，HTTP 层映射为 503。

保留 `try_reserve` 的非阻塞语义和原始内存上限，在 `WorkBudget::run_request` 增加请求级调度：正常请求共享准入并行执行；仅遇到 `ResourceLimit` 时，先完整退出失败的一轮，释放该轮临时资源，再异步排队取得独占准入，最多重试一次。单独执行仍超预算的请求继续返回错误，不无限重试。

调度只包在 `Tiler::tile_with_options`、`Tiler::warp_tile_with_options` 和 `MosaicTiler::tile_with_options` 的公开请求边界。Mosaic 内部的多资产读取/warp 不重复获取调度锁，避免嵌套锁死。共享相同 `WorkBudget` 的公开请求共享调度；直接调用底层 `CogReader` 的读取仍然 fail-fast，不承诺与这些非请求入口的额外内存竞争自动恢复。

`RequestContext` 在任务作用域内保留准入许可，解码/拼接/渲染的阻塞任务捕获同一许可。因此，取消请求不提前释放仍在运行任务的许可。排队和重试都计入原有请求 deadline，取消或超时移除排队请求。请求数量上限保持不变（默认 64），没有引入无界等待队列。

没有修改 TIFF 兼容性、IO/解码实现、PNG 编码、像素结果或数据文件；没有提高工作预算或缓存预算。高压力时可能以排队延迟换取完整成功的瓦片，不能用旧版本快速失败的响应时间当作成功加载速度。

## 回归测试

资源预算单元测试集中在 `src/resource_test.rs`，其中 6 个背压用例验证多阶段竞争、正常并行、有界重试、非资源错误、deadline、显式取消和阻塞任务退出后的许可释放。3 个公开 API 用例分别归属 `tests/tiler.rs` 和 `tests/mosaic/mod.rs`，验证有限预算下的单图、warp、多资产 Mosaic 并发及结果一致性。

```sh
cargo test --locked --all-features
cargo test --locked --all-features --examples
cargo fmt --all -- --check
```

2026-09-29：上述两组测试共 404 项通过、2 项忽略。真实文件 HTTP 对照的 1152 次请求，修复前 438 次 503，修复后 1152 次全部成功，PNG 有效且可见；与修复前有效基准逐次比对，1152 次像素一致。工作预算计账峰值约 510.03 MiB，低于 512 MiB。该计账不等于进程 RSS；块缓存和调用方持有的完成输出另计。

原 Chromium 场景（beach、z18、16 瓦片、6 并发、浏览器缓存关闭）复跑 3 对：96 次全部成功且可见，首次中位数 51.0 ms，二次 21.0 ms。此时间包括请求与图片解码，不包括地图 GPU 绘制。

本地原始记录与复跑脚本位于 `../titiler-vs-async-geotiff-bench/concurrency-fix-20260929/`。`before.jsonl`、`after.jsonl` 是 HTTP 明细，`browser-fixed/raw.jsonl` 是浏览器明细。首次指新渲染进程，未清空 macOS 文件缓存。6/16 并发场景每轮 16 个请求；32 并发场景重复同一组瓦片，每轮 32 个请求。以上是客户端并发上限，不是生产容量结论。

严格全目标 Clippy 的当前检查被 8 处原有 `field_reassign_with_default` 测试风格告警阻塞；已核对相关语句存在于修复前 HEAD，本次不修改无关测试。
