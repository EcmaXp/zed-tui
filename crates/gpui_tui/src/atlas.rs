use std::{borrow::Cow, sync::Arc};

use anyhow::Result;
use collections::HashMap;
use gpui::{
    AtlasKey, AtlasTextureId, AtlasTile, Bounds, DevicePixels, PlatformAtlas, Point, Size, TileId,
};
use parking_lot::Mutex;

#[derive(Default)]
struct AtlasState {
    tiles: HashMap<AtlasKey, AtlasTile>,
    next_tile_id: u32,
}

#[derive(Clone, Default)]
pub struct TuiAtlas(Arc<Mutex<AtlasState>>);

impl TuiAtlas {
    pub(crate) fn key_for(&self, tile_id: TileId) -> Option<AtlasKey> {
        self.0
            .lock()
            .tiles
            .iter()
            .find(|(_, tile)| tile.tile_id == tile_id)
            .map(|(key, _)| key.clone())
    }
}

impl PlatformAtlas for TuiAtlas {
    fn get_or_insert_with<'a>(
        &self,
        key: AtlasKey,
        build: &mut dyn FnMut() -> Result<Option<(Size<DevicePixels>, Cow<'a, [u8]>)>>,
    ) -> Result<Option<AtlasTile>> {
        let mut state = self.0.lock();
        if let Some(tile) = state.tiles.get(&key) {
            return Ok(Some(*tile));
        }

        let size = match &key {
            AtlasKey::Glyph(_) => match build()? {
                Some((size, _)) => size,
                None => return Ok(None),
            },
            AtlasKey::Svg(params) => params.size,
            AtlasKey::Image(_) => Size::new(DevicePixels(1), DevicePixels(1)),
        };

        state.next_tile_id += 1;
        let tile_id = state.next_tile_id;
        let tile = AtlasTile {
            texture_id: AtlasTextureId {
                index: 0,
                kind: key.texture_kind(),
            },
            tile_id: TileId(tile_id),
            padding: 0,
            bounds: Bounds {
                origin: Point::default(),
                size,
            },
        };
        state.tiles.insert(key, tile);
        Ok(Some(tile))
    }

    fn remove(&self, key: &AtlasKey) {
        self.0.lock().tiles.remove(key);
    }
}
