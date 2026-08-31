---
title: 快速上手
description: 运行示例服务并请求第一张渲染瓦片。
weight: 20
---

## 克隆并运行 {#run}

```bash
git clone https://github.com/zwishing/async-geotiff-rs.git
cd async-geotiff-rs
COG_URL='https://raw.githubusercontent.com/cogeotiff/rio-tiler/0b08b7f35a8b639cee2f35a0cb565034f7b55bfd/tests/fixtures/cog.tif'
cargo run --release --example serve -- "$COG_URL"
```

固定提交版本的 `rio-tiler` 测试文件约 800 KiB，是一个公开且支持 Range 的 COG，
适合首次运行验证。冒烟测试完成后再替换为自己的 COG。服务默认监听
`127.0.0.1:8080`，并在启动成功后打印查看器与瓦片 URL。

打开任一内置查看器：

- <http://127.0.0.1:8080/maplibre>
- <http://127.0.0.1:8080/openlayers>
- <http://127.0.0.1:8080/leaflet>

查看器会自动居中到已打开的数据集，是最可靠的可视化冒烟测试。也可以直接请求全球
0 级瓦片：

```bash
curl --fail --output tile.png \
  'http://127.0.0.1:8080/tiles/WebMercatorQuad/0/0/0.png'
```

成功时会生成可用任意图像查看器打开的 PNG 文件。合法但完全位于源数据范围之外的
坐标返回 HTTP 204，而不是合成一张空白图片；遇到这种情况，请使用内置查看器在自动
计算的数据集中心和缩放级别请求范围内瓦片。

## 浏览本地文件 {#local-files}

不传 URL 时，服务会发现 `data/*.tif` 和 `data/*.tiff`：

```bash
mkdir -p data
cp /path/to/example.tif data/
cargo run --release --example serve
```

查看器中会出现数据集切换器。

## 可选输出格式 {#formats}

PNG 始终可用。WebP 与 JPEG 需要显式启用：

```bash
cargo run --release --features webp,jpeg --example serve -- \
  https://example.com/cog.tif
```

`/formats` 端点只返回当前二进制实际编译的编码器。
