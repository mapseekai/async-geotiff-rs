pub mod color_formula;
pub mod colormap;
pub mod colour;
pub mod composite;
pub mod encode;
pub mod pipeline;

pub use colour::Colour;
pub use composite::{ColourDefinition, Composite, HandleGet};
pub use pipeline::{render, CompiledTileStyle};
