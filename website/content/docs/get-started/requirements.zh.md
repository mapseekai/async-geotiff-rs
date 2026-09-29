---
title: 环境要求
description: Rust、系统库和可选的文档工具链要求。
weight: 10
---

## 库与示例 {#library}

- Rust 1.92 或更高版本，与 `Cargo.toml` 保持一致。
- 默认 feature 集需要系统 `libproj` 9.x。
- 从仓库开发时需要 Git。

macOS：

```bash
brew install proj
```

Debian 或 Ubuntu：

```bash
sudo apt install libproj-dev libsqlite3-dev libtiff-dev clang
```

其他平台请通过受支持的包管理器安装 `libproj`，或关闭默认 features。项目目前没有
提供 Windows 专用安装命令。

如果只需要同 CRS 瓦片或内置的 EPSG:4326 ↔ EPSG:3857 快速路径，可以关闭默认
features：

```bash
cargo build --no-default-features
```

## 文档站 {#documentation}

`website/` 下的 OINK 站点需要 Go 1.27 或更高版本，以及 Hugo Extended 0.165.0
或更高版本。Hugo 的版本输出中必须包含 `extended`：

```bash
go version
hugo version
```

库本身不依赖 Hugo 或 Go。
