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
以及 feature 控制的 `webp`、`jpg`、`jpeg`。

## 样式参数 {#parameters}

| 参数 | 可重复 | 默认值或示例 | 含义 |
| --- | ---: | --- | --- |
| `bidx` | 是 | `bidx=1&bidx=2&bidx=3` | 从 1 开始的源波段选择。 |
| `expression` | 否 | `(b5-b4)/(b5+b4)` | 生成一个派生波段的算术表达式。 |
| `rescale` | 是 | `0,3000` | 显式逐波段 `min,max`，拉伸优先级最高。 |
| `stretch` | 否 | `minmax`、`percent`、`stddev` | 自动拉伸模式。 |
| `pc` | 否 | `2,98` | `stretch=percent` 使用的百分位裁剪。 |
| `sigma` | 否 | `2.0` | 标准差倍数。 |
| `nodata` | 否 | `nan`、`inf`、`-inf` 或浮点数 | 覆盖数据集 nodata。 |
| `colormap_name` | 否 | `viridis` | 内置单波段色带。 |
| `color_formula` | 否 | `gamma RGB 1.5` | 支持的 rio-color 操作序列。 |
| `resampling` | 否 | `nearest` | 源读取或缩放重采样核。 |
| `reproject` | 否 | `nearest` | 重投影阶段的重采样核。 |
| `tilesize` | 否 | `256` | 64、128、256、512 或 1024。 |
| `format` | 否 | `png` | 可选格式断言，必须与路径扩展名一致。 |

重采样核包括 `nearest`、`bilinear`、`cubic`、`cubic_spline`、`lanczos` 和
`average`。average 下采样时计算盒式均值，上采样时使用 nearest 行为。

镶嵌瓦片路由还接受 `pixel_selection=first`、`highest`、`lowest`、`mean` 或
`median`，默认值为 `first`。

## 响应 {#responses}

- `200 OK` 返回编码后的图像字节。
- `204 No Content` 表示坐标有效，但完全位于数据集或投影有效范围之外。
- 无效样式值与格式错误的坐标会返回客户端错误，底层保留结构化库错误。

默认输出尺寸为 256×256，以兼容更多 XYZ 客户端。
