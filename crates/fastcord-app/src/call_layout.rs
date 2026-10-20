//! Layouts of a call's video tiles (SPEC §8.10, issue #86).
//!
//! Pure decisions, no I/O: which of the call's videos (the self-view, other
//! participants' cameras, watched Go Live streams) are drawn, how large, and
//! which stay hidden. A hidden tile is not decoded and, for a camera, not
//! requested from the server, so what this module lets through is exactly what
//! costs CPU, memory and GPU time. The caps below are the documented budget.

use std::cell::Cell;
use std::fmt;
use std::sync::Arc;

use fastcord_model::{Snowflake, StreamKey};

use crate::gateway::TilesView;
use crate::playback::Session;

/// Videos drawn at once: the number of plane pairs the NV12 pass binds.
pub const MAX_VISIBLE_TILES: usize = crate::playback::MAX_VIDEOS;
/// Go Live streams watched at once; each is a connection and a decoder of its own.
pub const MAX_WATCHED_STREAMS: usize = 4;
/// With more tiles than this a grid tile is small (and a camera asks for its lowest layer).
const GRID_LARGE_TILES: usize = 4;

/// What a tile shows.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum TileId {
    /// The local camera, as sent.
    Preview,
    Camera(Snowflake),
    /// A watched Go Live stream, by its wire key.
    Stream(String),
}

impl TileId {
    pub fn camera_user(&self) -> Option<Snowflake> {
        match self {
            Self::Camera(user) => Some(*user),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// Every video the same size.
    #[default]
    Grid,
    /// One chosen video large, the others in a strip.
    Focus,
    /// Like focus, following whoever speaks on camera.
    Speaker,
}

impl Mode {
    pub const ALL: [Self; 3] = [Self::Grid, Self::Focus, Self::Speaker];
}

impl fmt::Display for Mode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Grid => "Grid",
            Self::Focus => "Focus",
            Self::Speaker => "Speaker",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TileSize {
    Large,
    Small,
}

/// What the call view looks like, chosen by the user.
#[derive(Debug, Default)]
pub struct CallView {
    /// The large call view is open; otherwise only the compact camera strip shows.
    pub open: bool,
    pub mode: Mode,
    pub pinned: Option<TileId>,
    /// One video fills the call view.
    pub full: Option<TileId>,
    /// Participants without video are not listed beside the tiles.
    pub hide_people: bool,
    /// The last participant seen speaking on camera (Speaker mode keeps them
    /// large through silence).
    speaker: Cell<Option<Snowflake>>,
}

/// Who streams: the owner named by a stream's wire key.
pub fn stream_owner(key: &str) -> Option<Snowflake> {
    match StreamKey::from_wire(key)? {
        StreamKey::Guild { owner_id, .. } | StreamKey::Call { owner_id, .. } => Some(owner_id),
    }
}

/// One video of the call and where its pictures arrive.
pub struct Tile {
    pub id: TileId,
    pub session: Arc<Session>,
}

/// Every video of the call in layout order: watched streams, other
/// participants' cameras, then the self-view while the camera is on.
pub fn tiles(streams: &[Arc<Session>], cameras: &TilesView, self_view: bool) -> Vec<Tile> {
    let streams = streams.iter().map(|session| Tile {
        id: TileId::Stream(session.key.clone()),
        session: session.clone(),
    });
    let remote = cameras.remote.iter().map(|(user, session)| Tile {
        id: TileId::Camera(*user),
        session: session.clone(),
    });
    let preview = cameras
        .preview
        .iter()
        .filter(|_| self_view)
        .map(|session| Tile {
            id: TileId::Preview,
            session: session.clone(),
        });
    streams.chain(remote).chain(preview).collect()
}

pub fn ids(tiles: &[Tile]) -> Vec<TileId> {
    tiles.iter().map(|tile| tile.id.clone()).collect()
}

/// The tiles to draw.
#[derive(Debug, PartialEq, Eq)]
pub struct Plan {
    /// The large tile of Focus, Speaker and the full view.
    pub main: Option<TileId>,
    /// Every drawn tile in order (the main one first).
    pub shown: Vec<(TileId, TileSize)>,
    /// Videos that exist but are over the cap and so neither decoded nor drawn.
    pub refused: usize,
}

impl Plan {
    pub fn contains(&self, id: &TileId) -> bool {
        self.shown.iter().any(|(shown, _)| shown == id)
    }

    /// Camera users drawn small, whose lowest layer is wanted.
    pub fn small_cameras(&self) -> Vec<Snowflake> {
        self.shown
            .iter()
            .filter(|(_, size)| *size == TileSize::Small)
            .filter_map(|(id, _)| id.camera_user())
            .collect()
    }

    /// The text that says why videos are missing, if some are.
    pub fn refusal(&self) -> Option<String> {
        (self.refused > 0).then(|| {
            format!(
                "{} more video{} not shown: at most {MAX_VISIBLE_TILES} are decoded and drawn at once. Pin one, or stop watching a stream, to choose.",
                self.refused,
                if self.refused == 1 { " is" } else { "s are" },
            )
        })
    }
}

impl CallView {
    /// Forgets a pin or full view whose video is gone, and closes the call
    /// view when no video is left.
    pub fn prune(&mut self, available: &[TileId]) {
        for slot in [&mut self.pinned, &mut self.full] {
            if slot.as_ref().is_some_and(|id| !available.contains(id)) {
                *slot = None;
            }
        }
        if available.is_empty() {
            self.open = false;
        }
    }

    /// Pins a tile (it becomes the large one, so a grid turns into focus) or
    /// unpins it.
    pub fn toggle_pin(&mut self, id: TileId) {
        if self.pinned.as_ref() == Some(&id) {
            self.pinned = None;
        } else {
            self.pinned = Some(id);
            if self.mode == Mode::Grid {
                self.mode = Mode::Focus;
            }
        }
    }

    /// Chooses the tiles. `available` lists every video in order (streams,
    /// then cameras, then the self-view); `speaking` the users speaking now.
    pub fn plan(&self, available: &[TileId], speaking: &[Snowflake]) -> Plan {
        let present = |id: &TileId| available.contains(id);
        if !self.open {
            // The compact strip: cameras only, all small. Streams wait for the call view.
            let cameras: Vec<&TileId> = available
                .iter()
                .filter(|id| !matches!(id, TileId::Stream(_)))
                .collect();
            let shown: Vec<_> = cameras
                .iter()
                .take(MAX_VISIBLE_TILES)
                .map(|id| ((*id).clone(), TileSize::Small))
                .collect();
            return Plan {
                main: None,
                refused: cameras.len() - shown.len(),
                shown,
            };
        }
        if let Some(full) = self.full.as_ref().filter(|id| present(id)) {
            return Plan {
                main: Some(full.clone()),
                shown: vec![(full.clone(), TileSize::Large)],
                refused: 0,
            };
        }
        let pinned = self.pinned.as_ref().filter(|id| present(id));
        let main = match self.mode {
            Mode::Grid => None,
            Mode::Focus => pinned.or_else(|| available.first()),
            Mode::Speaker => pinned
                .or_else(|| self.follow_speaker(available, speaking))
                .or_else(|| available.first()),
        };
        // The main tile, then a pinned one, then the rest: the cap drops the last.
        let mut order: Vec<&TileId> = Vec::with_capacity(available.len());
        order.extend(main);
        order.extend(pinned.filter(|id| Some(*id) != main));
        order.extend(
            available
                .iter()
                .filter(|id| Some(*id) != main && Some(*id) != pinned),
        );
        order.truncate(MAX_VISIBLE_TILES);
        let large_grid = order.len() <= GRID_LARGE_TILES;
        let shown = order
            .iter()
            .map(|id| {
                let large = if main.is_some() {
                    Some(*id) == main
                } else {
                    large_grid
                };
                (
                    (*id).clone(),
                    if large {
                        TileSize::Large
                    } else {
                        TileSize::Small
                    },
                )
            })
            .collect::<Vec<_>>();
        Plan {
            main: main.cloned(),
            refused: available.len() - shown.len(),
            shown,
        }
    }

    /// The camera tile of whoever speaks, else of whoever spoke last.
    fn follow_speaker<'a>(
        &self,
        available: &'a [TileId],
        speaking: &[Snowflake],
    ) -> Option<&'a TileId> {
        let camera_of = |user: Snowflake| available.iter().find(|id| **id == TileId::Camera(user));
        if let Some(found) = speaking.iter().find_map(|user| camera_of(*user)) {
            self.speaker.set(found.camera_user());
            return Some(found);
        }
        self.speaker.get().and_then(camera_of)
    }
}

#[cfg(test)]
mod tests;
