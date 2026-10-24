---
title: 版本发布与 crates.io
description: async-geotiff 的打包范围、验证和发布步骤。
weight: 40
---

包名是 `async-geotiff`，Rust 导入名是 `async_geotiff`。0.13.0 为首次 crates.io
分发做准备；旧 GitHub Release 仍提供源码包。GitHub 仓库公开并不会自动发布 crate，
Cargo 会单独上传 `.crate` 归档。

## 打包范围 {#contents}

`Cargo.toml` 明确列出包含内容：核心源码、examples 及 HTML 查看器、tests 和小型
可重建 fixture、重采样基准、Cargo 清单、README、变更日志和 MIT 许可
全文。测试与 fixture 一起保留，因为单元测试和示例会嵌入 fixture 字节。
website、脚本、Agent 指令、本地影像和生成报告不进入注册表包。

`benches/resampler_bench.rs` 有六组有效的重采样核对照，因此保留。
`scripts/run_load_audit.py` 测量本地有界延迟和峰值 RSS，在 Git 中保留，默认结果
写入忽略的 `target/load-audit/`。历史审查与机器实测可在截至 `7170204` 的 Git
历史中查阅，不再放在当前源代码树中。复现见[本地负载测量](/zh/docs/guides/local-workload/)。

## 发布前验证 {#verify}

更新 `Cargo.toml`、`Cargo.lock`、变更日志和中英文网站。按 `AGENTS.md` 跑五组
feature 的全目标与文档测试，以及 viewer 和严格 Hugo 构建，然后检查实际注册表包：

```sh
cargo +stable package --list --locked
cargo +stable publish --dry-run --locked
```

归档位于 `target/package/`。提交前本地预览可加 `--allow-dirty`，正式发布不加。
docs.rs 构建关闭默认 feature，避免要求原生 PROJ，同时生成 mosaic、STAC 和编码器
文档；应用使用本库时仍有正常的默认 feature。

## 发布 {#publish}

验证后提交发布改动，推送与版本号一致的标签：

```sh
git tag v0.13.0
git push origin main v0.13.0
```

`Release` workflow 会校验标签、安装原生构建依赖、运行测试、验证归档，并把归档
附到 GitHub Release。成功后将同一提交的文档部署到 Cloudflare Pages。
单独推送标签不会上传 crates.io。

上传注册表需要已验证邮箱的 crates.io 账号和有权发布该 crate 的 token：

```sh
cargo login
cargo +stable publish --locked
```

也可以设置仓库 secret `CARGO_REGISTRY_TOKEN`，在 GitHub Actions 的 `Release`
workflow 选择对应的**版本标签**并勾选 `publish_to_crates_io`。它会重新验证再上传。
不要把 token 放进命令参数、日志或提交文件。

已发布版本不能覆盖，应在上传前核实归档和版本号。详见
[Cargo 发布指南](https://doc.rust-lang.org/cargo/reference/publishing.html)。

## 从 0.12 升级 {#migration}

Raster Style 保持 SDK 0.4.0／协议 3.0/Q3。自定义 STAC 客户端应把已构建客户端的
`with_http_client` 改成 `with_http_client_builder`，传入配置好的 reqwest 0.11
`ClientBuilder`；旧入口返回错误。认证与重定向契约见[镶嵌指南](/zh/docs/guides/mosaics/)。
此版本还修正独立 mask 网格、极区覆盖、Alpha 点查询、精确网格 CubicSpline、极端
有限值重采样、histogram 计数及示例服务配置传递。
