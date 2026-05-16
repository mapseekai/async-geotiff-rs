# 栅格数据拉伸:多模式 rescale 设计

日期：2026-05-16
状态：设计已批准（v2，扩展为多模式）

## 问题

当请求非 uint8 的 COG（UInt16 等）瓦片且未指定显式 `?rescale=` 时，
渲染器套用硬编码 `0–255`，导致 UInt16（0–65535）几乎全饱和成白。
此外，固定的自定义 min/max 不够用：不同数据需要不同的拉伸策略
（最大最小值、百分比裁剪、均值±标准差），希望对标 QGIS 提供可选
拉伸模式，并在前端不传参时给出合理默认。

## 需求（锁定）

拉伸方式从“单一自定义”升级为**带模式**，共四种（对标 QGIS）：

1. **自定义 / UserDefined**（现状）：`rescale=min,max`，每波段重复。
2. **最大最小值 / MinMax**：用数据真实 min/max，按波段。
3. **百分比裁剪 / Cumulative count cut**：如 2%–98%，区间可配，按波段。
4. **均值±标准差 / StdDev**：`mean ± k·σ`，k 默认 2.0，按波段。

优先级：`rescale`（自定义） > `stretch`（模式） > **默认 `stddev`，k=2.0**。

### 查询参数

```
# 自定义（最高优先级，保持现状）
?rescale=470,660&rescale=470,700&rescale=350,730

# 最大最小值
?stretch=minmax

# 百分比裁剪 2%–98%
?stretch=percent&pc=2,98

# 均值±σ，k=2.5
?stretch=stddev&sigma=2.5

# 不传任何拉伸参数 → 等价 ?stretch=stddev&sigma=2.0
```

- `stretch` 取值：`minmax` | `percent` | `stddev`（大小写不敏感）。
- `pc=lo,hi`：百分位区间，默认 `2,98`；仅 `percent` 用；全局值，
  对所有波段生效，但产出的截断点按波段各自计算。
- `sigma=k`：标准差倍数，默认 `2.0`；仅 `stddev` 用；全局值，
  每波段用各自的 mean/σ。
- 同时给 `rescale` 与 `stretch` → `rescale` 胜，忽略 `stretch`。
- `stretch` 值非法 → 返回明确 400 错误（参数校验，非静默）。
- 所有模式均**按波段独立**计算（与 QGIS 默认一致）。
- u8 且 `stretch` 未给且 `rescale` 未给：保持现有 u8 快速路径，
  **不**套用默认 stddev（u8 0–255 已正确，零行为/性能变化）。
  显式 `stretch` 对 u8 同样生效。

## 统计数据来源（锁定）

MinMax / 百分比 / 均值±σ 都需要统计量。策略：
**优先读 COG 内嵌标签；缺失则从最高概览层单遍计算并按数据集缓存。**

内嵌标签（GDAL_METADATA，TIFF 标签 42112，`async_tiff 0.3.0`
`gdal_metadata()`）：

- `STATISTICS_MINIMUM` / `STATISTICS_MAXIMUM` / `STATISTICS_MEAN` /
  `STATISTICS_STDDEV`（按 `sample=` 波段索引）。
- 直方图（百分位用）：`STATISTICS_HISTOMIN` / `STATISTICS_HISTOMAX` /
  `STATISTICS_HISTONUMBINS` / `STATISTICS_HISTOBINS`（逗号分隔计数）。
- 退而其次的 min/max 来源：TIFF `SMinSampleValue` /
  `SMaxSampleValue`（标签 340/341）。

缺失任一所需量时，对该数据集触发**一次性概览计算**：读取最高
（最小）概览层全幅，单遍算出每波段 `min/max/mean/σ` 与定长直方图
（默认 256 bin，范围取 min..max），结果缓存。`nodata`（本例为 0）
与掩膜无效样本在所有统计中排除。

缓存：moka，键为 `dataset URI`，值为 `Arc<DatasetStats>`。首个用到
统计的请求承担一次性计算延迟（9GB 文件也只读最高概览，秒级），
之后命中缓存。与现有 moka 样式缓存相互独立，互不污染键。

## 架构

`compile()` / 样式缓存仍与数据集无关；统计获取需异步 IO（读概览），
故放在 tiler 中、读取与渲染之间解析，结果作为 effective rescale 传入
`render()`（签名已接受 rescale）。`render()` / `pipeline.rs` 热路径
不变。

```
请求解析（server.rs）
  ├─ rescale=...           → StretchSpec::Custom(Vec<(f64,f64)>)
  ├─ stretch=minmax        → StretchSpec::MinMax
  ├─ stretch=percent&pc=.. → StretchSpec::Percent{lo,hi}
  ├─ stretch=stddev&sigma= → StretchSpec::StdDev{k}
  └─ 均无                   → StretchSpec::StdDev{k:2.0}（u8 除外，见上）
  ↓
tiler.tile()  读取+重投影后、render 前：
  let effective: Vec<(f64,f64)> = match stretch_spec {
      Custom(v)        => v,
      _ if is_u8 && spec_is_default => 走原 u8 快速路径,
      mode             => {
          let stats = stats_provider.get(dataset_uri).await?; // 标签或概览计算+缓存
          resolve_rescale(mode, &stats, &compiled.bidx)
      }
  };
  ↓
render(pixels, &effective, …)   src/render/pipeline.rs（签名不变）
```

注意：统计标签/概览描述全幅波段分布，从某概览层渲染不显著改变分布，
故统计跨缩放级别一致复用（与 titiler/QGIS estimate 同理）。

## 组件 1 —— 拉伸规格解析（`src/style/stretch.rs`，新建）

```rust
pub enum StretchSpec {
    Custom(Vec<(f64, f64)>),     // 每波段 (min,max)，沿用现有 rescale 解析
    MinMax,
    Percent { lo: f64, hi: f64 },// 默认 2.0, 98.0
    StdDev  { k: f64 },          // 默认 2.0
}

/// 从查询参数解析。rescale 优先；否则按 stretch；都无 → StdDev{2.0}。
/// 非法 stretch / pc / sigma → Err（400）。
pub fn parse_stretch(params: &Query) -> Result<StretchSpec, ParamError>;
```

校验：`pc` 满足 `0<=lo<hi<=100`；`sigma>0`；`stretch` 仅三枚举值。

## 组件 2 —— 数据集统计（`src/io/stats.rs`，新建）

```rust
pub struct BandStat {
    pub min: f64, pub max: f64,
    pub mean: f64, pub stddev: f64,
    pub hist: Histogram,   // 边界 + 计数，用于分位
}
pub struct DatasetStats { pub bands: Vec<BandStat> } // 按原始波段索引

/// 标签优先；缺量则读最高概览单遍计算。按 dataset URI 缓存。
pub async fn dataset_stats(reader: &CogReader) -> AgResult<Arc<DatasetStats>>;

/// 解析 GDAL_METADATA / SMin·SMaxSampleValue / 直方图标签。
fn parse_embedded_stats(ifd: &Ifd) -> Option<DatasetStats>;
```

直方图分位：在累计计数上线性插值求 `lo%`、`hi%` 对应值（近似，
与 QGIS estimate 一致）。

## 组件 3 —— rescale 求解（`src/style/stretch.rs`）

```rust
pub fn resolve_rescale(
    spec: &StretchSpec,
    stats: &DatasetStats,
    bidx: &[usize],          // 0 基已编译波段
) -> Vec<(f64, f64)>;
```

逐选中波段：

- `Custom(v)`：直接返回 `v`（不需要 stats）。
- `MinMax`：`(b.min, b.max)`。
- `Percent{lo,hi}`：`(hist.quantile(lo), hist.quantile(hi))`。
- `StdDev{k}`：`(b.mean - k*b.stddev, b.mean + k*b.stddev)`，
  再夹到 `[b.min, b.max]`。

退化保护：

- `max <= min`（平坦/全 nodata）→ 扩为 `(min, min + 1.0)`。
- `Custom` 波段数与选中波段数不匹配 → 复用最后一组或返回 400
  （与现有 rescale 行为保持一致，实现时确认现状并对齐）。

## ImageData 改动

不再新增 `band_stats` 字段（v1 方案废弃）：统计改由
`stats.rs` 的按数据集缓存提供，避免每瓦片搬运。`ImageData`
保持现状不变。

## 错误处理

- 参数层（`stretch`/`pc`/`sigma` 非法）→ 明确 400，便于前端排错。
- 统计层（标签缺失、概览读取/解码失败、全 nodata）→ 不致命：
  概览计算失败则回退“整数用 dtype 自然范围、float 用本瓦片 min/max”
  并记录 warn；保证仍出图。

## 测试

- 单元（`stretch.rs` 解析）：rescale 优先于 stretch；缺省→StdDev2.0；
  非法 stretch/pc/sigma→Err；大小写不敏感。
- 单元（`stretch.rs` 求解）：四模式数值正确；StdDev 夹到 min/max；
  退化 `max<=min`；bidx 重排取对波段。
- 单元（`stats.rs`）：GDAL_METADATA 多波段解析；直方图分位线性插值；
  SMin/SMax 回退；nodata 排除。
- 集成：含统计标签的合成 UInt16 COG → 各模式输出符合预期；
  无标签 UInt16（即本例数据集）→ 概览计算路径产出可见对比、
  默认 stddev 不再全白；F32 概览计算可见对比。
- 回归：U8 RGB 默认请求与改动前逐字节一致（守护快速路径）。

## 不在范围内（YAGNI）

GDAL SCALE/OFFSET 应用；逐瓦片 estimate 模式（QGIS 的“当前范围”）；
gamma / sigmoid 等非线性拉伸；`/statistics` 端点；u8 默认自动对比度。
