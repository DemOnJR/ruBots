//! GoldSrc netchannel: packet framing, split reassembly and payload
//! decompression.
//!
//! Port of `internal/netchan/netchan.go`. Verified from the disassembly:
//!
//! * `Process` (`0x1406E9040`) requires 8 header bytes (`cmp rdx, 8`), treats a
//!   leading `-1` long as connectionless (`cmp dword [rcx], -1`), masks both
//!   longs with `0x3FFFFFFF` and takes bit 31 as a reliable flag (`shr esi, 0x1f`)
//! * `maybeDecompress` (`0x1406E8340`) tests a four byte `BZ2\0` magic
//!   (`cmp byte [rax], 0x42` / `0x5a` / `0x32` / `0`) and runs the remainder
//!   through bzip2
//! * `splitReassembler::feed` (`0x1406E8480`) recognises split packets by a
//!   leading `-2` long (`cmp dword [rbx], -2`), then a sequence long, then one
//!   packed byte: `& 0x0F` is the fragment count, `>> 4` is this fragment's
//!   index

use std::collections::HashMap;

/// First long of a connectionless datagram.
pub const CONNECTIONLESS: i32 = -1;
/// First long of a split (fragmented) datagram.
pub const SPLIT: i32 = -2;

/// `-2` long + sequence long + one packed count/index byte.
pub const SPLIT_HEADER_LEN: usize = 9;
/// Sequence + acknowledgement longs.
pub const NET_HEADER_LEN: usize = 8;

/// Magic prefixing a bzip2-compressed payload.
pub const BZ2_MAGIC: [u8; 4] = [b'B', b'Z', b'2', 0];

/// Low 30 bits of each header long carry the sequence number.
pub const SEQUENCE_MASK: u32 = 0x3FFF_FFFF;
/// Bit 31 of the sequence long: this packet carries reliable data.
pub const RELIABLE_FLAG: u32 = 1 << 31;
/// The netchannel body is munged with only the **low byte** of the sequence.
///
/// ReHLDS `Netchan_Process`:
/// `COM_UnMunge2(&net_message.data[8], net_message.cursize - 8, sequence & 0xFF)`.
///
/// Proven on 24,390 captured client packets with `sequence >= 256`, where the
/// two choices actually differ: keyed on the low byte, 99.9% of bodies parse as
/// a clean `clc_*` stream; keyed on the full sequence, only 63.1% do.
///
/// This is easy to get wrong and hard to notice. Byte 0 of every dword mixes
/// only with byte 0 of the key, so the **first byte is identical either way** —
/// a "does the body start with a valid opcode?" check passes at 100% even when
/// the other three bytes of every dword are wrong. The damage only begins at
/// sequence 256, i.e. a few seconds into a connection, which presents as the
/// server quietly ceasing to see the client's acknowledgements.
///
/// Note the asymmetry: the `clc_move` **payload** munge (table 1) uses the
/// **full** sequence — verified 100% against 11,693 real packets. Only the
/// netchannel body masks.
pub const MUNGE_SEQUENCE_MASK: u32 = 0xFF;

/// Bit 30 of the sequence long: this packet carries a fragment.
///
/// The `0x3FFFFFFF` mask in the original clears this bit alongside bit 31, so
/// it is definitely a flag; that it specifically means "fragmented" is the
/// standard GoldSrc meaning rather than something confirmed instruction by
/// instruction.
pub const FRAGMENT_FLAG: u32 = 1 << 30;

fn read_i32(b: &[u8]) -> i32 {
    i32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn read_u32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// What a datagram turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Datagram<'a> {
    /// `\xff\xff\xff\xff` out-of-band packet; payload follows the header.
    Connectionless(&'a [u8]),
    /// A fragment of a larger message.
    Split(SplitHeader, &'a [u8]),
    /// An ordinary sequenced packet.
    Sequenced(NetHeader, &'a [u8]),
    /// Too short to be any of the above.
    Runt,
}

/// Parsed netchannel header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetHeader {
    pub sequence: u32,
    pub reliable: bool,
    pub fragment: bool,
    pub ack: u32,
    pub reliable_ack: bool,
}

/// Parsed split-packet header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitHeader {
    pub sequence: i32,
    pub index: u8,
    pub count: u8,
}

/// Classify a raw datagram.
pub fn classify(data: &[u8]) -> Datagram<'_> {
    if data.len() < 4 {
        return Datagram::Runt;
    }
    match read_i32(data) {
        CONNECTIONLESS => Datagram::Connectionless(&data[4..]),
        SPLIT => {
            if data.len() < SPLIT_HEADER_LEN {
                return Datagram::Runt;
            }
            let packed = data[8];
            let header = SplitHeader {
                sequence: read_i32(&data[4..]),
                index: packed >> 4,
                count: packed & 0x0F,
            };
            Datagram::Split(header, &data[SPLIT_HEADER_LEN..])
        }
        _ => {
            if data.len() < NET_HEADER_LEN {
                return Datagram::Runt;
            }
            let seq_raw = read_u32(data);
            let ack_raw = read_u32(&data[4..]);
            let header = NetHeader {
                sequence: seq_raw & SEQUENCE_MASK,
                reliable: seq_raw & RELIABLE_FLAG != 0,
                fragment: seq_raw & FRAGMENT_FLAG != 0,
                ack: ack_raw & SEQUENCE_MASK,
                reliable_ack: ack_raw & RELIABLE_FLAG != 0,
            };
            Datagram::Sequenced(header, &data[NET_HEADER_LEN..])
        }
    }
}

/// Decompress a payload if it carries the `BZ2\0` magic; otherwise return it
/// unchanged.
pub fn maybe_decompress(data: Vec<u8>) -> Result<Vec<u8>, String> {
    if data.len() < 4 || data[..4] != BZ2_MAGIC {
        return Ok(data);
    }
    use std::io::Read;
    let mut out = Vec::new();
    let mut reader = bzip2_rs::DecoderReader::new(&data[4..]);
    reader
        .read_to_end(&mut out)
        .map_err(|e| format!("bzip2 payload failed to decompress: {e}"))?;
    Ok(out)
}

/// Number of independent fragment streams (normal + file).
pub const MAX_STREAMS: usize = 2;

/// Per-stream fragment descriptor, present when the sequence carries
/// [`FRAGMENT_FLAG`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FragmentInfo {
    pub id: u32,
    pub offset: u16,
    pub size: u16,
}

impl FragmentInfo {
    /// One-based position in the sequence.
    ///
    /// Verified live: a 5-fragment signon arrived with ids `0x00010005`
    /// through `0x00050005`, i.e. `(index << 16) | total`.
    pub fn index(&self) -> u16 {
        (self.id >> 16) as u16
    }

    /// How many fragments make up the whole message.
    pub fn total(&self) -> u16 {
        (self.id & 0xFFFF) as u16
    }
}

/// Reassembles an ordinary (non-SPLIT) fragmented message.
///
/// Ordering comes from [`FragmentInfo::index`], **not** from `offset` — every
/// fragment of the observed signon reported `offset = 0`, so keying on offset
/// silently collapses them all onto each other.
#[derive(Debug, Clone, Default)]
pub struct FragmentBuffer {
    parts: std::collections::BTreeMap<u16, Vec<u8>>,
    total: Option<u16>,
}

impl FragmentBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn received(&self) -> usize {
        self.parts.len()
    }

    pub fn total(&self) -> Option<u16> {
        self.total
    }

    /// Add one fragment; returns the assembled message once all have arrived.
    pub fn push(&mut self, info: FragmentInfo, data: &[u8]) -> Option<Vec<u8>> {
        let total = info.total();
        if total == 0 || info.index() == 0 || info.index() > total {
            return None;
        }
        // A different total means a new message reused the channel.
        if self.total != Some(total) {
            self.parts.clear();
            self.total = Some(total);
        }
        self.parts.insert(info.index(), data.to_vec());

        if self.parts.len() as u16 != total {
            return None;
        }
        let out: Vec<u8> = self.parts.values().flatten().copied().collect();
        self.parts.clear();
        self.total = None;
        Some(out)
    }
}

/// Fragment headers that follow the eight-byte netchannel header.
///
/// Layout verified against a live HLDS signon packet: for each of the two
/// streams a presence byte, and when set, a `u32` id, a `u16` offset and a
/// `u16` size. The observed reply was 1042 bytes with one stream present —
/// `1042 - 8 (header) - 10 (this) = 1024`, exactly the declared size.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Fragments {
    pub streams: [Option<FragmentInfo>; MAX_STREAMS],
    /// Bytes this header occupied.
    pub len: usize,
}

impl Fragments {
    /// Parse from the start of a sequenced packet's body.
    pub fn parse(body: &[u8]) -> Option<Self> {
        let mut out = Fragments::default();
        let mut o = 0usize;
        for slot in out.streams.iter_mut() {
            let present = *body.get(o)?;
            o += 1;
            if present != 0 {
                let id = u32::from_le_bytes(body.get(o..o + 4)?.try_into().ok()?);
                let offset = u16::from_le_bytes(body.get(o + 4..o + 6)?.try_into().ok()?);
                let size = u16::from_le_bytes(body.get(o + 6..o + 8)?.try_into().ok()?);
                o += 8;
                *slot = Some(FragmentInfo { id, offset, size });
            }
        }
        out.len = o;
        Some(out)
    }

    /// Is any stream carrying a fragment?
    pub fn any(&self) -> bool {
        self.streams.iter().any(Option::is_some)
    }
}

/// Client-to-server message ids.
pub mod clc {
    pub const BAD: u8 = 0;
    pub const NOP: u8 = 1;
    pub const MOVE: u8 = 2;
    pub const STRINGCMD: u8 = 3;
    pub const DELTA: u8 = 4;
    pub const RESOURCELIST: u8 = 5;
    pub const FILECONSISTENCY: u8 = 7;
    pub const VOICEDATA: u8 = 8;
    pub const CVARVALUE: u8 = 10;
    pub const CVARVALUE2: u8 = 11;
}

/// Outgoing netchannel.
///
/// Verified from `Transmit` (`0x1406E86E0`): the eight header bytes are
/// written in the clear and everything after them is munged with **table 2**
/// (`0x1407B00D0`) keyed on the outgoing sequence — `mov esi, 8` is the offset
/// and `lea r8, [rbx - 8]` the length. The reliable bit is `or esi, 0x80000000`
/// and the alternating reliable-sequence bit is toggled with
/// `xor qword [rax + 0x28], 1`.
#[derive(Debug, Clone, Default)]
pub struct NetChannel {
    pub outgoing_sequence: u32,
    pub incoming_sequence: u32,
    pub outgoing_reliable: u32,
    pub incoming_reliable: u32,
    /// The reliable message currently in flight, retransmitted in every packet
    /// until the server acknowledges it. Empty when nothing is outstanding.
    reliable_buf: Vec<u8>,
    /// Reliable payloads waiting for the channel to free up.
    queued: Vec<Vec<u8>>,
    /// Packets rejected by the stale/duplicate check, for diagnostics.
    pub dropped_stale: u32,
    /// Datagrams that never reached us, counted from gaps in the incoming
    /// sequence (`net_drop` in ReHLDS `Netchan_Process`).
    ///
    /// This matters far more than it looks: the reliable-acknowledgement bit is
    /// toggled once per *received* reliable packet, so a datagram lost between
    /// the server and our socket costs us a toggle the server already made. We
    /// then echo the wrong bit for ever, the server stops transmitting, and its
    /// reliable buffer overflows.
    pub lost_packets: u32,
    /// Our `outgoing_sequence` at the moment the in-flight reliable was sent.
    last_reliable_sequence: u32,
    /// The peer's most recent acknowledged sequence (`ack` field).
    incoming_acknowledged: u32,
    /// The peer's most recent reliable-acknowledgement bit.
    incoming_reliable_acknowledged: u32,
    /// The fragment header to attach to whatever is currently in flight, when
    /// that in-flight payload is one fragment of a larger upload.
    frag_header: Option<(u16, u16)>,
    /// A large payload being uploaded one fragment per round trip.
    frag_out: Option<FragOut>,
}

/// A client→server upload in progress.
#[derive(Debug, Clone)]
struct FragOut {
    data: Vec<u8>,
    /// 1-based index of the fragment to send next.
    next: u16,
    total: u16,
}

/// Bytes of payload per outgoing fragment.
///
/// Verified against a real client's `clc_fileconsistency` upload: eleven
/// fragments of exactly 128 bytes followed by a 4-byte remainder
/// (`idx=1/12 … 12/12`, `len=128` then `len=4`).
pub const FRAGMENT_PAYLOAD: usize = 128;

impl NetChannel {
    pub fn new() -> Self {
        Self { outgoing_sequence: 1, ..Default::default() }
    }

    /// Hand a reliable payload to the channel.
    ///
    /// It is **not** sent immediately: GoldSrc allows only one outstanding
    /// reliable message at a time, so it waits its turn behind anything
    /// already in flight. Drive the channel with [`transmit`](Self::transmit).
    ///
    /// Sending reliables back to back without this flow control is what makes
    /// HLDS log `SV_ReadClientMessage: badread` — verified live: a run that
    /// pushed `spawn` and `begin` straight out produced 186 badreads where the
    /// identical run without them produced none.
    pub fn queue_reliable(&mut self, payload: &[u8]) {
        self.queued.push(payload.to_vec());
    }

    /// Queue a payload to be uploaded on the **fragment stream**.
    ///
    /// Some client→server messages are not accepted as plain reliable
    /// messages: a real client sends `clc_resourcelist` and
    /// `clc_fileconsistency` as fragments, and the server ignores them
    /// otherwise. (Manutza\*'s "# detect 7 … sent using fragment method".)
    ///
    /// The payload is split into [`FRAGMENT_PAYLOAD`]-byte pieces and one goes
    /// out per acknowledged round trip, exactly as observed on the wire.
    /// Fragments carry **both** the reliable and fragment flags.
    pub fn queue_fragmented(&mut self, payload: &[u8]) {
        if payload.is_empty() {
            return;
        }
        let total = payload.len().div_ceil(FRAGMENT_PAYLOAD) as u16;
        self.frag_out = Some(FragOut { data: payload.to_vec(), next: 1, total });
    }

    /// Abandon everything in flight, exactly as ReHLDS `Netchan_Clear` does
    /// (`rehlds/engine/net_chan.cpp`).
    ///
    /// The server calls it on **both** sides of a level change: `SV_ActivateServer`
    /// does `Netchan_Clear(&cl->netchan)` and then writes
    /// `svc_stufftext "reconnect"` (`sv_main.cpp:6217-6222`), and the client's
    /// `reconnect` handler does the same before writing `clc_stringcmd "new"`
    /// (`Host_Reconnect_f`, `host_cmd.cpp`). A client that skips this keeps
    /// retransmitting a reliable the server has already thrown away, and keeps a
    /// half-finished fragment upload that will be reassembled against a stream
    /// the server has reset.
    ///
    /// The reliable bit is toggled when a reliable was still in flight —
    /// `chan->reliable_sequence ^= 1` in the engine — because the peer never
    /// acknowledged it and the next promotion must not reuse the same bit.
    /// Sequence numbers are **not** reset: the engine leaves
    /// `incoming_sequence`/`outgoing_sequence` alone, and so do we.
    pub fn clear(&mut self) {
        if !self.reliable_buf.is_empty() {
            self.outgoing_reliable ^= 1;
            self.reliable_buf.clear();
        }
        self.queued.clear();
        self.frag_out = None;
        self.frag_header = None;
    }

    /// Is a fragmented upload still in progress?
    pub fn fragment_upload_active(&self) -> bool {
        self.frag_out.is_some()
    }

    /// Fragments still to send, and the total, while an upload is running.
    pub fn fragment_progress(&self) -> Option<(u16, u16)> {
        self.frag_out.as_ref().map(|f| (f.next, f.total))
    }

    /// Is a reliable message currently awaiting acknowledgement?
    pub fn reliable_in_flight(&self) -> bool {
        !self.reliable_buf.is_empty()
    }

    /// Reliable payloads still waiting to be sent.
    pub fn queued_count(&self) -> usize {
        self.queued.len()
    }

    /// Build the next outgoing packet, carrying any in-flight reliable message
    /// followed by `unreliable` (moves, nops — anything that may be dropped).
    ///
    /// Mirrors ReHLDS `Netchan_Transmit`:
    ///
    /// * if nothing is in flight and something is queued, promote it and
    ///   **toggle** `outgoing_reliable` — the toggled bit is what the server
    ///   echoes back to acknowledge it
    /// * the reliable payload rides in **every** packet until acknowledged,
    ///   which is what makes it reliable over UDP
    /// * bit 31 of the sequence long is set while a reliable is in flight
    pub fn transmit(&mut self, unreliable: &[u8]) -> Vec<u8> {
        let mut just_promoted = false;
        if self.reliable_buf.is_empty() {
            // A fragmented upload takes precedence: it is a single logical
            // message and must not be interleaved with other reliables.
            if let Some(f) = self.frag_out.as_mut() {
                let start = (f.next as usize - 1) * FRAGMENT_PAYLOAD;
                let end = (start + FRAGMENT_PAYLOAD).min(f.data.len());
                self.reliable_buf = f.data[start..end].to_vec();
                self.frag_header = Some((f.next, f.total));
                if f.next >= f.total {
                    self.frag_out = None;
                } else {
                    f.next += 1;
                }
                self.outgoing_reliable ^= 1;
                just_promoted = true;
            } else if !self.queued.is_empty() {
                self.reliable_buf = self.queued.remove(0);
                self.frag_header = None;
                self.outgoing_reliable ^= 1;
                just_promoted = true;
            }
        }
        // Whether this packet actually carries the reliable payload.
        //
        // Quake/GoldSrc `Netchan_Transmit` does NOT put the in-flight reliable
        // into every packet. It resends only when it can prove the peer missed
        // it: the peer has acknowledged a sequence *later* than the one the
        // reliable went out on, yet is still echoing the wrong reliable bit.
        //
        // Repeating it in every packet — which is what this used to do — makes
        // the server re-execute the same command dozens of times a second. Each
        // execution queues more reliable output, and HLDS ends up dropping us
        // with `WARNING: reliable overflow` / `Reliable channel overflowed`.
        let lost = self.incoming_acknowledged > self.last_reliable_sequence
            && self.incoming_reliable_acknowledged != self.outgoing_reliable;
        let send_reliable = !self.reliable_buf.is_empty() && (just_promoted || lost);
        if send_reliable {
            self.last_reliable_sequence = self.outgoing_sequence;
        }
        let frag = self.frag_header.filter(|_| send_reliable);

        // A fragment packet carries a 10-byte header before the payload: for
        // each of the two streams a presence byte, and for a present stream a
        // `(index << 16) | total` id, a `u16` offset (always 0) and a `u16`
        // length. Only stream 0 is ever used by the client.
        let mut payload = Vec::new();
        if let Some((index, total)) = frag {
            let id = (u32::from(index) << 16) | u32::from(total);
            payload.push(1u8);
            payload.extend_from_slice(&id.to_le_bytes());
            payload.extend_from_slice(&0u16.to_le_bytes());
            payload.extend_from_slice(&(self.reliable_buf.len() as u16).to_le_bytes());
            payload.push(0u8); // stream 1 absent
        }
        payload.extend_from_slice(&self.reliable_buf);
        payload.extend_from_slice(unreliable);

        let seq = self.outgoing_sequence;
        let mut w1 = seq & SEQUENCE_MASK;
        if send_reliable {
            w1 |= RELIABLE_FLAG;
        }
        if frag.is_some() {
            w1 |= FRAGMENT_FLAG;
        }
        let mut w2 = self.incoming_sequence & SEQUENCE_MASK;
        if self.incoming_reliable != 0 {
            w2 |= RELIABLE_FLAG;
        }

        let mut body = payload;
        while body.len() % 4 != 0 {
            body.push(clc::NOP);
        }
        let n = body.len();
        let munge_key = (seq & MUNGE_SEQUENCE_MASK) as i32;
        proto::munge::munge(&mut body[..n], &proto::munge::TABLE2, munge_key);

        let mut out = Vec::with_capacity(8 + body.len());
        out.extend_from_slice(&w1.to_le_bytes());
        out.extend_from_slice(&w2.to_le_bytes());
        out.extend_from_slice(&body);

        self.outgoing_sequence = self.outgoing_sequence.wrapping_add(1) & SEQUENCE_MASK;
        out
    }

    /// Build a packet carrying `payload`, munging the body as the engine does.
    ///
    /// The payload is **not** padded. Munge transforms `len & !3` bytes and
    /// leaves any partial tail alone, and the receiver does the same, so an
    /// unaligned tail round-trips untouched. Padding would be actively
    /// harmful: `clc_bad` is opcode 0, so zero padding appends bogus commands
    /// and HLDS answers with
    /// `SV_ReadClientMessage: too many cmds -2 sent for <player>` and drops
    /// the client.
    pub fn build(&mut self, payload: &[u8], reliable: bool) -> Vec<u8> {
        let seq = self.outgoing_sequence;
        let mut w1 = seq & SEQUENCE_MASK;
        if reliable {
            w1 |= RELIABLE_FLAG;
        }
        let mut w2 = self.incoming_sequence & SEQUENCE_MASK;
        if self.incoming_reliable != 0 {
            w2 |= RELIABLE_FLAG;
        }

        let mut body = payload.to_vec();
        // Pad to a whole dword with `clc_nop`, which is what real clients do
        // (verified across 24,697 captured client packets: the body is always
        // dword-aligned and fully munged, never leaving an unmunged tail).
        // The padding byte MUST be `clc_nop` (1) and never zero — `clc_bad` is
        // opcode 0, so zero padding appends bogus commands and HLDS answers
        // with `too many cmds -2` and drops the client.
        while body.len() % 4 != 0 {
            body.push(clc::NOP);
        }
        let n = body.len();
        let munge_key = (seq & MUNGE_SEQUENCE_MASK) as i32;
        proto::munge::munge(&mut body[..n], &proto::munge::TABLE2, munge_key);

        let mut out = Vec::with_capacity(8 + body.len());
        out.extend_from_slice(&w1.to_le_bytes());
        out.extend_from_slice(&w2.to_le_bytes());
        out.extend_from_slice(&body);

        self.outgoing_sequence = self.outgoing_sequence.wrapping_add(1) & SEQUENCE_MASK;
        if reliable {
            self.outgoing_reliable ^= 1;
        }
        out
    }

    /// Decode an incoming sequenced packet, unmunging the body.
    ///
    /// Stale and duplicate packets are dropped (returning `None`) *before* any
    /// state is touched, exactly as ReHLDS `Netchan_Process` does with its
    /// `sequence <= chan->incoming_sequence` check. This ordering is
    /// load-bearing: the reliable-acknowledgement bit is a **toggle**, so
    /// letting a retransmission through flips it a second time, the peer never
    /// sees the acknowledgement it is waiting for, and it retransmits until
    /// HLDS gives up with `WARNING: reliable overflow` /
    /// `Reason: Reliable channel overflowed`.
    pub fn read(&mut self, data: &[u8]) -> Option<(NetHeader, Vec<u8>)> {
        let Datagram::Sequenced(header, body) = classify(data) else {
            return None;
        };
        // Drop anything we have already seen. (The first packet of a channel
        // is allowed through: incoming_sequence starts at 0 and sequences
        // start at 1.)
        if header.sequence <= self.incoming_sequence && self.incoming_sequence != 0 {
            self.dropped_stale = self.dropped_stale.saturating_add(1);
            return None;
        }
        let mut buf = body.to_vec();
        let n = buf.len() - buf.len() % 4;
        let munge_key = (header.sequence & MUNGE_SEQUENCE_MASK) as i32;
        proto::munge::unmunge(&mut buf[..n], &proto::munge::TABLE2, munge_key);
        if self.incoming_sequence != 0 && header.sequence > self.incoming_sequence + 1 {
            self.lost_packets += header.sequence - self.incoming_sequence - 1;
        }
        self.incoming_sequence = header.sequence;
        self.incoming_acknowledged = header.ack;
        self.incoming_reliable_acknowledged = u32::from(header.reliable_ack);
        // Toggled per *packet* that carries the reliable bit, fragments
        // included -- this is what ReHLDS `Netchan_Process` does. Toggling
        // once per reassembled message instead was tried against a live
        // server and stalls the signon completely.
        if header.reliable {
            self.incoming_reliable ^= 1;
        }
        // ReHLDS `Netchan_Process`: when the peer echoes our reliable bit back
        // in its acknowledgement, the in-flight message got through and the
        // channel is free for the next one.
        if header.reliable_ack == (self.outgoing_reliable != 0) {
            self.reliable_buf.clear();
            // The fragment that was riding on it is done too; the next
            // `transmit` promotes the following one.
            self.frag_header = None;
        }
        Some((header, buf))
    }

    /// A `clc_stringcmd` payload, e.g. `new` or `sendents`.
    pub fn string_command(cmd: &str) -> Vec<u8> {
        let mut p = vec![clc::STRINGCMD];
        p.extend_from_slice(cmd.as_bytes());
        p.push(0);
        p
    }
}

/// Collects split fragments until a message is complete.
#[derive(Debug, Default)]
pub struct SplitReassembler {
    pending: HashMap<i32, Pending>,
}

#[derive(Debug)]
struct Pending {
    count: u8,
    parts: HashMap<u8, Vec<u8>>,
}

impl SplitReassembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of partially-assembled messages currently held.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Feed one fragment. Returns the reassembled payload once every fragment
    /// of that sequence has arrived.
    pub fn feed(&mut self, header: SplitHeader, body: &[u8]) -> Option<Vec<u8>> {
        if header.count == 0 || header.index >= header.count {
            return None;
        }
        let entry = self.pending.entry(header.sequence).or_insert_with(|| Pending {
            count: header.count,
            parts: HashMap::new(),
        });

        // A changed fragment count means a new message reused the sequence.
        if entry.count != header.count {
            entry.count = header.count;
            entry.parts.clear();
        }
        entry.parts.insert(header.index, body.to_vec());

        if entry.parts.len() as u8 != entry.count {
            return None;
        }
        let mut out = Vec::new();
        for i in 0..entry.count {
            out.extend_from_slice(entry.parts.get(&i)?);
        }
        self.pending.remove(&header.sequence);
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split_packet(seq: i32, index: u8, count: u8, body: &[u8]) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend_from_slice(&SPLIT.to_le_bytes());
        p.extend_from_slice(&seq.to_le_bytes());
        p.push((index << 4) | (count & 0x0F));
        p.extend_from_slice(body);
        p
    }

    #[test]
    fn connectionless_is_recognised() {
        let mut d = vec![0xFF; 4];
        d.extend_from_slice(b"getchallenge steam\n");
        match classify(&d) {
            Datagram::Connectionless(p) => assert_eq!(p, b"getchallenge steam\n"),
            other => panic!("expected connectionless, got {other:?}"),
        }
    }

    #[test]
    fn sequenced_header_splits_flags_from_sequence() {
        let seq = 0x1234u32 | RELIABLE_FLAG;
        let ack = 0x0055u32 | RELIABLE_FLAG;
        let mut d = Vec::new();
        d.extend_from_slice(&seq.to_le_bytes());
        d.extend_from_slice(&ack.to_le_bytes());
        d.extend_from_slice(b"payload");

        match classify(&d) {
            Datagram::Sequenced(h, body) => {
                assert_eq!(h.sequence, 0x1234, "flag bits must be masked off");
                assert!(h.reliable);
                assert!(!h.fragment);
                assert_eq!(h.ack, 0x0055);
                assert!(h.reliable_ack);
                assert_eq!(body, b"payload");
            }
            other => panic!("expected sequenced, got {other:?}"),
        }
    }

    #[test]
    fn fragment_flag_is_separate_from_reliable() {
        let seq = 7u32 | FRAGMENT_FLAG;
        let mut d = Vec::new();
        d.extend_from_slice(&seq.to_le_bytes());
        d.extend_from_slice(&0u32.to_le_bytes());
        match classify(&d) {
            Datagram::Sequenced(h, _) => {
                assert_eq!(h.sequence, 7);
                assert!(h.fragment);
                assert!(!h.reliable);
            }
            other => panic!("expected sequenced, got {other:?}"),
        }
    }

    #[test]
    fn split_header_unpacks_index_and_count() {
        let p = split_packet(42, 2, 5, b"xyz");
        match classify(&p) {
            Datagram::Split(h, body) => {
                assert_eq!(h.sequence, 42);
                assert_eq!(h.index, 2);
                assert_eq!(h.count, 5);
                assert_eq!(body, b"xyz");
            }
            other => panic!("expected split, got {other:?}"),
        }
    }

    #[test]
    fn runts_are_rejected_not_misparsed() {
        assert_eq!(classify(&[]), Datagram::Runt);
        assert_eq!(classify(&[1, 2, 3]), Datagram::Runt);
        // -2 with a truncated split header
        assert_eq!(classify(&[0xFE, 0xFF, 0xFF, 0xFF, 0, 0]), Datagram::Runt);
    }

    #[test]
    fn fragments_reassemble_in_index_order() {
        let mut r = SplitReassembler::new();
        // Feed out of order on purpose.
        let (h1, b1) = match classify(&split_packet(9, 1, 3, b"BBB")) {
            Datagram::Split(h, b) => (h, b.to_vec()),
            _ => unreachable!(),
        };
        assert_eq!(r.feed(h1, &b1), None);

        let p2 = split_packet(9, 2, 3, b"CCC");
        let (h2, b2) = match classify(&p2) {
            Datagram::Split(h, b) => (h, b.to_vec()),
            _ => unreachable!(),
        };
        assert_eq!(r.feed(h2, &b2), None);
        assert_eq!(r.pending_count(), 1);

        let p0 = split_packet(9, 0, 3, b"AAA");
        let (h0, b0) = match classify(&p0) {
            Datagram::Split(h, b) => (h, b.to_vec()),
            _ => unreachable!(),
        };
        assert_eq!(r.feed(h0, &b0).as_deref(), Some(&b"AAABBBCCC"[..]));
        assert_eq!(r.pending_count(), 0, "completed message must be released");
    }

    #[test]
    fn out_of_range_fragment_index_is_ignored() {
        let mut r = SplitReassembler::new();
        assert_eq!(r.feed(SplitHeader { sequence: 1, index: 5, count: 3 }, b"x"), None);
        assert_eq!(r.feed(SplitHeader { sequence: 1, index: 0, count: 0 }, b"x"), None);
        assert_eq!(r.pending_count(), 0);
    }

    #[test]
    fn uncompressed_payloads_pass_through() {
        let data = b"not compressed".to_vec();
        assert_eq!(maybe_decompress(data.clone()).unwrap(), data);
    }

    /// The exact unmunged body of a real HLDS signon reply (first 14 bytes),
    /// captured on 2026-08-01 answering `clc_stringcmd "new"`.
    const LIVE_SIGNON_BODY: [u8; 14] = [
        0x01, 0x05, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x04, 0x00, // fragment header
        0x42, 0x5A, 0x32, 0x00, // "BZ2\0"
    ];

    #[test]
    fn the_live_fragment_header_parses_exactly() {
        let f = Fragments::parse(&LIVE_SIGNON_BODY).expect("parses");
        assert_eq!(f.len, 10, "header should be 10 bytes for one present stream");
        let s0 = f.streams[0].expect("stream 0 present");
        assert_eq!(s0.id, 0x0001_0005);
        assert_eq!(s0.offset, 0);
        assert_eq!(s0.size, 1024);
        assert!(f.streams[1].is_none(), "file stream absent");
        assert!(f.any());

        // 1042 total = 8 header + 10 fragment header + 1024 payload.
        assert_eq!(8 + f.len + usize::from(s0.size), 1042);
    }

    #[test]
    fn the_payload_after_the_fragment_header_is_bzip2() {
        let f = Fragments::parse(&LIVE_SIGNON_BODY).unwrap();
        assert_eq!(&LIVE_SIGNON_BODY[f.len..f.len + 4], &BZ2_MAGIC);
    }

    #[test]
    fn the_fragment_id_packs_index_and_total() {
        // The five ids observed on a live signon.
        let ids = [0x0001_0005u32, 0x0002_0005, 0x0003_0005, 0x0004_0005, 0x0005_0005];
        for (n, id) in ids.iter().enumerate() {
            let f = FragmentInfo { id: *id, offset: 0, size: 0 };
            assert_eq!(f.index(), n as u16 + 1, "index is one-based");
            assert_eq!(f.total(), 5);
        }
    }

    #[test]
    fn fragments_reassemble_by_index_not_offset() {
        // Every real fragment reported offset 0; only the index orders them.
        let mut buf = FragmentBuffer::new();
        let mk = |i: u16| FragmentInfo { id: (u32::from(i) << 16) | 3, offset: 0, size: 0 };

        assert_eq!(buf.push(mk(2), b"BBB"), None);
        assert_eq!(buf.push(mk(3), b"CCC"), None);
        assert_eq!(buf.received(), 2);
        assert_eq!(buf.total(), Some(3));
        assert_eq!(buf.push(mk(1), b"AAA").as_deref(), Some(&b"AAABBBCCC"[..]));
        assert_eq!(buf.received(), 0, "buffer resets after completing");
    }

    #[test]
    fn out_of_range_fragment_indices_are_ignored() {
        let mut buf = FragmentBuffer::new();
        // index 0 is invalid (ids are one-based), as is index > total.
        assert_eq!(buf.push(FragmentInfo { id: 0x0000_0003, offset: 0, size: 0 }, b"x"), None);
        assert_eq!(buf.push(FragmentInfo { id: 0x0009_0003, offset: 0, size: 0 }, b"x"), None);
        assert_eq!(buf.push(FragmentInfo { id: 0x0001_0000, offset: 0, size: 0 }, b"x"), None);
        assert_eq!(buf.received(), 0);
    }

    #[test]
    fn a_new_message_resets_a_partial_one() {
        let mut buf = FragmentBuffer::new();
        buf.push(FragmentInfo { id: 0x0001_0004, offset: 0, size: 0 }, b"old");
        assert_eq!(buf.received(), 1);
        // Different total -> different message.
        let done = buf.push(FragmentInfo { id: 0x0001_0001, offset: 0, size: 0 }, b"new");
        assert_eq!(done.as_deref(), Some(&b"new"[..]));
    }

    #[test]
    fn read_preserves_the_unmunged_tail() {
        // Munge only transforms whole dwords. A body whose length is not a
        // multiple of four must keep its trailing bytes -- dropping them
        // corrupts fragmented payloads (2 bytes lost per 1034-byte fragment).
        let mut ch = NetChannel::new();
        let mut pkt = Vec::new();
        pkt.extend_from_slice(&1u32.to_le_bytes());
        pkt.extend_from_slice(&0u32.to_le_bytes());
        pkt.extend_from_slice(&[0xAA; 10]); // 10 bytes: not a dword multiple
        let (_, body) = ch.read(&pkt).expect("decodes");
        assert_eq!(body.len(), 10, "tail bytes must survive");
    }

    #[test]
    fn a_header_with_no_fragments_is_two_bytes() {
        let f = Fragments::parse(&[0, 0, 0xAA]).unwrap();
        assert_eq!(f.len, 2);
        assert!(!f.any());
    }

    #[test]
    fn both_streams_can_carry_fragments() {
        let mut b = vec![1u8];
        b.extend_from_slice(&7u32.to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes());
        b.extend_from_slice(&512u16.to_le_bytes());
        b.push(1);
        b.extend_from_slice(&9u32.to_le_bytes());
        b.extend_from_slice(&512u16.to_le_bytes());
        b.extend_from_slice(&256u16.to_le_bytes());

        let f = Fragments::parse(&b).unwrap();
        assert_eq!(f.len, 18);
        assert_eq!(f.streams[0].unwrap().size, 512);
        assert_eq!(f.streams[1].unwrap().id, 9);
        assert_eq!(f.streams[1].unwrap().offset, 512);
    }

    #[test]
    fn a_truncated_fragment_header_is_rejected() {
        assert!(Fragments::parse(&[1, 0, 0]).is_none());
        assert!(Fragments::parse(&[]).is_none());
    }

    #[test]
    fn a_built_packet_has_a_cleartext_header_and_munged_body() {
        let mut ch = NetChannel::new();
        let payload = NetChannel::string_command("new");
        let pkt = ch.build(&payload, true);

        // Header in the clear.
        let w1 = u32::from_le_bytes(pkt[0..4].try_into().unwrap());
        assert_eq!(w1 & SEQUENCE_MASK, 1);
        assert!(w1 & RELIABLE_FLAG != 0);

        // Body is not the payload verbatim.
        assert_ne!(&pkt[8..8 + payload.len()], &payload[..]);

        // ...but unmunges back to it (plus dword padding).
        let mut body = pkt[8..].to_vec();
        proto::munge::unmunge(&mut body, &proto::munge::TABLE2, 1);
        assert_eq!(&body[..payload.len()], &payload[..]);
    }

    #[test]
    fn the_sequence_advances_per_packet() {
        let mut ch = NetChannel::new();
        let a = ch.build(b"\x01\x00\x00\x00", false);
        let b = ch.build(b"\x01\x00\x00\x00", false);
        let sa = u32::from_le_bytes(a[0..4].try_into().unwrap()) & SEQUENCE_MASK;
        let sb = u32::from_le_bytes(b[0..4].try_into().unwrap()) & SEQUENCE_MASK;
        assert_eq!(sb, sa + 1);
    }

    /// Build a peer packet acknowledging (or not) our reliable bit.
    fn peer_packet(seq: u32, ack: u32, ack_reliable: bool, body: &[u8]) -> Vec<u8> {
        let mut munged = body.to_vec();
        while munged.len() % 4 != 0 {
            munged.push(clc::NOP);
        }
        let n = munged.len();
        proto::munge::munge(&mut munged[..n], &proto::munge::TABLE2, seq as i32);
        let mut out = Vec::new();
        out.extend_from_slice(&(seq & SEQUENCE_MASK).to_le_bytes());
        let mut w2 = ack & SEQUENCE_MASK;
        if ack_reliable {
            w2 |= RELIABLE_FLAG;
        }
        out.extend_from_slice(&w2.to_le_bytes());
        out.extend_from_slice(&munged);
        out
    }

    #[test]
    fn the_body_munge_is_keyed_on_the_low_byte_of_the_sequence() {
        // Regression guard for the bug that killed every connection a few
        // seconds in: sequences 1 and 257 share a low byte, so they must munge
        // a given payload identically. Keyed on the full sequence they would
        // not, and the server would stop understanding us from sequence 256 on.
        let payload = b"\x03hello\x00\x01\x01";

        let mut a = NetChannel::new();
        a.outgoing_sequence = 1;
        let pa = a.transmit(payload);

        let mut b = NetChannel::new();
        b.outgoing_sequence = 257;
        let pb = b.transmit(payload);

        assert_eq!(
            &pa[8..],
            &pb[8..],
            "sequences sharing a low byte must produce the same munged body"
        );

        // And the first byte alone cannot tell the two keyings apart -- which
        // is exactly why this went unnoticed. Pin that fact so the weakness of
        // a "first byte is a valid opcode" check stays on the record.
        let mut c = NetChannel::new();
        c.outgoing_sequence = 2;
        let pc = c.transmit(payload);
        assert_ne!(&pa[8..], &pc[8..], "a different low byte must differ");
    }

    #[test]
    fn a_high_sequence_packet_round_trips() {
        // Build at a sequence past 255 and read it back.
        let mut tx = NetChannel::new();
        tx.outgoing_sequence = 5000;
        let payload = NetChannel::string_command("jointeam 1");
        let pkt = tx.transmit(&payload);

        let mut rx = NetChannel::new();
        rx.incoming_sequence = 4999;
        let (header, body) = rx.read(&pkt).expect("decodes");
        assert_eq!(header.sequence, 5000);
        assert_eq!(&body[..payload.len()], &payload[..]);
    }

    /// Decode a packet we built: strip the netchannel header, unmunge, and
    /// return `(header, fragment info, payload)`.
    fn decode_ours(pkt: &[u8]) -> (NetHeader, Option<Fragments>, Vec<u8>) {
        let mut rx = NetChannel::new();
        // Accept any sequence.
        rx.incoming_sequence = 0;
        let Datagram::Sequenced(header, _) = classify(pkt) else {
            panic!("not sequenced");
        };
        let (_, body) = rx.read(pkt).expect("decodes");
        if !header.fragment {
            return (header, None, body);
        }
        let frags = Fragments::parse(&body).expect("fragment header");
        let payload = body[frags.len..].to_vec();
        (header, Some(frags), payload)
    }

    #[test]
    fn a_fragmented_upload_matches_the_observed_wire_format() {
        // 12 fragments: eleven of 128 bytes then a 4-byte remainder, exactly
        // the shape of the real client's clc_fileconsistency upload.
        let data: Vec<u8> = (0..1412u32).map(|i| (i % 251) as u8).collect();
        let mut ch = NetChannel::new();
        ch.queue_fragmented(&data);
        assert!(ch.fragment_upload_active());

        let mut got = Vec::new();
        for expect_index in 1..=12u16 {
            let pkt = ch.transmit(&[]);
            let (header, frags, payload) = decode_ours(&pkt);

            assert!(header.reliable, "fragments carry the reliable flag");
            assert!(header.fragment, "and the fragment flag");
            let frags = frags.unwrap();
            assert_eq!(frags.len, 10, "10-byte, two-stream fragment header");
            let info = frags.streams[0].expect("stream 0 present");
            assert!(frags.streams[1].is_none(), "stream 1 unused by the client");
            assert_eq!(info.index(), expect_index);
            assert_eq!(info.total(), 12);
            assert_eq!(info.offset, 0, "offset is always zero");

            let expected_len = if expect_index == 12 { 4 } else { 128 };
            assert_eq!(usize::from(info.size), expected_len);
            got.extend_from_slice(&payload[..expected_len]);

            // Acknowledge so the next fragment is promoted.
            let ack = peer_packet(u32::from(expect_index), 1, ch.outgoing_reliable != 0, &[1]);
            ch.read(&ack).expect("ack decodes");
        }
        assert_eq!(got, data, "the upload reassembles to the original payload");
        assert!(!ch.fragment_upload_active(), "upload is finished");
    }

    #[test]
    fn a_fragment_waits_for_its_acknowledgement_before_the_next_one() {
        let data = vec![0xABu8; 300];
        let mut ch = NetChannel::new();
        ch.queue_fragmented(&data);

        // Fragment 1 goes out once.
        let pkt = ch.transmit(&[]);
        let (_, frags, _) = decode_ours(&pkt);
        let info = frags.unwrap().streams[0].unwrap();
        assert_eq!((info.index(), info.total()), (1, 3));

        // Without an acknowledgement the channel does not advance, and does
        // not blindly repeat either.
        for _ in 0..3 {
            let pkt = ch.transmit(&[clc::NOP]);
            let seq = u32::from_le_bytes(pkt[0..4].try_into().unwrap());
            assert!(seq & FRAGMENT_FLAG == 0, "no blind fragment repeat");
        }
        assert_eq!(ch.fragment_progress(), Some((2, 3)), "still on fragment 2");
    }

    #[test]
    fn a_small_payload_is_a_single_fragment() {
        // The real client's clc_resourcelist went as `idx=1/1 len=41`.
        let data = vec![5u8; 41];
        let mut ch = NetChannel::new();
        ch.queue_fragmented(&data);
        let pkt = ch.transmit(&[]);
        let (_, frags, payload) = decode_ours(&pkt);
        let info = frags.unwrap().streams[0].unwrap();
        assert_eq!((info.index(), info.total()), (1, 1));
        assert_eq!(usize::from(info.size), 41);
        assert_eq!(&payload[..41], &data[..]);
    }

    #[test]
    fn an_empty_fragmented_payload_is_ignored() {
        let mut ch = NetChannel::new();
        ch.queue_fragmented(&[]);
        assert!(!ch.fragment_upload_active());
    }

    #[test]
    fn only_one_reliable_is_in_flight_at_a_time() {
        let mut ch = NetChannel::new();
        ch.queue_reliable(&NetChannel::string_command("spawn 1 0"));
        ch.queue_reliable(&NetChannel::string_command("begin"));
        assert_eq!(ch.queued_count(), 2);
        assert!(!ch.reliable_in_flight());

        // First transmit promotes exactly one and toggles the reliable bit.
        let before = ch.outgoing_reliable;
        let _ = ch.transmit(&[]);
        assert!(ch.reliable_in_flight());
        assert_eq!(ch.outgoing_reliable, before ^ 1, "the bit must toggle");
        assert_eq!(ch.queued_count(), 1, "the second must wait its turn");
    }

    #[test]
    fn a_reliable_goes_out_once_and_is_not_repeated_blindly() {
        // Quake/GoldSrc sends the reliable on promotion and then stays quiet
        // until it can prove the peer missed it. Repeating it in every packet
        // makes the server re-execute the command continuously and overflow
        // its outgoing reliable buffer.
        let mut ch = NetChannel::new();
        let cmd = NetChannel::string_command("spawn 1 0");
        ch.queue_reliable(&cmd);

        let first = ch.transmit(&[]);
        let seq = u32::from_le_bytes(first[0..4].try_into().unwrap());
        assert!(seq & RELIABLE_FLAG != 0, "first send carries the reliable");
        let mut rx = NetChannel::new();
        let (_, body) = rx.read(&first).expect("decodes");
        assert_eq!(&body[..cmd.len()], &cmd[..]);

        // Nothing has acknowledged anything yet, so the next packets must NOT
        // repeat it.
        for _ in 0..3 {
            let pkt = ch.transmit(&[clc::NOP]);
            let seq = u32::from_le_bytes(pkt[0..4].try_into().unwrap());
            assert!(
                seq & RELIABLE_FLAG == 0,
                "must not repeat the reliable without evidence of loss"
            );
        }
        assert!(ch.reliable_in_flight(), "still awaiting acknowledgement");
    }

    #[test]
    fn a_reliable_is_resent_once_loss_is_evident() {
        // Loss is proven when the peer acknowledges a sequence LATER than the
        // one the reliable went out on while still echoing the wrong bit.
        let mut ch = NetChannel::new();
        ch.queue_reliable(&NetChannel::string_command("spawn 1 0"));
        let sent_on = ch.outgoing_sequence;
        let _ = ch.transmit(&[]);

        // Peer acks a later sequence, with the opposite reliable bit.
        let wrong_bit = ch.outgoing_reliable == 0;
        let ack = peer_packet(10, sent_on + 3, wrong_bit, &[1]);
        ch.read(&ack).expect("decodes");

        let pkt = ch.transmit(&[clc::NOP]);
        let seq = u32::from_le_bytes(pkt[0..4].try_into().unwrap());
        assert!(seq & RELIABLE_FLAG != 0, "loss detected, so resend");
    }

    #[test]
    fn acknowledging_frees_the_channel_for_the_next_reliable() {
        let mut ch = NetChannel::new();
        ch.queue_reliable(&NetChannel::string_command("spawn 1 0"));
        ch.queue_reliable(&NetChannel::string_command("begin"));
        let _ = ch.transmit(&[]);
        let in_flight_bit = ch.outgoing_reliable;
        assert!(ch.reliable_in_flight());

        // The peer echoes our reliable bit: the message got through.
        let ack = peer_packet(1, 1, in_flight_bit != 0, &[1]);
        ch.read(&ack).expect("decodes");
        assert!(!ch.reliable_in_flight(), "ack must clear the in-flight buffer");

        // The next transmit promotes the queued `begin`.
        let _ = ch.transmit(&[]);
        assert!(ch.reliable_in_flight());
        assert_eq!(ch.queued_count(), 0);
        assert_eq!(ch.outgoing_reliable, in_flight_bit ^ 1, "toggled again");
    }

    #[test]
    fn a_duplicate_packet_is_dropped_without_flipping_the_ack_bit() {
        // The regression that produced "reliable overflow": the reliable bit
        // is a toggle, so processing a retransmission twice flips it back and
        // the peer never sees its acknowledgement.
        // A *reliable* peer packet: bit 31 of w1 set, which is what drives the
        // toggle.
        let reliable_peer_packet = |seq: u32| {
            let mut p = peer_packet(seq, 0, false, &[1]);
            let w1 = (seq & SEQUENCE_MASK) | RELIABLE_FLAG;
            p[0..4].copy_from_slice(&w1.to_le_bytes());
            p
        };

        let mut ch = NetChannel::new();
        let pkt = reliable_peer_packet(5);
        let before = ch.incoming_reliable;
        ch.read(&pkt).expect("first copy is accepted");
        let after_first = ch.incoming_reliable;
        assert_eq!(after_first, before ^ 1, "a new reliable toggles the bit");

        assert!(ch.read(&pkt).is_none(), "a duplicate must be dropped");
        assert_eq!(
            ch.incoming_reliable, after_first,
            "the acknowledgement bit must not move on a duplicate"
        );

        // An older packet is stale too.
        assert!(ch.read(&reliable_peer_packet(4)).is_none());
        assert_eq!(ch.incoming_reliable, after_first);

        // A newer one is accepted, and toggles again.
        assert!(ch.read(&reliable_peer_packet(6)).is_some());
        assert_eq!(ch.incoming_reliable, after_first ^ 1);
    }

    #[test]
    fn a_mismatched_ack_does_not_clear_the_reliable() {
        let mut ch = NetChannel::new();
        ch.queue_reliable(&NetChannel::string_command("spawn 1 0"));
        let _ = ch.transmit(&[]);
        let in_flight_bit = ch.outgoing_reliable;

        // The peer's ack bit does NOT match ours -- not an acknowledgement.
        let stale = peer_packet(1, 1, in_flight_bit == 0, &[1]);
        ch.read(&stale).expect("decodes");
        assert!(ch.reliable_in_flight(), "must keep retransmitting");
    }

    #[test]
    fn unreliable_payloads_ride_after_the_reliable_one() {
        let mut ch = NetChannel::new();
        let cmd = NetChannel::string_command("spawn 1 0");
        ch.queue_reliable(&cmd);
        let pkt = ch.transmit(&[0xAA, 0xBB]);

        let mut rx = NetChannel::new();
        let (_, body) = rx.read(&pkt).expect("decodes");
        assert_eq!(&body[..cmd.len()], &cmd[..]);
        assert_eq!(&body[cmd.len()..cmd.len() + 2], &[0xAA, 0xBB]);
    }

    #[test]
    fn payloads_are_padded_to_a_dword_with_nop() {
        // Real clients always emit dword-aligned bodies, padded with
        // `clc_nop`. The padding byte must never be zero: `clc_bad` is opcode
        // 0, so zero padding appends bogus commands and HLDS drops the client
        // with `too many cmds -2`.
        let mut ch = NetChannel::new();
        let pkt = ch.build(b"abc", false);
        assert_eq!(pkt.len(), 8 + 4, "body is rounded up to a whole dword");

        let mut rx = NetChannel::new();
        let (_, body) = rx.read(&pkt).expect("decodes");
        assert_eq!(body, b"abc\x01", "padded with clc_nop, not zero");
    }

    #[test]
    fn an_unaligned_payload_round_trips_intact() {
        // 5 bytes of payload become 8: the whole body is munged, with three
        // `clc_nop`s appended, which the server harmlessly skips.
        let payload = NetChannel::string_command("new"); // 5 bytes
        assert_eq!(payload.len(), 5);
        let mut tx = NetChannel::new();
        let pkt = tx.build(&payload, true);
        assert_eq!(pkt.len(), 8 + 8);

        let mut rx = NetChannel::new();
        let (_, body) = rx.read(&pkt).expect("decodes");
        assert_eq!(&body[..5], &payload[..], "the payload survives unchanged");
        assert!(
            body[5..].iter().all(|&b| b == clc::NOP),
            "the tail is nop padding"
        );
    }

    #[test]
    fn string_commands_are_nul_terminated_after_the_opcode() {
        let p = NetChannel::string_command("sendents");
        assert_eq!(p[0], clc::STRINGCMD);
        assert_eq!(&p[1..p.len() - 1], b"sendents");
        assert_eq!(*p.last().unwrap(), 0);
    }

    #[test]
    fn a_packet_round_trips_through_build_and_read() {
        let mut tx = NetChannel::new();
        let payload = NetChannel::string_command("fullupdate");
        let pkt = tx.build(&payload, true);

        let mut rx = NetChannel::new();
        let (header, body) = rx.read(&pkt).expect("should decode");
        assert_eq!(header.sequence, 1);
        assert!(header.reliable);
        assert_eq!(&body[..payload.len()], &payload[..]);
    }

    #[test]
    fn bz2_magic_is_exactly_four_bytes() {
        assert_eq!(&BZ2_MAGIC, b"BZ2\0");
    }

    /// A real bzip2 stream (produced by Python's `bz2.compress`). Note this is
    /// the raw `BZh9...` stream -- GoldSrc prefixes it with the `BZ2\0` magic,
    /// which is *not* part of the bzip2 format itself.
    const BZ2_STREAM: &[u8] = &[
        66, 90, 104, 57, 49, 65, 89, 38, 83, 89, 87, 103, 127, 88, 0, 0, 53, 145, 128, 64, 0, 63,
        255, 255, 240, 32, 0, 80, 166, 141, 0, 104, 0, 2, 191, 213, 73, 164, 208, 105, 166, 35,
        106, 121, 168, 205, 13, 80, 242, 170, 106, 183, 120, 108, 155, 105, 46, 193, 194, 210, 93,
        70, 233, 176, 67, 242, 140, 220, 44, 163, 226, 171, 168, 178, 16, 213, 41, 114, 229, 208,
        187, 146, 41, 194, 132, 130, 187, 59, 250, 192,
    ];

    #[test]
    fn bz2_payload_is_actually_decompressed() {
        let mut packet = BZ2_MAGIC.to_vec();
        packet.extend_from_slice(BZ2_STREAM);
        let out = maybe_decompress(packet).expect("must decompress");
        assert_eq!(
            out,
            b"the quick brown fox jumps over the lazy dog".repeat(3)
        );
    }

    #[test]
    fn corrupt_bz2_payload_errors_rather_than_panicking() {
        let mut packet = BZ2_MAGIC.to_vec();
        packet.extend_from_slice(&BZ2_STREAM[..20]); // truncated
        assert!(maybe_decompress(packet).is_err());
    }
}
