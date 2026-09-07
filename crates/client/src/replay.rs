//! Turning a recorded `.dem` back into positions on a map.
//!
//! [`crate::demo`] gets the bytes out of the file; this decodes them with the
//! same machinery the live bot uses — [`crate::signon::walk`] to learn the
//! delta tables from the loading lump, then [`crate::world::Decoder`] over the
//! playback frames — and reduces each frame to "who was where, facing where".
//!
//! That is what a replay needs and it is all it needs: the app draws the map
//! from the nav grid already, so a recording only has to supply the moving
//! parts. Keeping it to positions also means a twenty-minute session is a few
//! hundred kilobytes in memory rather than the four megabytes the demo itself
//! occupies.

use crate::demo::{DemoInfo, DemoReader};

/// One player, at one instant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Actor {
    /// Entity number: player slot + 1.
    pub entity: u16,
    pub origin: [f32; 3],
    pub angles: [f32; 3],
    /// 1 = T, 2 = CT, 0 = unknown at that point in the recording.
    pub team: u8,
}

/// Everything a replay knows about one instant.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    /// Seconds from the start of the playback lump.
    pub time: f32,
    pub actors: Vec<Actor>,
}

/// A decoded recording, ready to scrub through.
#[derive(Debug, Clone)]
pub struct Replay {
    pub map: String,
    pub duration: f32,
    pub snapshots: Vec<Snapshot>,
    /// Frames whose entity block failed to decode. Non-zero means the replay
    /// is incomplete rather than wrong, and it is worth showing.
    pub undecoded: usize,
}

impl Replay {
    /// Decode a `.dem` into a timeline.
    pub fn load(path: impl AsRef<std::path::Path>) -> std::io::Result<Self> {
        let (info, frames) = DemoReader::read(path)?;
        Ok(Self::from_frames(&info, &frames))
    }

    fn from_frames(info: &DemoInfo, frames: &[crate::demo::Frame]) -> Self {
        // The loading lump teaches the delta tables. Without them nothing in
        // the playback lump can be decoded at all, which is why the recorder
        // takes care to capture the signon burst.
        let mut signon = crate::signon::Signon::default();
        for frame in frames.iter().filter(|f| f.signon) {
            let walked = crate::signon::walk(&frame.data);
            if walked.registry.len() > signon.registry.len() {
                signon = walked;
            }
        }

        let mut decoder =
            crate::world::Decoder::new(&signon, crate::stream::UserMsgTable::default());
        let mut snapshots = Vec::new();
        let mut undecoded = 0;
        let start = frames.iter().find(|f| !f.signon).map(|f| f.time);

        for frame in frames.iter().filter(|f| !f.signon) {
            let before = decoder.stats.entity_errors;
            decoder.feed(&frame.data);
            if decoder.stats.entity_errors > before {
                undecoded += 1;
            }
            let actors: Vec<Actor> = decoder
                .players()
                .into_iter()
                .map(|p| Actor {
                    entity: p.entity,
                    origin: p.origin,
                    angles: p.angles,
                    team: p.team as u8,
                })
                .collect();
            if actors.is_empty() {
                continue;
            }
            snapshots.push(Snapshot {
                time: frame.time - start.unwrap_or(0.0),
                actors,
            });
        }

        Self {
            map: info.map.clone(),
            duration: snapshots.last().map_or(info.duration, |s| s.time),
            snapshots,
            undecoded,
        }
    }

    /// The snapshot at or before `time`, for a scrubber.
    pub fn at(&self, time: f32) -> Option<&Snapshot> {
        if self.snapshots.is_empty() {
            return None;
        }
        let idx = match self
            .snapshots
            .binary_search_by(|s| s.time.total_cmp(&time))
        {
            Ok(i) => i,
            Err(0) => 0,
            Err(i) => i - 1,
        };
        self.snapshots.get(idx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_replay_answers_nothing_rather_than_panicking() {
        let r = Replay {
            map: "de_dust2".into(),
            duration: 0.0,
            snapshots: Vec::new(),
            undecoded: 0,
        };
        assert!(r.at(0.0).is_none());
        assert!(r.at(99.0).is_none());
    }

    #[test]
    fn scrubbing_lands_on_the_snapshot_at_or_before_the_time() {
        let snap = |t: f32| Snapshot {
            time: t,
            actors: vec![Actor {
                entity: 1,
                origin: [t, 0.0, 0.0],
                angles: [0.0; 3],
                team: 1,
            }],
        };
        let r = Replay {
            map: "de_dust2".into(),
            duration: 3.0,
            snapshots: vec![snap(0.0), snap(1.0), snap(2.0), snap(3.0)],
            undecoded: 0,
        };
        assert_eq!(r.at(-1.0).map(|s| s.time), Some(0.0), "before the start");
        assert_eq!(r.at(1.0).map(|s| s.time), Some(1.0), "exactly on one");
        assert_eq!(r.at(1.7).map(|s| s.time), Some(1.0), "between two");
        assert_eq!(r.at(99.0).map(|s| s.time), Some(3.0), "past the end");
    }
}
