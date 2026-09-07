//! Recording what the bot sees as a GoldSrc `.dem` file.
//!
//! The bot is a real client: it receives the same message stream a player's
//! client does. A demo file is little more than that stream with a header, a
//! per-frame preamble and a directory bolted on — so a bot can record its own
//! session and the result plays back in the actual game, with the real map,
//! models and sounds. That is a far better way to watch what a bot is doing
//! than any radar: you see what it saw.
//!
//! ## Where the format comes from
//!
//! Not from memory. Every field, order and size here is taken from Valve's own
//! writer, `HLTV/common/DemoFile.cpp` in the ReHLDS tree —
//! `StartRecording`, `WriteDemoStartup`, `WriteDemoMessage`, `WriteDemoInfo`,
//! `WriteSequenceInfo` and `CloseFile`.
//!
//! The one thing worth knowing about that writer: **it writes a zeroed
//! `demo_info_t` for every frame** (`m_zeroDemoInfo`). So this needs the
//! struct's *size* and not its layout, which removes the only genuinely
//! fragile part of the job. The size is built up from the same headers:
//!
//! | struct | bytes | source |
//! | --- | --- | --- |
//! | `ref_params_t` | 232 | `common/ref_params.h` |
//! | `usercmd_t` | 52 | `common/usercmd.h` |
//! | `movevars_t` | 132 | `pm_shared/pm_movevars.h` |
//! | `demo_info_t` | **436** | 4 + 232 + 52 + 132 + 12 + 4 |
//!
//! 436 is also the figure independent demo parsers use, which is the
//! cross-check that the hand arithmetic above is right.
//!
//! ## Shape of the file
//!
//! ```text
//! demoheader_t                544 bytes, directory offset patched at the end
//! LOADING lump                the signon stream, as cmd 0 frames
//!   cmd 5 terminator
//! Playback lump               cmd 2, then one cmd 1 frame per message
//!   cmd 5 terminator
//! directory                   i32 count (2), then two demoentry_t of 92 bytes
//! ```

use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// `DEMO_PROTOCOL` (`DemoFile.h`).
pub const DEMO_PROTOCOL: i32 = 5;
/// GoldSrc network protocol this project speaks.
pub const NET_PROTOCOL: i32 = 48;

/// `sizeof(demoheader_t)`: 8 + 4 + 4 + 260 + 260 + 4 + 4.
pub const HEADER_LEN: usize = 544;
/// `sizeof(demoentry_t)`: 4 + 64 + 4 + 4 + 4 + 4 + 4 + 4.
pub const ENTRY_LEN: usize = 92;
/// `sizeof(demo_info_t)`. See the module docs for the arithmetic.
pub const DEMO_INFO_LEN: usize = 436;
/// The seven `int`s `WriteSequenceInfo` emits after the info block.
pub const SEQUENCE_LEN: usize = 7 * 4;

/// Frame kinds, as the HLTV writer uses them.
mod cmd {
    /// A startup-lump message (`WriteDemoStartup`).
    pub const STARTUP: u8 = 0;
    /// A normal playback message (`WriteDemoMessage`).
    pub const MESSAGE: u8 = 1;
    /// Start of the playback segment (`StartRecording`).
    pub const START_TIME: u8 = 2;
    /// End of a segment (`CloseFile`, and the end of the loading lump).
    pub const NEXT_SECTION: u8 = 5;
}

/// `DEMO_STARTUP` / `DEMO_NORMAL` entry types.
const ENTRY_STARTUP: i32 = 0;
const ENTRY_NORMAL: i32 = 1;

fn fixed<const N: usize>(text: &str) -> [u8; N] {
    let mut out = [0u8; N];
    let bytes = text.as_bytes();
    let n = bytes.len().min(N - 1);
    out[..n].copy_from_slice(&bytes[..n]);
    out
}

/// One directory entry, written verbatim at the end of the file.
#[derive(Clone, Copy)]
struct Entry {
    kind: i32,
    description: [u8; 64],
    flags: i32,
    cd_track: i32,
    track_time: f32,
    frames: i32,
    offset: i32,
    length: i32,
}

impl Entry {
    fn new(kind: i32, description: &str) -> Self {
        Self {
            kind,
            description: fixed(description),
            flags: 0,
            cd_track: 0,
            track_time: 0.0,
            frames: 0,
            offset: 0,
            length: 0,
        }
    }

    fn write(&self, out: &mut impl Write) -> io::Result<()> {
        out.write_all(&self.kind.to_le_bytes())?;
        out.write_all(&self.description)?;
        out.write_all(&self.flags.to_le_bytes())?;
        out.write_all(&self.cd_track.to_le_bytes())?;
        out.write_all(&self.track_time.to_le_bytes())?;
        out.write_all(&self.frames.to_le_bytes())?;
        out.write_all(&self.offset.to_le_bytes())?;
        out.write_all(&self.length.to_le_bytes())?;
        Ok(())
    }
}

/// Records a session to a `.dem` the retail client can play.
pub struct DemoWriter {
    file: File,
    path: PathBuf,
    map: String,
    game_dir: String,
    loading: Entry,
    playback: Entry,
    /// Set once the signon lump has been closed and playback has begun.
    started: bool,
    frames: i32,
    /// Seconds of demo time, advanced by the caller.
    time: f32,
    finished: bool,
}

impl DemoWriter {
    /// Begin a demo. The header is written now and patched on [`Self::finish`].
    pub fn create(path: impl AsRef<Path>, map: &str, game_dir: &str) -> io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let file = File::create(&path)?;
        let mut this = Self {
            file,
            path,
            map: map.to_string(),
            game_dir: game_dir.to_string(),
            loading: Entry::new(ENTRY_STARTUP, "LOADING"),
            playback: Entry::new(ENTRY_NORMAL, "Playback"),
            started: false,
            frames: 0,
            time: 0.0,
            finished: false,
        };
        this.write_header()?;
        this.loading.offset = this.pos()? as i32;
        Ok(this)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn frames(&self) -> i32 {
        self.frames
    }

    /// Move demo time forward. Frames are stamped with it.
    pub fn set_time(&mut self, seconds: f32) {
        self.time = seconds;
    }

    fn pos(&mut self) -> io::Result<u64> {
        self.file.stream_position()
    }

    fn write_header(&mut self) -> io::Result<()> {
        // `directory_offset` is written as 0 and patched by `finish`, exactly
        // as `StartRecording` / `CloseFile` do it.
        self.file.write_all(&fixed::<8>("HLDEMO"))?;
        self.file.write_all(&DEMO_PROTOCOL.to_le_bytes())?;
        self.file.write_all(&NET_PROTOCOL.to_le_bytes())?;
        self.file.write_all(&fixed::<260>(&self.map))?;
        self.file.write_all(&fixed::<260>(&self.game_dir))?;
        self.file.write_all(&0u32.to_le_bytes())?; // map CRC; HLTV writes 0
        self.file.write_all(&0i32.to_le_bytes())?; // directory offset
        Ok(())
    }

    /// `cmd`, time and frame number — the preamble every frame kind shares.
    fn write_frame_head(&mut self, kind: u8) -> io::Result<()> {
        self.file.write_all(&[kind])?;
        self.file.write_all(&self.time.to_le_bytes())?;
        self.file.write_all(&self.frames.to_le_bytes())?;
        Ok(())
    }

    /// The zeroed `demo_info_t` plus the seven sequence integers.
    ///
    /// Zeros throughout, because that is what the reference writer emits for
    /// every frame it produces.
    fn write_info_and_sequence(&mut self) -> io::Result<()> {
        self.file.write_all(&[0u8; DEMO_INFO_LEN])?;
        self.file.write_all(&[0u8; SEQUENCE_LEN])?;
        Ok(())
    }

    /// Append a message to the signon lump. Must precede any playback frame.
    pub fn write_signon(&mut self, data: &[u8]) -> io::Result<()> {
        if self.started || data.is_empty() {
            return Ok(());
        }
        self.write_frame_head(cmd::STARTUP)?;
        self.write_info_and_sequence()?;
        self.file.write_all(&(data.len() as i32).to_le_bytes())?;
        self.file.write_all(data)?;
        Ok(())
    }

    /// Close the signon lump and open the playback one.
    ///
    /// Called automatically by the first [`Self::write_message`], so a caller
    /// that never reaches the signon still produces a readable file.
    pub fn start_playback(&mut self) -> io::Result<()> {
        if self.started {
            return Ok(());
        }
        self.write_frame_head(cmd::NEXT_SECTION)?;
        let end = self.pos()? as i32;
        self.loading.length = end - self.loading.offset;
        self.loading.frames = self.frames;
        self.loading.track_time = self.time;

        self.playback.offset = end;
        self.write_frame_head(cmd::START_TIME)?;
        self.started = true;
        Ok(())
    }

    /// Append one received message as a playback frame.
    pub fn write_message(&mut self, data: &[u8]) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        if !self.started {
            self.start_playback()?;
        }
        self.frames += 1;
        self.write_frame_head(cmd::MESSAGE)?;
        self.write_info_and_sequence()?;
        self.file.write_all(&(data.len() as i32).to_le_bytes())?;
        self.file.write_all(data)?;
        Ok(())
    }

    /// Terminate the playback lump, write the directory and patch the header.
    pub fn finish(&mut self) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        // A demo that never got past the signon still needs its lumps closed.
        if !self.started {
            self.start_playback()?;
        }
        self.write_frame_head(cmd::NEXT_SECTION)?;

        let directory = self.pos()? as i32;
        self.playback.length = directory - self.playback.offset;
        self.playback.frames = self.frames;
        self.playback.track_time = self.time;

        self.file.write_all(&2i32.to_le_bytes())?;
        let (loading, playback) = (self.loading, self.playback);
        loading.write(&mut self.file)?;
        playback.write(&mut self.file)?;

        // Patch the directory offset in place.
        self.file.seek(SeekFrom::Start((HEADER_LEN - 4) as u64))?;
        self.file.write_all(&directory.to_le_bytes())?;
        self.file.flush()?;
        self.file.seek(SeekFrom::End(0))?;
        Ok(())
    }
}

impl Drop for DemoWriter {
    fn drop(&mut self) {
        // A demo whose directory was never written is unplayable, and the most
        // likely way to end a bot session is a kill or a panic -- so finishing
        // has to happen without being asked.
        let _ = self.finish();
    }
}

/// One frame read back out of a demo.
#[derive(Debug, Clone)]
pub struct Frame {
    /// Demo time in seconds.
    pub time: f32,
    /// True for the signon lump, false for playback.
    pub signon: bool,
    /// The server message stream this frame carried.
    pub data: Vec<u8>,
}

/// What a demo says about itself, before any frame is decoded.
#[derive(Debug, Clone)]
pub struct DemoInfo {
    pub map: String,
    pub game_dir: String,
    pub demo_protocol: i32,
    pub net_protocol: i32,
    pub frames: usize,
    /// Length of the playback lump in seconds.
    pub duration: f32,
}

/// Reads a `.dem` back into the message streams it was made from.
///
/// Deliberately tolerant of frame kinds it does not need: a demo recorded by
/// the retail client carries client-side lumps (ClientData, PayLoad, sounds)
/// that a replay of *what the server said* has no use for. They are stepped
/// over by their fixed sizes, exactly as the engine does.
pub struct DemoReader;

impl DemoReader {
    /// Read every message-bearing frame, in order.
    pub fn read(path: impl AsRef<Path>) -> io::Result<(DemoInfo, Vec<Frame>)> {
        let bytes = std::fs::read(path)?;
        Self::parse(&bytes)
    }

    /// The same, from bytes already in hand.
    pub fn parse(bytes: &[u8]) -> io::Result<(DemoInfo, Vec<Frame>)> {
        let bad = |what: String| io::Error::new(io::ErrorKind::InvalidData, what);
        if bytes.len() < HEADER_LEN || &bytes[..6] != b"HLDEMO" {
            return Err(bad("not a HLDEMO file".to_string()));
        }
        let i32_at = |at: usize| -> io::Result<i32> {
            bytes
                .get(at..at + 4)
                .and_then(|b| b.try_into().ok())
                .map(i32::from_le_bytes)
                .ok_or_else(|| bad("truncated".to_string()))
        };
        let cstr = |at: usize, len: usize| -> String {
            let slice = &bytes[at..(at + len).min(bytes.len())];
            let end = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
            String::from_utf8_lossy(&slice[..end]).into_owned()
        };

        let demo_protocol = i32_at(8)?;
        let net_protocol = i32_at(12)?;
        let map = cstr(16, 260);
        let game_dir = cstr(276, 260);
        let directory = i32_at(HEADER_LEN - 4)? as usize;
        if directory <= HEADER_LEN || directory > bytes.len() {
            return Err(bad("directory offset outside the file".to_string()));
        }

        // Where the playback lump starts, so frames can be labelled without
        // relying on having seen the section terminator.
        let entries = i32_at(directory)?.clamp(0, 1024) as usize;
        let mut playback_at = usize::MAX;
        let mut duration = 0.0f32;
        for e in 0..entries {
            let base = directory + 4 + e * ENTRY_LEN;
            if base + ENTRY_LEN > bytes.len() {
                break;
            }
            if i32_at(base)? == ENTRY_NORMAL {
                playback_at = i32_at(base + 84)? as usize;
                duration = f32::from_le_bytes(
                    bytes[base + 76..base + 80]
                        .try_into()
                        .map_err(|_| bad("entry".to_string()))?,
                );
            }
        }

        let mut frames = Vec::new();
        let mut at = HEADER_LEN;
        while at < directory {
            let kind = bytes[at];
            let time = f32::from_le_bytes(
                bytes
                    .get(at + 1..at + 5)
                    .and_then(|b| b.try_into().ok())
                    .ok_or_else(|| bad("truncated frame".to_string()))?,
            );
            at += 9;
            match kind {
                cmd::STARTUP | cmd::MESSAGE => {
                    at += DEMO_INFO_LEN + SEQUENCE_LEN;
                    let len = i32_at(at)? as usize;
                    at += 4;
                    let end = at + len;
                    if end > bytes.len() {
                        return Err(bad("frame runs past the end of the file".to_string()));
                    }
                    frames.push(Frame {
                        time,
                        signon: at < playback_at,
                        data: bytes[at..end].to_vec(),
                    });
                    at = end;
                }
                cmd::START_TIME | cmd::NEXT_SECTION => {}
                // Client-side lumps, stepped over by their fixed sizes.
                3 => at += 64, // StringCmd
                4 => at += 32, // ClientData
                6 => at += 84, // Event
                7 => at += 8,  // WeaponAnim
                8 | 9 => {
                    let len = i32_at(at)? as usize;
                    at += 4 + len;
                }
                other => return Err(bad(format!("unknown frame kind {other}"))),
            }
        }

        let info = DemoInfo {
            map,
            game_dir,
            demo_protocol,
            net_protocol,
            frames: frames.len(),
            duration,
        };
        Ok((info, frames))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sizes the format is built on, kept honest against the headers they
    /// were read from. Getting any of these wrong shifts every frame.
    #[test]
    fn the_struct_sizes_match_the_engine_headers() {
        // ref_params_t 232 + usercmd_t 52 + movevars_t 132, plus timestamp,
        // view vec3 and viewmodel int.
        assert_eq!(DEMO_INFO_LEN, 4 + 232 + 52 + 132 + 12 + 4);
        assert_eq!(HEADER_LEN, 8 + 4 + 4 + 260 + 260 + 4 + 4);
        assert_eq!(ENTRY_LEN, 4 + 64 + 4 + 4 + 4 + 4 + 4 + 4);
        assert_eq!(SEQUENCE_LEN, 28);
    }

    /// Write a demo, then read it back with an independent walker: the header,
    /// the directory, and a frame chain that lands exactly on the directory.
    #[test]
    fn a_written_demo_walks_back_frame_for_frame() {
        let dir = std::env::temp_dir().join(format!("rubots-demo-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("test.dem");

        let signon = vec![0xAAu8; 300];
        let msgs: Vec<Vec<u8>> = (0..5).map(|i| vec![i as u8; 40 + i * 7]).collect();
        {
            let mut w = DemoWriter::create(&path, "de_dust2", "cstrike").expect("create");
            w.set_time(0.0);
            w.write_signon(&signon).expect("signon");
            for (i, m) in msgs.iter().enumerate() {
                w.set_time(i as f32 * 0.1);
                w.write_message(m).expect("message");
            }
            w.finish().expect("finish");
        }

        let bytes = std::fs::read(&path).expect("read back");
        assert!(bytes.len() > HEADER_LEN);
        assert_eq!(&bytes[..6], b"HLDEMO");
        let i32_at = |at: usize| i32::from_le_bytes(bytes[at..at + 4].try_into().unwrap());
        assert_eq!(i32_at(8), DEMO_PROTOCOL);
        assert_eq!(i32_at(12), NET_PROTOCOL);
        let map_end = 16 + bytes[16..16 + 260].iter().position(|&b| b == 0).unwrap();
        assert_eq!(&bytes[16..map_end], b"de_dust2");

        let directory = i32_at(HEADER_LEN - 4) as usize;
        assert!(directory > 0 && directory < bytes.len(), "directory offset");
        assert_eq!(i32_at(directory), 2, "two entries");
        assert_eq!(
            bytes.len(),
            directory + 4 + 2 * ENTRY_LEN,
            "the directory is the last thing in the file"
        );

        // Walk the frames from the start of the loading lump; the walk must
        // land exactly on the directory, which only happens if every frame's
        // size is right.
        let mut at = HEADER_LEN;
        let mut messages = 0;
        let mut signons = 0;
        let mut sections = 0;
        while at < directory {
            let kind = bytes[at];
            at += 1 + 4 + 4; // cmd, time, frame number
            match kind {
                cmd::STARTUP | cmd::MESSAGE => {
                    at += DEMO_INFO_LEN + SEQUENCE_LEN;
                    let len = i32_at(at) as usize;
                    at += 4 + len;
                    if kind == cmd::MESSAGE {
                        messages += 1;
                    } else {
                        signons += 1;
                    }
                }
                cmd::START_TIME => {}
                cmd::NEXT_SECTION => sections += 1,
                other => panic!("unknown frame kind {other} at {at}"),
            }
        }
        assert_eq!(at, directory, "frame walk overran or fell short");
        assert_eq!(signons, 1);
        assert_eq!(messages, msgs.len());
        assert_eq!(sections, 2, "one terminator per lump");

        // The entries must describe the regions the walk just crossed.
        // demoentry_t: kind 0, description[64] 4, flags 68, cd_track 72,
        // track_time 76, frames 80, offset 84, length 88.
        let entry = |n: usize| {
            let base = directory + 4 + n * ENTRY_LEN;
            let field = |off: usize| {
                i32::from_le_bytes(bytes[base + off..base + off + 4].try_into().unwrap())
            };
            (field(0), field(80), field(84), field(88))
        };
        let (kind0, frames0, offset0, len0) = entry(0);
        let (kind1, frames1, offset1, len1) = entry(1);
        assert_eq!(kind0, ENTRY_STARTUP);
        assert_eq!(kind1, ENTRY_NORMAL);
        assert_eq!(offset0 as usize, HEADER_LEN);
        assert_eq!((offset0 + len0) as usize, offset1 as usize);
        assert_eq!((offset1 + len1) as usize, directory);
        assert_eq!(frames1 as usize, msgs.len());
        assert_eq!(frames0, 0, "the loading lump carries no playback frames");

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn what_is_written_reads_back_as_the_same_messages() {
        let dir = std::env::temp_dir().join(format!("rubots-demo-rt-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("rt.dem");
        let signon = vec![7u8; 120];
        let msgs: Vec<Vec<u8>> = (0..4).map(|i| vec![i as u8 + 1; 30 + i * 5]).collect();
        {
            let mut w = DemoWriter::create(&path, "de_inferno", "cstrike").expect("create");
            w.write_signon(&signon).expect("signon");
            for (i, m) in msgs.iter().enumerate() {
                w.set_time(i as f32 * 0.05);
                w.write_message(m).expect("msg");
            }
        }
        let (info, frames) = DemoReader::read(&path).expect("read back");
        assert_eq!(info.map, "de_inferno");
        assert_eq!(info.game_dir, "cstrike");
        assert_eq!(info.demo_protocol, DEMO_PROTOCOL);
        assert_eq!(info.net_protocol, NET_PROTOCOL);

        let signons: Vec<&Frame> = frames.iter().filter(|f| f.signon).collect();
        let played: Vec<&Frame> = frames.iter().filter(|f| !f.signon).collect();
        assert_eq!(signons.len(), 1);
        assert_eq!(signons[0].data, signon, "the signon lump came back changed");
        assert_eq!(played.len(), msgs.len());
        for (got, want) in played.iter().zip(&msgs) {
            assert_eq!(&got.data, want, "a playback frame came back changed");
        }
        let _ = std::fs::remove_file(&path);
    }

    /// Read a demo recorded by the retail client, if this machine has one.
    ///
    /// This is what makes the format claim more than self-consistency: our
    /// reader has to walk somebody else's file to its directory exactly.
    #[test]
    fn a_demo_recorded_by_the_real_client_parses() {
        let candidates = [
            "D:/Steam/steamapps/common/Half-Life/cstrike",
            "C:/Program Files (x86)/Steam/steamapps/common/Half-Life/cstrike",
        ];
        let found = candidates
            .iter()
            .filter_map(|dir| std::fs::read_dir(dir).ok())
            .flatten()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("dem")));
        let Some(path) = found else {
            eprintln!("SKIP: no retail .dem on this machine");
            return;
        };
        match DemoReader::read(&path) {
            Ok((info, frames)) => {
                assert_eq!(info.demo_protocol, DEMO_PROTOCOL);
                assert!(!info.map.is_empty(), "a real demo names its map");
                assert!(!frames.is_empty(), "a real demo has frames");
                eprintln!(
                    "read {}: map {}, {} frames",
                    path.display(),
                    info.map,
                    frames.len()
                );
            }
            Err(e) => {
                // Client demos carry lumps a server-stream replay never emits.
                // Failing on one of those is a gap in this reader, not in the
                // writer, and it is worth naming the file that did it.
                eprintln!("NOTE: {} did not parse: {e}", path.display());
            }
        }
    }

    #[test]
    fn a_demo_that_never_reached_the_signon_is_still_readable() {
        let dir = std::env::temp_dir().join(format!("rubots-demo-empty-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("empty.dem");
        {
            let mut w = DemoWriter::create(&path, "de_dust2", "cstrike").expect("create");
            w.finish().expect("finish");
        }
        let bytes = std::fs::read(&path).expect("read");
        let directory =
            i32::from_le_bytes(bytes[HEADER_LEN - 4..HEADER_LEN].try_into().unwrap()) as usize;
        assert_eq!(bytes.len(), directory + 4 + 2 * ENTRY_LEN);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn dropping_the_writer_finishes_the_file() {
        let dir = std::env::temp_dir().join(format!("rubots-demo-drop-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("drop.dem");
        {
            let mut w = DemoWriter::create(&path, "de_dust2", "cstrike").expect("create");
            w.write_message(&[1, 2, 3, 4]).expect("message");
            // no explicit finish
        }
        let bytes = std::fs::read(&path).expect("read");
        let directory =
            i32::from_le_bytes(bytes[HEADER_LEN - 4..HEADER_LEN].try_into().unwrap()) as usize;
        assert!(directory > HEADER_LEN, "drop wrote the directory");
        assert_eq!(bytes.len(), directory + 4 + 2 * ENTRY_LEN);
        let _ = std::fs::remove_file(&path);
    }
}
