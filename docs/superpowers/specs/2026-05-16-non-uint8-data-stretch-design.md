# 栅格数据拉伸:多模式 rescale 设计

日期：2026-05-16
状态：设计已批准（v3，含统计存储与精度策略定稿）

## 问题

非 uint8 的 COG（UInt16 等）未指定 `?rescale=` 时套用硬编码
`0–255`，导致几乎全饱和成白。且固定自定义 min/max 不够用，需对标
QGIS 提供可选拉伸模式，并在前端不传参时给合理默认。

## 需求（锁定）

四种拉伸模式（对标 QGIS），优先级
`rescale`（自定义） > `stretch`（模式） > **默认 `stddev` k=2.0**：

1. **Custom**（现状）：`rescale=min,max`，每波段重复。
2. **MinMax**：数据 min/max，按波段。
3. **Percent**（累计计数裁剪）：如 2%–98%，区间可配，按波段。
4. **StdDev**：`mean ± k·σ`，k 默认 2.0，按波段。

### 查询参数

```
?rescale=470,660&rescale=470,700&rescale=350,730   # 自定义（最高优先级）
?stretch=minmax
?stretch=percent&pc=2,98
?stretch=stddev&sigma=2.5
# 不传任何拉伸参数 → 等价 ?stretch=stddev&sigma=2.0
```

- `stretch`：`minmax|percent|stddev`（大小写不敏感）。
- `pc=lo,hi`：百分位区间，默认 `2,98`；全局值，截断点按波段各算。
- `sigma=k`：σ 倍数，默认 `2.0`；全局值，每波段用各自 mean/σ。
- `rescale` 与 `stretch` 同时给 → `rescale` 胜。
- `stretch`/`pc`/`sigma` 非法 → 400（参数校验，非静默）。
- 所有模式按波段独立计算（QGIS 默认）。
- u8 且 `rescale`、`stretch` 均未给：保持现有 u8 快速路径，**不**
  套默认 stddev（零行为/性能变化）；显式 `stretch` 对 u8 仍生效。

## 统计数据：来源与精度（锁定）

需要统计的模式：MinMax / Percent / StdDev（Custom 不需要）。

**来源优先级**

1. **内嵌 STATISTICS 标签 → 精确**（写文件时 GDAL 在全分辨率上
   算出，零运行时成本）。GDAL_METADATA（TIFF 42112，`async_tiff`
   `gdal_metadata()`）：`STATISTICS_MINIMUM/MAXIMUM/MEAN/STDDEV`
   及直方图 `STATISTICS_HISTOMIN/HISTOMAX/HISTONUMBINS/HISTOBINS`
   （按 `sample=` 波段索引）；min/max 退路 `SMin/SMaxSampleValue`
   （标签 340/341）。
2. **无标签 → 顶层（最高/最小）概览单遍计算 → 近似**。仅读最高
   概览全幅（本例 441×456≈20 万像素×3 波段×2B ≈ 1.2MB，亚秒级，
   与原图 9GB 无关），单遍算每波段 `min/max/mean/σ` + 定长直方图
   （默认 256 bin）。
3. **无概览的非规范 COG** → 不做全分辨率扫描；直接回退
   “整数用 dtype 自然范围、float 用本瓦片 min/max”，记 warn。

**精度策略（已定：概览近似 + 标注说明）**

概览经下采样平滑，各统计量偏差不同，如实记录并对外标注：

| 统计量 | 概览近似偏差 |
|---|---|
| min/max | 极值被平滑，范围**向内收窄**，偏差最大 |
| mean | 基本准确 |
| stddev | 被**低估**（拉伸范围偏窄），中等偏差 |
| 百分位(2/98) | 对平滑**相当稳健**，偏差最小 |

- 统计来源标注：`DatasetStats.provenance: {Exact, Estimated}`；瓦片
  响应加 `X-Stats-Provenance: exact|estimated` 头，便于前端/排错
  知情。
- 因近似偏差，**默认 StdDev 与可选 Percent 是稳健选择**；MinMax
  在 Estimated 下文档明确标注“向内收窄的近似”。
- 需精确：离线给 COG 补统计（`gdal_edit.py -stats` 或转 COG 时
  `-stats`），本服务即命中精确标签路径，前端无需改动。
- 不提供运行时全图精确扫描（YAGNI；精度/速度权衡交由离线补标签）。

## 统计的存储与生命周期（锁定）

复用现有按数据集存活的对象，**不**另起独立缓存：
`AppState.tilers: moka::Cache<String, Arc<Tiler>>` 已使每个数据集
的 `Arc<Tiler>` 跨所有瓦片请求存活（首次 `try_get_with` 合并并发）。

统计作为 `Tiler` 上的**惰性记忆化字段**：

```rust
pub struct Tiler {
    reader: Arc<CogReader>,
    stats: tokio::sync::OnceCell<Arc<DatasetStats>>,  // 新增
}

impl Tiler {
    async fn stats(&self) -> AgResult<Arc<DatasetStats>> {
        self.stats.get_or_try_init(|| async {
            Ok(Arc::new(load_dataset_stats(&self.reader).await?))
        }).await.cloned()
    }
}
```

- **惰性**：首个“需要统计”的瓦片（非 Custom）触发；`?rescale=`
  自定义请求永不触发，零额外开销。
- 算一次后存于 `OnceCell`，随 `AppState.tilers` 缓存存活；后续请求
  零重算，并发首请求由 `OnceCell` 合并（与 moka 打开 Tiler 同语义）。
- 选惰性而非“打开即算”：`open_http` 仅读元数据很轻；若打开即算，
  纯 `?rescale=` 用户也白付一次概览读取。惰性命中后与“打开即算”
  完全等价（同存于长寿对象）。

## 架构

`compile()` / 样式 moka 缓存仍与数据集无关。统计获取需异步 IO
（读概览），故在 tiler 中、读取+重投影后、`render()` 前解析出
effective rescale 传入 `render()`（签名已接受 rescale）。`render()`
/ `pipeline.rs` 热路径不变。`ImageData` 不新增字段（统计由 Tiler
惰性提供，不随瓦片搬运）。

```
请求解析（server.rs）→ StretchSpec
  rescale=..  → Custom(Vec<(f64,f64)>)
  stretch=..  → MinMax | Percent{lo,hi} | StdDev{k}
  均无        → StdDev{2.0}（u8 默认请求除外，走原快速路径）
  ↓
tiler.tile() 读取+重投影后、render 前：
  effective = match spec {
    Custom(v) => v,
    mode      => resolve_rescale(mode, &*self.stats().await?, &compiled.bidx),
  };
  ↓
render(pixels, &effective, …)   src/render/pipeline.rs（签名不变）
```

注意：统计描述全幅分布，跨缩放级别一致复用（与 QGIS estimate 同理）。

## 组件

### `src/style/stretch.rs`（新建）

```rust
pub enum StretchSpec {
    Custom(Vec<(f64, f64)>),
    MinMax,
    Percent { lo: f64, hi: f64 }, // 默认 2,98
    StdDev  { k: f64 },           // 默认 2.0
}
pub fn parse_stretch(q: &Query) -> Result<StretchSpec, ParamError>;
pub fn resolve_rescale(
    spec: &StretchSpec, stats: &DatasetStats, bidx: &[usize],
) -> Vec<(f64, f64)>;
```

校验：`0<=lo<hi<=100`；`k>0`；`stretch` 仅三枚举。

求解（逐选中波段）：
- `Custom(v)` → 直接返回 `v`。
- `MinMax` → `(b.min, b.max)`。
- `Percent{lo,hi}` → 直方图累计计数线性插值求分位。
- `StdDev{k}` → `(mean-k·σ, mean+k·σ)` 再夹到 `[min,max]`。
- 退化 `max<=min` → `(min, min+1.0)`。
- `Custom` 波段数与选中数不匹配 → 实现时先核对现有 rescale 行为
  并对齐（沿用现状语义，不在本设计擅改）。

### `src/io/stats.rs`（新建）

```rust
pub enum Provenance { Exact, Estimated }
pub struct BandStat { pub min:f64, pub max:f64, pub mean:f64,
                      pub stddev:f64, pub hist: Histogram }
pub struct DatasetStats { pub bands: Vec<BandStat>,
                          pub provenance: Provenance } // 原始波段索引

pub async fn load_dataset_stats(reader:&CogReader) -> AgResult<DatasetStats>;
fn parse_embedded_stats(ifd:&Ifd) -> Option<DatasetStats>;        // Exact
async fn compute_from_top_overview(r:&CogReader)
    -> AgResult<DatasetStats>;                                    // Estimated
```

统计计算全程排除 `nodata`（本例为 0）与掩膜无效样本。

## 错误处理

- 参数非法（stretch/pc/sigma）→ 400，前端可定位。
- 概览读取/解码失败、全 nodata、无概览 → 不致命：回退 dtype/
  本瓦片 min/max 并记 warn，仍出图。

## 测试

- 单元 `stretch.rs`：rescale 优先；缺省→StdDev2.0；非法→Err；
  大小写；四模式数值；StdDev 夹 min/max；退化；bidx 重排。
- 单元 `stats.rs`：GDAL_METADATA 多波段解析；直方图分位线性插值；
  SMin/SMax 回退；nodata 排除；provenance 标注正确。
- 集成：含标签 UInt16（Exact，各模式数值符合）；无标签 UInt16
  （本例数据集，概览路径 Estimated、默认 stddev 不再全白）；
  无概览 → 回退路径仍出图；F32 概览路径可见对比。
- 回归：U8 RGB 默认请求与改动前逐字节一致（守护快速路径）。

## 不在范围内（YAGNI）

运行时全分辨率精确扫描 / `stats=estimate|exact` 旋钮；GDAL
SCALE/OFFSET；逐瓦片 estimate 模式；gamma/sigmoid 非线性拉伸；
`/statistics` 端点；u8 默认自动对比度。
