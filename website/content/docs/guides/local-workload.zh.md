---
title: 本地负载测量与基准
description: 无需外部服务，复现有界延迟与峰值 RSS 测量。
weight: 50
---

这些工具从 Git 检出目录运行，支持 macOS 和 Linux；它们是开发工具，不会在应用
依赖本库时执行。先单独编译优化后的二进制，避免编译器内存混入测量：

```sh
cargo build --release --locked --example load_audit --features mosaic
python3 scripts/run_load_audit.py --samples 32 --concurrency 4
```

默认结果写入被 Git 忽略的 `target/load-audit/results.json`。可用 `--output`
设置其他路径，用 `--concurrency 1` 或 `8` 比较并发。采样限制为 4–128，
并发为 1–16；超过 120 秒会终止子进程。脚本只依赖 Python 标准库。

负载在临时 loopback HTTP 服务中提供 64×64 Deflate TIFF，带 32×32 overview、
三个波段、四个原生块和 nodata 角落，每个 GET 注入 2 ms 延迟。直接调用库的瓦片
API，输出 64×64 PNG；不需要凭据和公共数据，也不代表示例服务 HTTP 路由的端到端
性能。冷请求包含 reader／客户端／元数据，热请求复用 reader 和解码块；mosaic
使用三个独立 URL 的相同像素与 Mean reducer。吞吐包含批次屏障，不是开放到达率。

JSON 包含 nearest-rank P50/P95/P99、缓存计数、GET 数量、准备的响应字节、活跃
处理器、工作预算峰值，以及取消／deadline／准入拒绝／提前内存拒绝的观测。32 次
采样的 P99 是最大值，不能当成稳定的生产尾延迟。“响应字节”是服务端准备的数据，
不证明已送达取消的客户端；取消返回后仍可能有远端工作完成。

脚本记录 UTC、平台、CPU、Rust 版本、提交及工作区状态、二进制和 fixture 的
SHA256、CPU 时间及子进程的 `wait4 ru_maxrss`。macOS 返回字节，Linux 的 KiB
会换算为字节。RSS 是整个负载进程全程峰值，不是各场景峰值或进程树总量。

这些是单机有界合成测量，不证明生产容量、任意恶意编解码输入的内存上限或无限
请求流表现。fixture 可用 `python3 tests/fixtures/generate.py` 重建，Deflate
字节也受本地 zlib 版本影响。

## 重采样微基准 {#resampler}

```sh
cargo bench --locked --bench resampler_bench
```

六组场景比较连续／跨步内存布局下带／不带 mask 的 Bilinear 采样，以及带／不带
mask 的 Average 降采样，测的是核本身成本，不是 HTTP 延迟或跨 CRS 流水线吞吐。
Criterion 只作为开发依赖，不会成为应用使用本库时的依赖。
