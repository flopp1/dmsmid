//! Domino file → Standard MIDI File (format 1, one SMF track per Domino track).
//!
//! Streaming: the zlib stream is inflated on a background thread into a few
//! recycled blocks, and each event is written as soon as it is decoded. Memory
//! is constant apart from the sounding notes waiting for their note-off and the
//! events of the current tick. Track lengths and the header are patched at the end.

use crate::chunk::{self, Fields};
use crate::dms::*;
use crate::envelope;
use crate::module::{Expansion, Module, ID_CHANNEL_PRESSURE, ID_PITCH_BEND};
use crate::pipe::{self, BlockSource};
use crate::smf;
use crate::Report;
use anyhow::{bail, Context, Result};
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::sync::mpsc::sync_channel;

pub fn convert<W: Write + Seek>(file: &[u8], module: &Module, out: W) -> Result<(W, Report)> {
    let Some(magic) = envelope::magic_of(file) else { bail!("not Domino data (unknown header)") };
    let Some(size) = file.get(magic.len()..magic.len() + 4) else { bail!("file is truncated") };
    let size = u32::from_le_bytes(size.try_into().unwrap()) as u64;
    let zlib = &file[magic.len() + 4..];

    std::thread::scope(|s| {
        let (tx, rx) = sync_channel(4);
        let (recycle_tx, recycle_rx) = sync_channel(6);
        s.spawn(move || pipe::inflate(zlib, size, tx, recycle_rx));
        let mut src = BlockSource::new(rx, recycle_tx);
        let r = write_song(&mut src, size, module, out);
        if r.is_ok() {
            src.finish()?;
        }
        r
    })
}

fn write_song<W: Write + Seek>(src: &mut BlockSource, size: u64, module: &Module, out: W) -> Result<(W, Report)> {
    let mut report = Report::default();
    let mut w = Out { w: BufWriter::with_capacity(1 << 20, out), pos: 0 };
    // ntracks and PPQ are patched at the end.
    w.write(b"MThd\0\0\0\x06\0\x01\0\0\0\0")?;
    let (mut ppq, mut song_name, mut copyright) = (None, Vec::new(), Vec::new());
    let mut ntracks = 0u32;

    while src.offset + 6 <= size {
        let h = src.take(6)?;
        let id = u16::from_le_bytes([h[0], h[1]]);
        let len = u32::from_le_bytes(h[2..6].try_into().unwrap()) as u64;
        if src.offset + len > size {
            report.warn("damaged chunk at the end of the file (ignored)");
            src.skip(size - src.offset)?;
            break;
        }
        match id {
            ROOT_TRACK => {
                let p = ppq.context("no PPQ before the first track (root chunk 1002)")?;
                if p == 0 || p > 0x7FFF {
                    bail!("unusable PPQ {p}");
                }
                if ntracks == 0xFFFF {
                    bail!("too many tracks for a MIDI file");
                }
                let header = TrackHeader { first: ntracks == 0, song_name: &song_name, copyright: &copyright };
                write_track(src, len, &header, module, &mut w, &mut report)?;
                ntracks += 1;
            }
            ROOT_NAME => song_name = src.take(len as usize)?.to_vec(),
            ROOT_COPYRIGHT => copyright = src.take(len as usize)?.to_vec(),
            ROOT_PPQ => ppq = chunk::uint(src.take(len as usize)?).map(|v| v as u16),
            _ => src.skip(len)?,
        }
    }
    if src.offset < size {
        report.warn("stray bytes at the end of the file (ignored)");
        src.skip(size - src.offset)?;
    }
    if ntracks == 0 {
        bail!("the file has no tracks");
    }
    let mut out = w.w;
    out.seek(SeekFrom::Start(10))?;
    out.write_all(&(ntracks as u16).to_be_bytes())?;
    out.write_all(&ppq.unwrap_or(480).to_be_bytes())?;
    out.seek(SeekFrom::End(0))?;
    out.flush()?;
    report.tracks = ntracks as usize;
    let out = out.into_inner().map_err(|e| e.into_error())?;
    Ok((out, report))
}

/// Output with a running byte position (so track lengths can be patched without flushing).
struct Out<W: Write + Seek> {
    w: BufWriter<W>,
    pos: u64,
}

impl<W: Write + Seek> Out<W> {
    #[inline]
    fn write(&mut self, b: &[u8]) -> std::io::Result<()> {
        self.pos += b.len() as u64;
        self.w.write_all(b)
    }
}

struct TrackHeader<'a> {
    first: bool,
    song_name: &'a [u8],
    copyright: &'a [u8],
}

fn write_track<W: Write + Seek>(
    src: &mut BlockSource,
    len: u64,
    hdr: &TrackHeader,
    module: &Module,
    w: &mut Out<W>,
    report: &mut Report,
) -> Result<()> {
    let start = w.pos;
    w.write(b"MTrk\0\0\0\0")?;
    let mut em = Emitter::default();
    let (mut ch, mut port, mut conductor, mut name) = (0u8, 0u8, false, Vec::new());
    let mut header_done = false;
    let mut end = 0u64;
    let stop = src.offset + len;

    while src.offset < stop {
        if stop - src.offset < 6 {
            report.warn("damaged track (rest skipped)");
            src.skip(stop - src.offset)?;
            break;
        }
        let h = src.take(6)?;
        let id = u16::from_le_bytes([h[0], h[1]]);
        let clen = u32::from_le_bytes(h[2..6].try_into().unwrap()) as u64;
        if clen > stop - src.offset {
            report.warn("damaged track (rest skipped)");
            src.skip(stop - src.offset)?;
            break;
        }
        let body = src.take(clen as usize)?;
        if id < 2001 {
            if !header_done {
                match id {
                    TR_PORT => port = chunk::uint(body).unwrap_or(0) as u8,
                    TR_CHANNEL => ch = chunk::uint(body).unwrap_or(0) as u8 & 0x0F,
                    TR_NAME => name = body.to_vec(),
                    TR_CONDUCTOR => conductor = chunk::uint(body) == Some(1),
                    _ => {}
                }
            }
            continue;
        }
        if !header_done {
            header_done = true;
            write_header(w, &mut em, hdr, &name, conductor, port)?;
        }
        if id == TR_END {
            end = Fields::new(body).uint(POS).unwrap_or(0) as u64;
            continue;
        }
        event(id, body, ch, module, w, &mut em, report)?;
    }
    if !header_done {
        write_header(w, &mut em, hdr, &name, conductor, port)?;
    }
    em.finish(w, end)?;

    let track_len = w.pos - start - 8;
    let Ok(track_len) = u32::try_from(track_len) else { bail!("a track is too large for a MIDI file") };
    let here = w.pos;
    w.w.seek(SeekFrom::Start(start + 4))?;
    w.w.write_all(&track_len.to_be_bytes())?;
    w.w.seek(SeekFrom::Start(here))?;
    Ok(())
}

fn write_header<W: Write + Seek>(
    w: &mut Out<W>,
    em: &mut Emitter,
    hdr: &TrackHeader,
    name: &[u8],
    conductor: bool,
    port: u8,
) -> Result<()> {
    let name = if hdr.first && name.is_empty() { hdr.song_name } else { name };
    if !name.is_empty() {
        em.write(w, 0, &smf::meta(0x03, name))?;
    }
    if hdr.first && !hdr.copyright.is_empty() {
        em.write(w, 0, &smf::meta(0x02, hdr.copyright))?;
    }
    if !conductor {
        em.write(w, 0, &[0xFF, 0x21, 1, port])?;
    }
    Ok(())
}

/// Writes one track's events in Domino's order. Within a tick: note-offs, then
/// other events as stored, then time signatures, then key signatures, then the
/// offs of zero-length notes. Events must arrive in ascending position.
#[derive(Default)]
struct Emitter {
    cur: u64,
    last: u64,
    seq: u64,
    offs: BinaryHeap<Reverse<(u64, u64, u8, u8)>>, // (tick, seq, status, key)
    late_offs: Vec<[u8; 3]>,
    time_sigs: Vec<Vec<u8>>,
    key_sigs: Vec<Vec<u8>>,
    msgs: Vec<([u8; 3], usize)>,
    vlq: Vec<u8>,
}

impl Emitter {
    #[inline]
    fn write<W: Write + Seek>(&mut self, w: &mut Out<W>, tick: u64, bytes: &[u8]) -> Result<()> {
        let delta = tick - self.last;
        if delta > 0x0FFF_FFFF {
            bail!("gap between events is too long for a MIDI file");
        }
        self.vlq.clear();
        smf::vlq(delta as u32, &mut self.vlq);
        w.write(&self.vlq)?;
        w.write(bytes)?;
        self.last = tick;
        Ok(())
    }

    /// Flush what belongs at the end of the current tick.
    fn close_tick<W: Write + Seek>(&mut self, w: &mut Out<W>) -> Result<()> {
        let cur = self.cur;
        for list in [std::mem::take(&mut self.time_sigs), std::mem::take(&mut self.key_sigs)] {
            for m in &list {
                self.write(w, cur, m)?;
            }
        }
        for m in std::mem::take(&mut self.late_offs) {
            self.write(w, cur, &m)?;
        }
        Ok(())
    }

    /// Move to `pos`; returns the position actually used (never earlier than the current one).
    fn advance<W: Write + Seek>(&mut self, w: &mut Out<W>, pos: u64, report: &mut Report) -> Result<u64> {
        if pos < self.cur {
            report.warn("event stored out of order (moved later to keep the MIDI file valid)");
            return Ok(self.cur);
        }
        if pos > self.cur {
            self.close_tick(w)?;
            while let Some(&Reverse((t, _, st, key))) = self.offs.peek() {
                if t > pos {
                    break;
                }
                self.offs.pop();
                self.write(w, t, &[st, key, 0])?;
            }
            self.cur = pos;
        }
        Ok(pos)
    }

    fn note_off(&mut self, tick: u64, status: u8, key: u8) {
        if tick == self.cur {
            self.late_offs.push([status, key, 0]);
        } else {
            self.offs.push(Reverse((tick, self.seq, status, key)));
            self.seq += 1;
        }
    }

    fn finish<W: Write + Seek>(&mut self, w: &mut Out<W>, end: u64) -> Result<()> {
        self.close_tick(w)?;
        while let Some(Reverse((t, _, st, key))) = self.offs.pop() {
            self.write(w, t, &[st, key, 0])?;
        }
        let eot = end.max(self.last);
        self.write(w, eot, &[0xFF, 0x2F, 0])
    }
}

/// Canonical 40-byte note body: position, key, velocity, gate.
#[inline]
fn fast_note(b: &[u8]) -> Option<(u32, u8, u8, u32)> {
    if b.len() == 34
        && b[0..6] == [0xE9, 0x03, 4, 0, 0, 0]
        && b[10..16] == [0xD1, 0x07, 1, 0, 0, 0]
        && b[17..23] == [0xD2, 0x07, 1, 0, 0, 0]
        && b[24..30] == [0xD3, 0x07, 4, 0, 0, 0]
    {
        let pos = u32::from_le_bytes(b[6..10].try_into().unwrap());
        let gate = u32::from_le_bytes(b[30..34].try_into().unwrap());
        Some((pos, b[16], b[23], gate))
    } else {
        None
    }
}

fn event<W: Write + Seek>(
    id: u16,
    body: &[u8],
    ch: u8,
    module: &Module,
    w: &mut Out<W>,
    em: &mut Emitter,
    report: &mut Report,
) -> Result<()> {
    if id == EV_NOTE {
        if let Some((pos, key, vel, gate)) = fast_note(body) {
            return note(pos as u64, key as u32, Some(vel as u32), gate, ch, w, em, report);
        }
    }
    let f = Fields::new(body);
    if !f.is_valid() {
        report.warn("damaged event (skipped)");
        return Ok(());
    }
    let Some(pos) = f.uint(POS) else {
        report.warn(format!("event {id} without a position (skipped)"));
        return Ok(());
    };
    let pos = pos as u64;
    match id {
        EV_NOTE => {
            let (Some(key), Some(gate)) = (f.uint(2001), f.uint(2003)) else {
                report.warn("note without key or length (skipped)");
                return Ok(());
            };
            note(pos, key, f.uint(2002), gate, ch, w, em, report)?;
        }
        EV_PROGRAM => {
            let Some(patch) = f.uint(2003) else {
                report.warn("program change without a number");
                return Ok(());
            };
            let pos = em.advance(w, pos, report)?;
            // 2001/2002 are -1 in every file seen; when set they are taken to be bank MSB/LSB.
            for (field, cc) in [(2001, 0u8), (2002, 32u8)] {
                if let Some(b) = f.sint(field).filter(|b| (0..=127).contains(b)) {
                    em.write(w, pos, &[0xB0 | ch, cc, b as u8])?;
                }
            }
            em.write(w, pos, &[0xC0 | ch, patch.clamp(1, 128) as u8 - 1])?;
        }
        EV_CONTROL => {
            let Some(cid) = f.uint(2001) else {
                report.warn("control change without an ID");
                return Ok(());
            };
            let cid = cid as u16;
            let value = f.float(2003).unwrap_or(0.0).round() as i32;
            let gate = f.float(2002).unwrap_or(0.0).round() as i32;
            em.msgs.clear();
            match module.get(cid) {
                Some(ccm) => match ccm.expand(ch, value, gate, |m, n| em.msgs.push((m, n))) {
                    Expansion::Done => {}
                    Expansion::Sysex => {
                        report.warn(format!("CC macro {cid} sends system exclusive (skipped)"));
                        return Ok(());
                    }
                    Expansion::Error(e) => {
                        report.warn(format!("{e} (skipped)"));
                        return Ok(());
                    }
                },
                None => match cid {
                    0..=127 => em.msgs.push(([0xB0 | ch, cid as u8, value.clamp(0, 127) as u8], 3)),
                    ID_PITCH_BEND => {
                        let v = (value + 8192).clamp(0, 0x3FFF) as u16;
                        em.msgs.push(([0xE0 | ch, (v & 0x7F) as u8, (v >> 7) as u8], 3));
                    }
                    ID_CHANNEL_PRESSURE => em.msgs.push(([0xD0 | ch, value.clamp(0, 127) as u8, 0], 2)),
                    _ => {
                        report.warn(format!("CC macro {cid} is not defined in module '{}' (skipped)", module.name));
                        return Ok(());
                    }
                },
            }
            let pos = em.advance(w, pos, report)?;
            for i in 0..em.msgs.len() {
                let (m, n) = em.msgs[i];
                em.write(w, pos, &m[..n])?;
            }
        }
        EV_TEMPO => {
            let Some(bpm) = f.float(2001).filter(|b| *b > 0.0) else {
                report.warn("invalid tempo (skipped)");
                return Ok(());
            };
            // Domino's own export: truncate a single-precision division.
            let us = ((60_000_000f32 / bpm) as u32).clamp(1, 0xFF_FFFF);
            let pos = em.advance(w, pos, report)?;
            em.write(w, pos, &smf::meta(0x51, &us.to_be_bytes()[1..]))?;
        }
        EV_TIME_SIG => {
            let (Some(num), Some(den)) = (f.uint(2001), f.uint(2002)) else {
                report.warn("incomplete time signature (skipped)");
                return Ok(());
            };
            if !den.is_power_of_two() || num == 0 || num > 255 {
                report.warn(format!("time signature {num}/{den} cannot be written (skipped)"));
                return Ok(());
            }
            em.advance(w, pos, report)?;
            em.time_sigs.push(smf::meta(0x58, &[num as u8, den.trailing_zeros() as u8, 24, 8]));
        }
        EV_KEY_SIG => {
            let Some((sf, minor)) = f.uint(2001).and_then(|c| key_from_code(c as u8)) else {
                report.warn("unknown key signature (skipped)");
                return Ok(());
            };
            em.advance(w, pos, report)?;
            em.key_sigs.push(smf::meta(0x59, &[sf as u8, minor as u8]));
        }
        EV_SYSEX => report.warn("system exclusive event (skipped)"),
        EV_VARIABLES | EV_MEASURE_LINK | EV_SCALE | EV_CHORD => {
            let what = match id {
                EV_VARIABLES => "Variables",
                EV_MEASURE_LINK => "Measure Link",
                EV_SCALE => "Scale",
                _ => "Chord",
            };
            report.warn(format!("Domino-only event '{what}' (dropped, as Domino's own export does)"));
        }
        _ => match TEXT_EVENTS.iter().find(|(e, _)| *e == id) {
            Some((_, meta)) => {
                let pos = em.advance(w, pos, report)?;
                em.write(w, pos, &smf::meta(*meta, f.get(2001).unwrap_or_default()))?;
            }
            None => report.warn(format!("unknown event type {id} (skipped)")),
        },
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
#[inline]
fn note<W: Write + Seek>(
    pos: u64,
    key: u32,
    vel: Option<u32>,
    gate: u32,
    ch: u8,
    w: &mut Out<W>,
    em: &mut Emitter,
    report: &mut Report,
) -> Result<()> {
    let key = (key & 0x7F) as u8;
    // Velocity 0 is a silent note (see to_dms::Pairing): written as a velocity-0
    // note-on with its own note-off, exactly as such files store it.
    let vel = vel.unwrap_or(100).min(127) as u8;
    let pos = em.advance(w, pos, report)?;
    em.write(w, pos, &[0x90 | ch, key, vel])?;
    em.note_off(pos + gate as u64, 0x80 | ch, key);
    report.notes += 1;
    Ok(())
}
