---
title: 样式与波段计算
description: 使用 Raster Style 3.0 JSON 与 Q3 输入配置栅格样式。
weight: 10
---

样式输入使用 `raster-style = "=0.4.0"` SDK，协议版本为 `3.0`，需要 Rust 1.92。
SDK 负责结构和语义校验，适配器再检查本渲染器支持的能力。未知字段、重复单值参数和
未实现能力均报错。下列能力适用于 `Tiler::tile` 和 `MosaicTiler::tile`，不代表完整规范的后端认证。

## Rust 输入 {#recipes}

```rust
use async_geotiff::style::TileStyle;

let style = TileStyle::from_json(r#"{
  "version": "3.0",
  "extensions": ["https://mapseekai.github.io/raster-style/extensions/raster/tile/v1.0.0/schema.json"],
  "renderer": {"type": "pseudocolor", "bidx": [1], "colormap_name": "viridis"},
  "stretch": {"method": "linear", "rescale": [[0, 3000]]},
  "raster:tile": {"format": "png", "tilesize": 256}
}"#)?;
```

也可以使用 `TileStyle::from_raster_query` 解析原始 Q3 查询字符串，或用
`TileStyle::from_raster_style(&RasterStyle)` 接收 SDK 已校验对象。
`RasterStyle` 从 `async_geotiff::style` 导出。
`style.raster_style_json()` 返回规范化 JSON。修改样式时重新解析输入；直接覆盖
`TileStyle` 不暴露可变字段；`format()` / `tilesize()` 提供只读输出配置。

Tile 与 Mosaic 是独立版本扩展：在 `extensions` 声明精确的 Schema URL，
配置位于 `raster:tile`、`raster:mosaic`。Q3 使用 `extensions` 传输 URL 数组、
`extension_fields` 传输业务字段对象，均为百分号编码的 JSON。
推荐用 `raster_style::encode_query` 编码已校验的 SDK 文档。
校验不完整或声明第三方扩展的样式在执行前即被拒绝。

## HTTP Q3 输入

单波段色带：

```text
/tiles/WebMercatorQuad/0/0/0.png?version=3.0&type=pseudocolor&bidx=1&method=linear&rescale=0,3000&colormap_name=viridis
```

RGB 支持将一组范围广播到三个输出：

```text
version=3.0&type=rgb&bidx=1&bidx=2&bidx=3&method=linear&rescale=0,3000
```

`dataset`、`mode`、`_v` 是示例服务上下文，不属于样式文档；TMS 和瓦片坐标仍在路径中。
服务保留原始查询片段，由 SDK 解码一次。`+` 必须写成 `%2B`，空格写成 `%20`，
不要先 URL 解码再调用 `from_raster_query`。JSON 入口是 Rust API；示例 HTTP 服务使用 Q3 GET。
PNG 是默认编码。WebP 要启用 `webp` feature，声明 Tile 1.0.0 扩展，并配置 `raster:tile: {format: "webp", lossless: true}`，
并使用 `.webp` 路径；路径与样式格式必须一致。

## 拉伸与颜色 {#stretch}

- 支持 `gray`、`rgb`、`pseudocolor`、`categorized`、`single_color`。
- `method=none`（含省略）对显示路径固定使用 `[0,255]`，不会自动读取统计值。
- `method=linear` 必须给出 `rescale`；RGB 接受一组广播范围或三组逐通道范围。
- 显示路径支持 `range_policy=clamp|transparent`；透明策略在颜色处理前判断越界。
- 命名色带使用项目固定依赖中的 `colorous` 256 项表：viridis、magma、inferno、plasma、cividis、turbo、greys。
- 原生 `colormap` 精确值对象按数据值匹配；区间数组使用半开区间 `[lo,hi)`，末区间也不包含上界。
  pseudocolor 在显式拉伸或 Gamma 后转为 0–255 色表索引；categorized 始终按数据值精确匹配。
- `gray` 支持 `renderer_invert`；`single_color` 的有效输入包括 0 和负数。
- `opacity` 作用于输出 Alpha；最终 Alpha 为 0 时输出透明黑色。

`categorized` 只支持显式精确值表与 nearest，读取原始层，避免未经验证的概览改变类别。
普通数值样式支持现有 nearest、bilinear、cubic、cubic_spline、lanczos、average 核。
每个依赖波段分别重采样并传播有效性；源 Alpha 波段暂不支持，会在读取像元前报错。

`cubic_spline` 在整数像素中心仍会平滑数据。因此，请求瓦片网格与源网格精确对齐时
也会执行该核，直接瓦片渲染与 warp 路径保持相同重采样语义。Cubic、CubicSpline
和 Lanczos 对接近数值上限的有限 F64 常量也保持有效，不会因中间计算溢出而变透明。
实际非有限样本或无法表示的结果仍然无效。

## 统计拉伸与曲线

支持全部 7 种方法：`none`、`linear`、`minmax`、`percentile`、`stddev`、
`histogram_equalization`、`curve`。三个示例页均提供对应参数：

```text
method=percentile&percentiles=2,98
method=stddev&stddev=2
method=histogram_equalization
method=curve&curves=[[[0,0],[128,0.5],[255,1]]]
```

百分位对排序样本做线性插值；标准差为总体标准差；直方图均衡使用经验 CDF，不使用分桶近似。
曲线点为 `[数据值,归一化输出]`，支持一条广播曲线或三条 RGB 曲线。实际 Q3 URL 中需编码 JSON。

`Tiler` 与 `MosaicTiler` 在完整数据源足迹或固定 mosaic 源集合的联合足迹上，绑定
**1024 × 1024、EPSG:4326** 的共享统计网格。网格北向上，经纬度像元中心等距，覆盖及有效性
mask 从源继承；overview 根据各源完整范围和固定网格尺寸选择，与输出瓦片缩放级别无关。
统计网格重放相同的重采样、NoData、表达式、完整元组有效性与 mosaic 阶段。
不会改成当前瓦片统计，也不直接信任嵌入式 GDAL 汇总值。

省略 statistics 时使用 dataset/mosaic 范围、sample 精度、最多 1,000,000 个行优先等距网格位置。
`statistics={"scope":"dataset","accuracy":"exact"}` 遍历**该绑定网格**所有像元，
不表示遍历源数据原生分辨率的全部像元。无效采样丢弃且不补采；空统计输出透明；常量范围映射到 0.5。

统计结果跨瓦片共享，缓存绑定 reader 实例、顺序及完整规范化样式。统计计算、源读取和 median 保留输入
受现有 CPU/内存预算约束；选定 overview 的完整读取超过预算时明确报错。最多缓存两个统计计划。

`quadkey_zoom=0` 的 MosaicJSON 提供固定完整源集合。分区 MosaicJSON 的统计拉伸暂报错，
避免摊平索引后丢失覆盖范围和分区绘制顺序；瓦片始终使用索引选中的源。
自定义 `MosaicSource` 的 `statistics_assets()` 必须提供与瓦片选择语义一致的完整合成集合。
未绑定集合的动态 STAC、viewport 和外部 `statistics.ref` 暂报错。
低层 `render(ImageData, ...)` 不能自行推断完整数据范围；统计类方法通过 tiler 绑定后执行。

## 表达式 {#expressions}

支持十进制数、括号、一元负号和四则运算，波段编号从 1 开始。RGB 可用 `;` 分隔三个输出。

```text
version=3.0&type=gray&expression=(b5-b4)%2F(b5%2Bb4)&method=linear&rescale=-1,1
```

该 NDVI 示例要求第 5 波段是近红外、第 4 波段是红光，使用前检查数据元信息。
表达式与 `bidx` 互斥，可与受支持的 Gamma 组合。无效源值、除零或非有限结果透明。
当前不支持函数、条件运算和科学计数法；SDK 与现有表达式解析器共同限制输入大小。

## 颜色公式 {#color-formulas}

`effects.color_formula` 支持 Gamma，按数组顺序执行；Q3 示例：

```text
version=3.0&type=gray&bidx=1&color_formula=gamma%20rgb%202
```

暂不支持外部统计快照、calibration（除 none）、index、color_mapping、源调色板、
mode 重采样、地形、第三方扩展、其他颜色操作、JPEG 与有损 WebP。请求这些能力会报错。

## Mosaic

启用默认的 `mosaic` feature 后，JSON 中声明：

```json
"raster:mosaic": {"pixel_selection": "highest", "stage": "after_channels", "rank_channel": 1}
```

在 `extensions` 声明官方 Mosaic 1.0.0 Schema URL。Q3 的 `extensions` 参数传输声明数组，`extension_fields` 传输包含 `raster:mosaic` 的对象；旧的镶嵌短参数不再接受。
支持 first、highest、lowest、mean、median，以及 before_channels / after_channels 两个阶段。
使用 `mosaic.tile(coord, &style).await?`，选择方法直接取自样式。
`tile_with_options(coord, &style, &options)` 提供请求取消和超时控制。

before_channels 的排序位置对应去重后按源波段号升序排列的依赖表；after_channels
对应输出通道顺序。`rank_channel` 从 1 开始，仅用于 highest/lowest。
所有分量有效的同源元组才参加合并；不会把不同源的通道拼成一组。Gamma、色表和 opacity
在合并后执行一次。源 Alpha 波段限制仍适用。

资产列表由 MosaicJSON、STAC 或自定义 MosaicSource 提供，保持在样式之外。
配置、资源限制与示例见[镶嵌指南](/zh/docs/guides/mosaics/)。

## 迁移

旧 `from_query` / `from_pairs`、`ValidatedTileStyle`、`compile_validated`、可变样式字段、
`Stretch` / `ColourDefinition` / `Colour` / `lut_from_stops` 与独立 mosaic selection 参数已移除。
统一使用 `from_json` / `from_raster_query` / `from_raster_style`。
`TileStyle::default()` 是显式灰度波段 1、固定 0–255 的新规范样式。
HTTP 请求必须包含 `version=3.0&type=...`，旧参数与未知字段返回错误。
三个查看器只生成 Q3，Opacity 由服务端应用一次；MapLibre 提供 mosaic 阶段和排序通道。

| 旧输入 | 新输入 |
| --- | --- |
| `bidx=1&colormap_name=viridis&rescale=0,3000` | 增加 `version=3.0&type=pseudocolor&method=linear` |
| RGB 的三组相同 `rescale` | 一组 `rescale` 即可广播，但需显式 `type=rgb` 和三个 `bidx` |
| 不指定拉伸，对非 U8 自动 stddev | 新格式使用固定 `[0,255]`；需调用方显式给定范围 |
| 连续 stops 数组 `colormap` | 不能直接复用；新格式原生数组是区间表，连续 color_mapping 尚未实现 |
| `stretch=percent&pc=2,98` | `method=percentile&percentiles=2,98` |
