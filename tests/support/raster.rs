use async_geotiff::{
    bounds::{Bbox, GeoTransform},
    crs::Crs,
    pixel::{ImageData, PixelData},
    render::pipeline::render,
    style::{compile, TileStyle},
};
use ndarray::Array3;
use serde_json::Value;

pub(crate) fn pixels(style: Value, values: &[f64]) -> Vec<u8> {
    let style = TileStyle::from_json(&style.to_string()).unwrap();
    let image = ImageData {
        pixels: PixelData::F64(
            Array3::from_shape_vec((values.len(), 1, 1), values.to_vec()).unwrap(),
        ),
        mask: None,
        nodata: None,
        crs: Crs::epsg(3857),
        bbox: Bbox::new(0., 0., 1., 1.),
        transform: GeoTransform::from_gdal(&[0., 1., 0., 1., 0., -1.]),
    };
    image::load_from_memory(&render(&image, &compile(&style).unwrap()).unwrap())
        .unwrap()
        .to_rgba8()
        .into_raw()
}
