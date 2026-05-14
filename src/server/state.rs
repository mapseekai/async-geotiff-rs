use std::sync::Arc;

use crate::tiler::Tiler;

#[derive(Clone)]
pub struct AppState {
    pub tiler: Arc<Tiler>,
}
