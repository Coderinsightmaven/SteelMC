//! Domain-scoped filled-map saved data.
//!
//! Issue #541 establishes the persistent map identity, color, scale, and lock
//! foundation needed by `MapItem`. Banner/frame markers and transient holder,
//! decoration, dirty-region, and packet state belong to the later map tracking
//! module rather than this persistence module.

use std::{collections::BTreeMap, io, sync::Arc};

use steel_registry::data_components::MapId;
use steel_utils::{
    Identifier,
    locks::SyncRwLock,
    saved_data::{SavedDataManager, names as saved_data_names},
};
use tokio::task::spawn_blocking;
use wincode::{SchemaRead, SchemaWrite};

use crate::{config::ResolvedDomainConfig, server::worlds::WorldMap, world::World};

const MAP_SIZE: i32 = 128;
const MAP_COLOR_COUNT: usize = MAP_SIZE as usize * MAP_SIZE as usize;

/// Largest scale accepted by Vanilla maps.
pub const MAX_SCALE: i8 = 4;

/// Map stores keyed by Steel domain.
pub(crate) struct DomainMapData {
    domains: BTreeMap<String, Arc<MapDataStore>>,
}

/// Vanilla's logical-server-owned map index and map saved data for one domain.
pub struct MapDataStore {
    saved_data: SavedDataManager,
    state: SyncRwLock<MapDataState>,
}

struct MapDataState {
    last_map_id: i32,
    maps: BTreeMap<MapId, MapItemSavedData>,
    revision: u64,
    saved_revision: u64,
}

/// Persistent data backing one filled map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MapItemSavedData {
    /// Map center X in world coordinates.
    center_x: i32,
    /// Map center Z in world coordinates.
    center_z: i32,
    /// Steel loaded-world identity corresponding to Vanilla's level resource key.
    dimension: Identifier,
    /// Whether player positions are tracked on the map.
    tracking_position: bool,
    /// Whether positions beyond the normal off-map range remain tracked.
    unlimited_tracking: bool,
    /// Map scale in `0..=4`.
    scale: i8,
    /// Packed map colors in row-major 128x128 order.
    colors: Vec<u8>,
    /// Whether terrain updates and further scaling are disabled.
    locked: bool,
}

/// Data required to allocate one fresh filled map.
pub struct NewMapData {
    origin_x: f64,
    origin_z: f64,
    scale: i8,
    tracking_position: bool,
    unlimited_tracking: bool,
    dimension: Identifier,
}

impl NewMapData {
    /// Creates blank map data matching Vanilla `MapItem.create`.
    pub fn blank(
        world: &World,
        origin_x: f64,
        origin_z: f64,
        scale: i8,
        tracking_position: bool,
        unlimited_tracking: bool,
    ) -> Self {
        Self {
            origin_x,
            origin_z,
            scale,
            tracking_position,
            unlimited_tracking,
            dimension: world.key.clone(),
        }
    }
}

#[derive(SchemaWrite, SchemaRead)]
struct PersistentMapData {
    last_map_id: i32,
    maps: Vec<PersistentMapItemSavedData>,
}

#[derive(SchemaWrite, SchemaRead)]
struct PersistentMapItemSavedData {
    id: i32,
    center_x: i32,
    center_z: i32,
    dimension: Identifier,
    tracking_position: bool,
    unlimited_tracking: bool,
    scale: i8,
    colors: Vec<u8>,
    locked: bool,
}

impl DomainMapData {
    /// Loads and binds one shared map store for every domain.
    pub(crate) async fn load_and_bind(
        domains: &[ResolvedDomainConfig],
        worlds: &WorldMap,
    ) -> io::Result<Self> {
        let mut map_data = BTreeMap::new();
        for domain in domains {
            let world = domain_default_world(worlds, &domain.name)?;
            let store = MapDataStore::load(world.saved_data.clone())
                .await
                .map_err(|error| map_data_io_error(&domain.name, error))?;
            map_data.insert(domain.name.clone(), Arc::new(store));
        }
        let result = Self { domains: map_data };
        for world in worlds.values() {
            let Some(domain_maps) = result.get(world.domain()) else {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "loaded world {} has no map-data owner for domain {}",
                        world.key,
                        world.domain()
                    ),
                ));
            };
            world.bind_map_data(Arc::clone(domain_maps))?;
        }
        Ok(result)
    }

    /// Returns the map store owned by `domain`.
    pub(crate) fn get(&self, domain: &str) -> Option<&Arc<MapDataStore>> {
        self.domains.get(domain)
    }

    /// Persists every changed domain map store.
    pub(crate) async fn save_all(&self) -> io::Result<usize> {
        let mut saved = 0;
        for (domain, map_data) in &self.domains {
            if map_data
                .save()
                .await
                .map_err(|error| map_data_io_error(domain, error))?
            {
                saved += 1;
            }
        }
        Ok(saved)
    }
}

impl MapDataStore {
    async fn load(saved_data: SavedDataManager) -> io::Result<Self> {
        let loader = saved_data.clone();
        let persistent = spawn_blocking(move || {
            loader.sync_load_wincode::<PersistentMapData>(saved_data_names::MAP_DATA)
        })
        .await
        .map_err(|error| io::Error::other(format!("map-data load task failed: {error}")))??;

        let state = match persistent {
            Some(persistent) => MapDataState::from_persistent(persistent)?,
            None => MapDataState::empty(),
        };
        Ok(Self {
            saved_data,
            state: SyncRwLock::new(state),
        })
    }

    /// Allocates and stores a fresh map centered with Vanilla's grid formula.
    pub fn create_map(&self, map: NewMapData) -> io::Result<MapId> {
        let map = MapItemSavedData::create_fresh(map);
        self.insert(map)
    }

    /// Allocates a new ID for an existing map value, such as a locked or scaled copy.
    pub fn insert(&self, map: MapItemSavedData) -> io::Result<MapId> {
        let mut state = self.state.write();
        let Some(id) = state.last_map_id.checked_add(1) else {
            return Err(io::Error::other("map ID space is exhausted"));
        };
        let map_id = MapId::new(id);
        if state.maps.contains_key(&map_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("map ID {id} is already allocated"),
            ));
        }
        state.maps.insert(map_id, map);
        state.last_map_id = id;
        state.revision = state.revision.wrapping_add(1);
        Ok(map_id)
    }

    /// Returns a snapshot of the saved data for `id`.
    pub fn get(&self, id: MapId) -> Option<MapItemSavedData> {
        self.state.read().maps.get(&id).cloned()
    }

    /// Mutates an allocated map and marks the store dirty.
    pub fn update<R>(
        &self,
        id: MapId,
        update: impl FnOnce(&mut MapItemSavedData) -> R,
    ) -> Option<R> {
        let mut state = self.state.write();
        let result = update(state.maps.get_mut(&id)?);
        state.revision = state.revision.wrapping_add(1);
        Some(result)
    }

    async fn save(&self) -> io::Result<bool> {
        let Some((revision, persistent)) = self.persistent_snapshot() else {
            return Ok(false);
        };
        let saver = self.saved_data.clone();
        spawn_blocking(move || saver.sync_save_wincode(saved_data_names::MAP_DATA, &persistent))
            .await
            .map_err(|error| io::Error::other(format!("map-data save task failed: {error}")))??;

        let mut state = self.state.write();
        if state.revision == revision {
            state.saved_revision = revision;
        }
        Ok(true)
    }

    fn persistent_snapshot(&self) -> Option<(u64, PersistentMapData)> {
        let state = self.state.read();
        if state.revision == state.saved_revision {
            return None;
        }
        let maps = state
            .maps
            .iter()
            .map(|(&id, map)| PersistentMapItemSavedData {
                id: id.id(),
                center_x: map.center_x,
                center_z: map.center_z,
                dimension: map.dimension.clone(),
                tracking_position: map.tracking_position,
                unlimited_tracking: map.unlimited_tracking,
                scale: map.scale,
                colors: map.colors.clone(),
                locked: map.locked,
            })
            .collect();
        Some((
            state.revision,
            PersistentMapData {
                last_map_id: state.last_map_id,
                maps,
            },
        ))
    }
}

impl MapDataState {
    const fn empty() -> Self {
        Self {
            last_map_id: -1,
            maps: BTreeMap::new(),
            revision: 0,
            saved_revision: 0,
        }
    }

    fn from_persistent(persistent: PersistentMapData) -> io::Result<Self> {
        if persistent.last_map_id < -1 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid last map ID {}", persistent.last_map_id),
            ));
        }
        let mut maps = BTreeMap::new();
        for map in persistent.maps {
            if map.id < 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid negative map ID {}", map.id),
                ));
            }
            if map.colors.len() != MAP_COLOR_COUNT {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "map {} color buffer has length {}, expected {MAP_COLOR_COUNT}",
                        map.id,
                        map.colors.len()
                    ),
                ));
            }
            let id = MapId::new(map.id);
            let previous = maps.insert(
                id,
                MapItemSavedData {
                    center_x: map.center_x,
                    center_z: map.center_z,
                    dimension: map.dimension,
                    tracking_position: map.tracking_position,
                    unlimited_tracking: map.unlimited_tracking,
                    scale: map.scale.clamp(0, MAX_SCALE),
                    colors: map.colors,
                    locked: map.locked,
                },
            );
            if previous.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("duplicate map ID {}", id.id()),
                ));
            }
        }
        if maps
            .last_key_value()
            .is_some_and(|(&id, _)| id.id() > persistent.last_map_id)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "last map ID precedes an allocated map",
            ));
        }
        Ok(Self {
            last_map_id: persistent.last_map_id,
            maps,
            revision: 0,
            saved_revision: 0,
        })
    }
}

impl MapItemSavedData {
    /// Vanilla `MapItemSavedData.createFresh`.
    fn create_fresh(map: NewMapData) -> Self {
        let scale = map.scale.clamp(0, MAX_SCALE);
        let (center_x, center_z) = Self::fresh_center(map.origin_x, map.origin_z, scale);
        Self {
            center_x,
            center_z,
            dimension: map.dimension,
            tracking_position: map.tracking_position,
            unlimited_tracking: map.unlimited_tracking,
            scale,
            colors: vec![0; MAP_COLOR_COUNT],
            locked: false,
        }
    }

    /// Returns the map center X in world coordinates.
    #[must_use]
    pub const fn center_x(&self) -> i32 {
        self.center_x
    }

    /// Returns the map center Z in world coordinates.
    #[must_use]
    pub const fn center_z(&self) -> i32 {
        self.center_z
    }

    /// Returns the loaded-world key corresponding to Vanilla's dimension key.
    #[must_use]
    pub const fn dimension(&self) -> &Identifier {
        &self.dimension
    }

    /// Returns whether player positions are tracked.
    #[must_use]
    pub const fn tracking_position(&self) -> bool {
        self.tracking_position
    }

    /// Returns whether positions beyond the normal off-map range remain tracked.
    #[must_use]
    pub const fn unlimited_tracking(&self) -> bool {
        self.unlimited_tracking
    }

    /// Returns the map scale.
    #[must_use]
    pub const fn scale(&self) -> i8 {
        self.scale
    }

    /// Returns the packed map colors in row-major 128x128 order.
    #[must_use]
    pub fn colors(&self) -> &[u8] {
        &self.colors
    }

    /// Returns whether terrain updates and further scaling are disabled.
    #[must_use]
    pub const fn is_locked(&self) -> bool {
        self.locked
    }

    /// Updates one map color, returning whether it changed or `None` for invalid coordinates.
    pub fn update_color(&mut self, x: usize, y: usize, color: u8) -> Option<bool> {
        let index = y
            .checked_mul(MAP_SIZE as usize)?
            .checked_add(x)
            .filter(|&index| index < self.colors.len() && x < MAP_SIZE as usize)?;
        if self.colors[index] == color {
            return Some(false);
        }
        self.colors[index] = color;
        Some(true)
    }

    /// Vanilla `MapItemSavedData.locked`.
    #[must_use]
    pub fn locked(&self) -> Self {
        let mut result = self.clone();
        result.locked = true;
        result
    }

    /// Vanilla `MapItemSavedData.scaled`.
    #[must_use]
    pub fn scaled(&self) -> Self {
        Self::create_fresh(NewMapData {
            origin_x: f64::from(self.center_x),
            origin_z: f64::from(self.center_z),
            scale: self.scale.saturating_add(1).min(MAX_SCALE),
            tracking_position: self.tracking_position,
            unlimited_tracking: self.unlimited_tracking,
            dimension: self.dimension.clone(),
        })
    }

    fn fresh_center(origin_x: f64, origin_z: f64, scale: i8) -> (i32, i32) {
        let size = MAP_SIZE * (1_i32 << scale);
        let area_x = ((origin_x + 64.0) / f64::from(size)).floor() as i32;
        let area_z = ((origin_z + 64.0) / f64::from(size)).floor() as i32;
        (area_x * size + size / 2 - 64, area_z * size + size / 2 - 64)
    }
}

fn domain_default_world<'a>(worlds: &'a WorldMap, domain: &str) -> io::Result<&'a World> {
    worlds
        .default_world(domain)
        .map(AsRef::as_ref)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("domain '{domain}' has no loaded default world"),
            )
        })
}

fn map_data_io_error(domain: &str, error: io::Error) -> io::Error {
    io::Error::new(
        error.kind(),
        format!("map-data I/O failed for domain '{domain}': {error}"),
    )
}

#[cfg(test)]
mod tests {
    use std::{
        env::temp_dir,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::*;

    fn new_map(origin_x: f64, origin_z: f64, scale: i8) -> NewMapData {
        NewMapData {
            origin_x,
            origin_z,
            scale,
            tracking_position: true,
            unlimited_tracking: false,
            dimension: Identifier::vanilla_static("overworld"),
        }
    }

    #[test]
    fn fresh_map_centers_and_scale_match_vanilla() {
        let data = MapItemSavedData::create_fresh(new_map(-65.0, -64.0, 0));
        assert_eq!((data.center_x, data.center_z), (-128, 0));

        let clamped = MapItemSavedData::create_fresh(new_map(3_000.0, -3_000.0, i8::MAX));
        assert_eq!(clamped.scale, MAX_SCALE);
        assert_eq!((clamped.center_x, clamped.center_z), (3_008, -3_136));
    }

    #[test]
    fn locking_copies_colors_while_scaling_creates_a_fresh_map() {
        let mut data = MapItemSavedData::create_fresh(new_map(500.0, -500.0, 2));
        data.colors[321] = 47;
        assert_eq!(data.update_color(128, 0, 1), None);

        let locked = data.locked();
        assert!(locked.locked);
        assert_eq!(locked.colors[321], 47);

        let scaled = data.scaled();
        assert_eq!(scaled.scale, 3);
        assert_eq!(scaled.colors[321], 0);
        assert!(!scaled.locked);
    }

    #[tokio::test]
    async fn map_data_round_trips_and_continues_the_id_sequence() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time should be after Unix epoch")
            .as_nanos();
        let path = temp_dir().join(format!("steel-map-data-round-trip-{unique}"));
        let saved_data = SavedDataManager::new(Some(path.as_path()));
        let store = MapDataStore::load(saved_data.clone())
            .await
            .expect("empty map data should load");
        let first_id = store
            .create_map(new_map(500.0, -500.0, 2))
            .expect("first map allocation should succeed");
        assert_eq!(
            store.update(first_id, |map| map.update_color(65, 2, 47)),
            Some(Some(true))
        );
        assert!(store.save().await.expect("changed map data should save"));

        let reloaded = MapDataStore::load(saved_data)
            .await
            .expect("saved map data should reload");
        let map = reloaded
            .get(first_id)
            .expect("saved map should be present after reload");
        assert_eq!((map.center_x, map.center_z), (704, -320));
        assert_eq!(map.colors.len(), MAP_COLOR_COUNT);
        assert_eq!(map.colors[321], 47);

        let next_id = reloaded
            .create_map(new_map(0.0, 0.0, 0))
            .expect("map allocation after reload should succeed");
        assert_eq!(next_id.id(), first_id.id() + 1);
    }
}
