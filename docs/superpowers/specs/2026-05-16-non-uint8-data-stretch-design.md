# 非 uint8 栅格数据的默认拉伸设计

日期：2026-05-16
状态：设计已批准

## 问题

当请求一个非 uint8 的 COG（UInt16、Int16、UInt32、Int32、Float32、
Float64）瓦片，且**未**指定显式的 `?rescale=` 查询参数时，渲染器会套用
硬编码的 `0–255` 范围（`src/style/params.rs` 中的 `auto_rgb` /
单波段自动路径）。于是 UInt16 的值（0–65535）几乎全部饱和到 255，
客户端看到的是一张近乎全白的图。

titiler/rio-tiler 通过从 COG 中存储的**数据集波段统计值**推导默认拉伸
范围来规避这一问题。本设计为本项目加入相同行为。

## 已确定的决策（锁定）

- 默认范围来源：**来自 COG 元数据的数据集统计值**，并在缺失时按
  回退链处理。
- 由统计值推导范围的公式：**原始 `STATISTICS_MINIMUM` →
  `STATISTICS_MAXIMUM`**（可预测，与 GDAL/titiler 默认一致；不做
  σ/百分位对比度拉伸）。
- Float（F32/F64）且无统计标签：**按本瓦片 min/max**（无统计的整数
  使用 dtype 自然范围；仅在罕见的“无统计 float”情形下接受可能的
  瓦片接缝）。
- 适用范围：仅在未给显式 `rescale` 且 dtype 不是 U8 时触发。U8 的行为
  及 U8 快速路径保持不变。

## 架构（方案 A：在 tiler 中、读取与渲染之间解析）

理由：`compile()` / `compile_cached()` 构建 `rescale`，但无法获知数据集
的 dtype 或统计值，且 moka 样式缓存不能以数据集身份作为键。因此默认
范围在“读取+重投影”之后、样式编译之后再解析，然后传入 `render()`
（其签名已接受 rescale）。`params.rs` 保持与数据集无关；`render()` /
`pipeline.rs` 热路径不受影响。

```
CogReader.read_window()              src/io/cog.rs (约 412 行)
  ├─ 现有：gdal_nodata() → nodata
  └─ 新增：parse_band_stats(data_ifd.gdal_metadata(),
                            data_ifd.min_sample_value(),
                            data_ifd.max_sample_value())
        → band_stats: Option<Vec<(f64,f64)>>   （按原始波段索引）
  ↓
ImageData { …, nodata, band_stats }  src/pixel.rs:113  （新增一个字段）
  ↓
warp::warp()                          原样传递 band_stats
  ↓
tiler.tile()  第 5/6 步衔接处         src/tiler.rs  （新增一个调用点）
  if compiled.rescale.is_empty() && !is_u8(pixels) && !explicit_range_composite:
      effective = resolve_default_rescale(&pixels, band_stats, &compiled.bidx)
  else:
      effective = compiled.rescale
  ↓
render(pixels, &effective, …)         src/render/pipeline.rs（签名不变）
```

注意：统计标签描述的是全分辨率波段。从概览（overview）层级读取不会
显著改变值分布，titiler 也具备同样特性，因此无论概览层级如何，统计值
都按原样使用。

## 组件 1 —— 统计值解析（`src/io/stats.rs`，新建）

```rust
/// 从 COG 元数据解析的每波段 (min, max)，0 基波段索引。
pub fn parse_band_stats(
    gdal_metadata: Option<&str>,
    min_sample_value: Option<&[u16]>,
    max_sample_value: Option<&[u16]>,
) -> Option<Vec<(f64, f64)>>;
```

GDAL_METADATA（TIFF 标签 42112，`async_tiff 0.3.0` 通过
`gdal_metadata()` 暴露）是一个 XML 字符串：

```xml
<GDALMetadata>
  <Item name="STATISTICS_MINIMUM" sample="0">128</Item>
  <Item name="STATISTICS_MAXIMUM" sample="0">4071</Item>
  <Item name="STATISTICS_MINIMUM" sample="1">96</Item>
  ...
</GDALMetadata>
```

每个波段的解析优先级：

1. GDAL_METADATA 中的 `STATISTICS_MINIMUM` / `STATISTICS_MAXIMUM`，
   按 `sample=` 索引匹配。
2. 若存在则取 TIFF `SMinSampleValue` / `SMaxSampleValue`
   （`min_sample_value()` / `max_sample_value()`，标签 340/341）。
3. 两者均无该波段 → 该波段为 `None`（由下游处理）。

解析采用对 `<Item name=… sample=…>value</Item>` 的小型手写扫描——不引入
新依赖。XML 缺失或格式错误返回 `None`，绝不报错。与 `cog.rs:412`
现有的 `gdal_nodata().and_then(|s| s.parse().ok())` 模式保持一致。

## 组件 2 —— `ImageData` 管线接入

`src/pixel.rs:113` 新增一个字段：

```rust
pub struct ImageData {
    pub pixels: PixelData,
    pub mask: Option<Array2<bool>>,
    pub bbox: Bbox,
    pub crs: Crs,
    pub transform: GeoTransform,
    pub nodata: Option<f64>,
    pub band_stats: Option<Vec<(f64, f64)>>, // 新增：全波段 (min,max)
}
```

`band_stats` 按**原始**波段顺序索引。`warp` 原样传递该字段（仅一行
结构体字段拷贝；不改动数值）。其他 `ImageData` 构造处设
`band_stats: None`。

## 组件 3 —— `resolve_default_rescale`（`src/style/rescale.rs`，新建）

```rust
pub fn resolve_default_rescale(
    pixels: &PixelData,
    band_stats: Option<&[(f64, f64)]>,
    bidx: &[usize],            // 0 基的已编译/已选波段
) -> Vec<(f64, f64)>;
```

纯函数（无 IO）。对每个选中的波段，按优先级：

1. **该波段有统计值** → 取 `band_stats` 中的原始 `(min, max)`。
2. **无统计、整数 dtype** → dtype 自然范围：
   - U16 → `(0.0, 65535.0)`
   - I16 → `(-32768.0, 32767.0)`
   - U32 → `(0.0, 4294967295.0)`
   - I32 → `(-2147483648.0, 2147483647.0)`
   - U8 → 不会到达（调用方已跳过 u8）
3. **无统计、float dtype（F32/F64）** → 本瓦片 min/max：对该波段像素
   单遍扫描，跳过 nodata 与掩膜无效样本。

退化情形保护：

- `max <= min`（平坦波段 / 全 nodata 瓦片）→ 扩展为 `(min, min + 1.0)`。
- float 单遍扫描未找到有效样本 → `(0.0, 1.0)`。

这是拉伸策略的唯一归属处，且无需 IO 即可完整单元测试。`tiler.rs`
调用点是 float 回退唯一需要访问像素的地方，而那里恰好持有重投影后的
`PixelData`。

## 触发条件

仅在**全部**满足时套用默认值：

- 编译后的样式 `rescale` 为空（未给 `?rescale=`），且
- dtype 不是 U8，且
- 该 composite 不是离散 / 显式 vmin-vmax 的 colormap（它们已自带范围）。

显式 `?rescale=` 始终优先。U8 保持现有快速路径不变（8 位 RGB 行为与
性能零变化）。单波段非 u8（自动 "greys" colormap）与三波段非 u8
auto-RGB 都走该解析器。

## 错误处理

本特性绝不引入硬错误。GDAL XML 格式错误、标签缺失、全 nodata 瓦片
均沿优先级链静默回退。

## 测试

- 单元（`stats.rs`）：多波段 `sample=` 项；缺项；XML 格式错误 →
  `None`；SMin/SMax 回退路径。
- 单元（`rescale.rs`）：各优先级分支；退化 `max<=min`；float 单遍
  扫描跳过 nodata；`bidx` 重排选中正确的统计项。
- 集成：含统计值的合成 UInt16 COG → 输出覆盖完整色调范围（断言并非
  近乎全白）；不含统计值的 UInt16 → dtype 范围路径（记录偏暗的
  权衡）；无统计的 F32 → 本瓦片路径产生可见对比。
- 回归：现有 U8 RGB 瓦片与改动前逐字节一致（守护快速路径）。

## 不在范围内（YAGNI）

百分位 / mean±σ 对比度拉伸；GDAL SCALE/OFFSET 应用；整数的逐瓦片
拉伸；`/statistics` 端点；u8 的自动对比度。
