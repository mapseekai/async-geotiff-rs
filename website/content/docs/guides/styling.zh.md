---
title: 样式与波段计算
description: 选择波段、拉伸数值、应用色带，并通过表达式生成派生波段。
weight: 10
---

Rust API 与示例服务的查询字符串使用同一个 `TileStyle` 模型。样式会在昂贵的 COG
读取和渲染工作之前完成校验。

## 常用配方 {#recipes}

三波段自然色 RGB：

```text
bidx=1&bidx=2&bidx=3&rescale=0,3000&rescale=0,3000&rescale=0,3000
```

单波段色带：

```text
bidx=1&colormap_name=viridis&rescale=0,3000
```

百分位拉伸：

```text
stretch=percent&pc=2,98
```

下采样使用 average，重投影使用 bilinear：

```text
resampling=average&reproject=bilinear
```

## 拉伸行为 {#stretch}

对于非 U8 数据，有效值域按以下优先级选择：

1. 显式的 `rescale=min,max`。
2. 指定的 `stretch` 模式。
3. 两者都不存在时使用 `stretch=stddev&sigma=2.0`。

U8 RGB 保留 0–255 快速路径。数据统计优先来自内嵌 `STATISTICS_*` 标签；如果没有，
则采样最高概览层一次并缓存估算结果。

## 算术表达式 {#expressions}

`expression` 生成一个派生输出波段，波段编号从 1 开始：

```text
expression=(b5-b4)/(b5+b4)&rescale=-1,1&colormap_name=viridis
```

这个 NDVI 公式只适用于所选 COG 的第 5 波段为近红外、第 4 波段为红光的情况。库不会
根据波段编号推断光谱含义；使用前必须检查资产元数据并调整引用。预渲染的 STAC
`visual` 资产也不会自动满足此示例。

放入 URL 时请对 `/` 和 `+` 进行百分号编码：

```bash
curl --output ndvi.png \
  'http://127.0.0.1:8080/tiles/WebMercatorQuad/0/0/0.png?expression=(b5-b4)%2F(b5%2Bb4)&rescale=-1,1&colormap_name=viridis'
```

语法支持有限十进制数、括号、二元 `+`、`-`、`*`、`/` 和一元负号。乘除优先于
加减，不支持科学计数法。

`expression` 不能与 `bidx` 或 `color_formula` 同时使用，并且最多接受一个
`rescale`。nodata 输入、除零和其他非有限结果会变为透明像元。输入上限为 4096
字节、256 个 token 和 128 层嵌套。

## 颜色公式 {#color-formulas}

支持的 rio-color 子集按从左到右的顺序应用：

- `gamma`
- `sigmoidal`
- `saturation`

例如：

```text
color_formula=gamma RGB 1.5
```

放入 URL 查询字符串时需要对空格编码。
