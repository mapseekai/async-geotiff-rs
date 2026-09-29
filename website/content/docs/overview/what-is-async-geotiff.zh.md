---
title: 什么是 async-geotiff？
description: 项目目标、支持的数据类型以及有意保留的边界。
weight: 10
---

`async-geotiff` 是一个 Rust 库，用于把云优化 GeoTIFF（COG）转换为带样式的
XYZ 地图瓦片。它只读取生成目标瓦片所需的字节范围，解码原生 TIFF 块，按需
重投影，应用渲染样式，最后编码输出。

## 核心能力 {#capabilities}

- 通过 HTTPS 或调用方配置的 `object_store` 后端异步访问 COG。
- 在一个按字节限制的共享缓存中保存解码后的原生 TIFF 块。
- 支持 `WebMercatorQuad`（EPSG:3857）和 `WorldCRS84Quad`（EPSG:4326）。
- 支持 U8、U16、I16、U32、I32、F32 和 F64 源像元。
- 支持 nearest、bilinear、cubic、cubic-spline、Lanczos 和 average 重采样。
- 支持拉伸、色带、RGB 合成、颜色公式和算术波段计算。
- 默认输出 PNG，并可通过 feature 启用无损 WebP。
- 使用 MosaicJSON、STAC 或自定义数据源组合多资产镶嵌。

## 有意保留的边界 {#boundaries}

**不依赖 GDAL 运行时。** TIFF I/O 使用 `async-tiff` 和 `object_store`。
默认的 `proj` feature 仅在非内置快速路径的坐标转换中使用系统 `libproj`。

**库中不包含 Web 框架。** Axum HTTP 层以及 MapLibre、OpenLayers、Leaflet
查看器位于 `examples/`。应用可以直接使用 `CogReader`、`Tiler` 或
`MosaicTiler`，无需引入 Axum。

**不管理凭据。** 使用 S3、GCS、MinIO 或其他对象存储时，由应用完成认证配置，
再把可用的 store 传给库。

**不绑定可观测性后端。** crate 发出 `tracing` span 并公开缓存统计；应用自行选择
OpenTelemetry、Prometheus、日志或其他导出方式。

> [!NOTE]
> 完全位于数据集或投影有效范围之外的瓦片返回 `Ok(None)`；示例 HTTP 服务将其映射为
> `204 No Content`。
