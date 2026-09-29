---
title: 示例 HTTP API
description: 路由、瓦片路径、样式参数与响应行为。
weight: 10
---

HTTP API 属于 `examples/serve.rs`，不会进入库本身的依赖表面。

## 路由 {#routes}

| 方法与路径 | 用途 |
| --- | --- |
| `GET /tiles/{tms}/{z}/{x}/{y}.{ext}` | 从选中的单个 COG 渲染瓦片。 |
| `GET /mosaic/tiles/{tms}/{z}/{x}/{y}.{ext}` | 从配置的 mosaic 或 STAC 数据源渲染瓦片。 |
| `GET /datasets` | 列出 `data/` 下发现的 TIFF 文件。 |
| `GET /cache/stats` | 返回解码块缓存统计。 |
| `GET /formats` | 返回当前服务编译支持的输出格式。 |
| `GET /bounds` | 返回示例查看器使用的边界。 |
| `GET`、`HEAD /data/{name}` | 通过范围请求提供本地 TIFF。 |

支持的 TMS 名称是 `WebMercatorQuad` 和 `WorldCRS84Quad`。扩展名支持 `png`，
以及 feature 控制的无损 `webp`。

## Raster Style Q2 {#parameters}

两条瓦片路由只接受完整 Q2：`version=2.0&type=...`。未知字段、旧格式和未实现能力报错。
`dataset`、`mode`、`_v` 是独立请求上下文，不能重复；资产地址不属于样式。

| 参数 | 是否可重复 | 含义 |
| --- | --- | --- |
| `version` / `type` | 否 | `2.0` 与 gray/rgb/pseudocolor/categorized/single_color。 |
| `bidx` | 是 | 从 1 开始的源波段索引，RGB 恰好三个。 |
| `expression` | 否 | 灰度一个表达式；RGB 三个分号分隔的表达式；与 bidx 互斥。 |
| `method` / `rescale` | 范围可重复 | none、linear、minmax、percentile、stddev、histogram_equalization、curve；参数与共享统计网格见样式指南。 |
| `colormap_name` / `colormap` | 否 | 伪彩色色表；原生色表是精确值或半开区间。 |
| `nodata` | 否 | 有限数值或 nan。 |
| `color_formula` | 是 | 已支持 Gamma 操作，按顺序应用。 |
| `opacity` | 否 | 0–1，默认 1。 |
| `resampling` / `reproject` | 否 | nearest、bilinear、cubic、cubic_spline、lanczos、average。 |
| `tilesize` | 否 | 64、128、256（默认）、512、1024。 |
| `format` / `lossless` | 否 | png（默认）或 webp&lossless=true；format 必须与路径一致。 |
| `pixel_selection` / `stage` | 否 | mosaic 必填；first/highest/lowest/mean/median 与 before_channels/after_channels。 |
| `rank_channel` | 否 | highest/lowest 的排序通道位置，从 1 开始。 |

原始 Q2 中 `+` 必须编码为 `%2B`；不能先解码后直接拼接。样式语义与限制见[样式指南](../../guides/styling/)。

## 响应 {#responses}

- `200 OK` 返回编码后的图像字节。
- `204 No Content` 表示坐标有效，但完全位于数据集或投影有效范围之外。
- 无效样式值与格式错误的坐标会返回客户端错误，底层保留结构化库错误。

默认输出尺寸为 256×256，以兼容更多 XYZ 客户端。
