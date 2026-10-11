use std::{borrow::Cow, sync::Arc};

use anyhow::Result;
use collections::HashMap;
use gpui::{
    AtlasKey, AtlasTextureId, AtlasTile, Bounds, DevicePixels, PlatformAtlas, Point, Size, TileId,
};
use parking_lot::{Mutex, MutexGuard};

#[derive(Default)]
struct AtlasState {
    tiles: HashMap<AtlasKey, AtlasTile>,
    keys: Vec<Option<AtlasKey>>,
}

#[derive(Clone, Default)]
pub struct TuiAtlas(Arc<Mutex<AtlasState>>);

pub(crate) struct TileKeys<'a>(MutexGuard<'a, AtlasState>);

impl TileKeys<'_> {
    pub(crate) fn get(&self, tile_id: TileId) -> Option<&AtlasKey> {
        self.0.keys.get(key_index(tile_id)?)?.as_ref()
    }
}

fn key_index(tile_id: TileId) -> Option<usize> {
    usize::try_from(tile_id.0).ok()?.checked_sub(1)
}

impl TuiAtlas {
    pub(crate) fn tile_keys(&self) -> TileKeys<'_> {
        TileKeys(self.0.lock())
    }
}

impl PlatformAtlas for TuiAtlas {
    fn get_or_insert_with<'a>(
        &self,
        key: AtlasKey,
        build: &mut dyn FnMut() -> Result<Option<(Size<DevicePixels>, Cow<'a, [u8]>)>>,
    ) -> Result<Option<AtlasTile>> {
        if let Some(tile) = self.0.lock().tiles.get(&key) {
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

        let mut state = self.0.lock();
        if let Some(tile) = state.tiles.get(&key) {
            return Ok(Some(*tile));
        }
        state.keys.push(Some(key.clone()));
        let tile_id = u32::try_from(state.keys.len())?;
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
        let mut state = self.0.lock();
        if let Some(tile) = state.tiles.remove(key)
            && let Some(slot) = key_index(tile.tile_id).and_then(|index| state.keys.get_mut(index))
        {
            *slot = None;
        }
    }
}
