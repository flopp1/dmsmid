//! Standard MIDI File → Domino file, mimicking Domino's own MIDI import.
//!
//! - Tempo, time signature and key signature from any track go to the conductor
//!   track. SMF track 0 *is* the conductor when it has no channel messages;
//!   otherwise an empty conductor is created in front.
//! - Domino holds one channel per track, so an SMF track using several channels
//!   (e.g. every format-0 file) is split into one Domino track per channel.
//!
//! Two passes over the memory-mapped input, both without copying it:
//! 1. every SMF track is scanned in parallel to learn the exact size of each
//!    output track (Domino chunks carry their size up front), the tempo map and
//!    all warnings;
//! 2. the output is generated in order and compressed on all cores
//!    ([`pipe::compress_parallel`]). A note's length is found by a second cursor
//!    reading ahead in the same track, so nothing is buffered per note.

use crate::dms::{self, CONTROL_LEN, NOTE_LEN, PROGRAM_LEN, TEXT_EVENTS};
use crate::envelope;
use crate::module::{Module, ID_CHANNEL_PRESSURE, ID_PITCH_BEND};
use crate::pipe;
use crate::smf::{self, Ev, Events};
use crate::Report;
use anyhow::{bail, Context, Result};
use flate2::Compression;
use std::collections::VecDeque;
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

const MAX_GATE: u64 = 65535; // Domino caps note length on import

/// What one channel message becomes.
enum Midi {
    NoteOn { key: u8, vel: u8 },
    NoteOff { key: u8 },
    Control { id: u16, gate: i32, value: i32 },
    Program(u8),
    Dropped(String),
}

#[inline]
fn midi(st: u8, d1: u8, d2: u8, module: &Module) -> Midi {
    match st & 0xF0 {
        0x90 if d2 > 0 => Midi::NoteOn { key: d1, vel: d2 },
        0x80 | 0x90 => Midi::NoteOff { key: d1 },
        0xA0 => match module.poly_pressure() {
            Some(c) => Midi::Control { id: c.id, gate: d1 as i32 - c.gate_offset, value: d2 as i32 - c.value_offset },
            None => Midi::Dropped(format!(
                "poly key pressure (module '{}' has no such controller; dropped, as Domino does)",
                module.name
            )),
        },
        0xB0 => Midi::Control { id: d1 as u16, gate: 0, value: d2 as i32 - module.cc_offset(d1) },
        0xC0 => Midi::Program(d1),
        0xD0 => Midi::Control { id: ID_CHANNEL_PRESSURE, gate: 0, value: d1 as i32 - module.offset(ID_CHANNEL_PRESSURE, 0) },
        _ => {
            let raw = d1 as i32 | (d2 as i32) << 7;
            Midi::Control { id: ID_PITCH_BEND, gate: 0, value: raw - module.offset(ID_PITCH_BEND, 8192) }
        }
    }
}

fn is_tempo_map(e: &Ev) -> bool {
    matches!(e, Ev::Meta(0x51 | 0x58 | 0x59, _))
}

/// Append the Domino event for a meta/sysex event, or say why it has none.
fn meta_event(out: &mut Vec<u8>, e: &Ev, pos: u32) -> Result<(), String> {
    match *e {
        Ev::Meta(0x51, d) if d.len() == 3 => {
            let us = u32::from_be_bytes([0, d[0], d[1], d[2]]).max(1);
            dms::tempo(out, pos, (60_000_000f64 / us as f64) as f32);
        }
        Ev::Meta(0x58, d) if d.len() >= 2 && d[1] < 8 => dms::time_sig(out, pos, d[0], 1 << d[1]),
        Ev::Meta(0x59, d) if d.len() == 2 => dms::key_sig(out, pos, dms::key_code(d[0] as i8, d[1] != 0)),
        Ev::Meta(ty, d) => match TEXT_EVENTS.iter().find(|(_, m)| *m == ty) {
            Some((id, _)) => dms::text(out, *id, pos, d),
            None => return Err(format!("{} (not supported by Domino, dropped)", meta_name(ty))),
        },
        Ev::Sysex(_) => return Err("system exclusive (ignored)".into()),
        Ev::Midi(..) => unreachable!(),
    }
    Ok(())
}

fn tick32(t: u64) -> Result<u32> {
    u32::try_from(t).ok().context("song is too long for a Domino file (tick overflow)")
}

// ---------------------------------------------------------------------------
// Pass 1: scan

#[derive(Default)]
struct OutScan {
    ch: u8,
    notes: usize,
    events_len: u64,
    last_tick: u64,
    end: u64,
}

#[derive(Default)]
struct Scan {
    name: Vec<u8>,
    port: u16,
    outs: Vec<OutScan>,
    /// Tempo-map events (and, for a conductor track 0, all its events): (tick, track, index, bytes).
    cond: Vec<(u64, u32, u32, Vec<u8>)>,
    copyright: Option<Vec<u8>>,
    end: u64,
    report: Report,
}

fn scan_conductor(track: &[u8]) -> Result<Scan> {
    let mut s = Scan::default();
    let mut buf = Vec::new();
    for (idx, e) in smf::events(track).enumerate() {
        let (tick, ev) = e?;
        match ev {
            Ev::Meta(0x03, d) if s.name.is_empty() => s.name = d.to_vec(),
            Ev::Meta(0x2F, _) => s.end = tick,
            Ev::Meta(0x02, d) => {
                s.copyright.get_or_insert_with(|| d.to_vec());
            }
            Ev::Meta(0x03 | 0x20 | 0x21, _) => {}
            ev => {
                buf.clear();
                match meta_event(&mut buf, &ev, tick32(tick)?) {
                    Ok(()) => s.cond.push((tick, 0, idx as u32, buf.clone())),
                    Err(w) => s.report.warn(w),
                }
            }
        }
    }
    Ok(s)
}

fn scan_track(ti: u32, track: &[u8], module: &Module) -> Result<Scan> {
    let mut s = Scan::default();
    let mut buf = Vec::new();
    // Per output: per key, the start ticks of sounding notes.
    let mut open: Vec<Vec<VecDeque<u64>>> = Vec::new();
    let mut slot = [u8::MAX; 16]; // channel → output index
    // Meta events go to the first output, which may not be known yet.
    let (mut extra_len, mut extra_last) = (0u64, 0u64);
    let mut prefix = None;

    for (idx, e) in smf::events(track).enumerate() {
        let (tick, ev) = e?;
        match ev {
            Ev::Midi(st, d1, d2) => {
                let ch = st & 0x0F;
                if slot[ch as usize] == u8::MAX {
                    slot[ch as usize] = s.outs.len() as u8;
                    s.outs.push(OutScan { ch, ..Default::default() });
                    open.push(vec![VecDeque::new(); 128]);
                }
                let o = slot[ch as usize] as usize;
                let len = match midi(st, d1, d2, module) {
                    Midi::NoteOn { key, .. } => {
                        open[o][key as usize & 0x7F].push_back(tick);
                        s.outs[o].notes += 1;
                        NOTE_LEN
                    }
                    Midi::NoteOff { key } => {
                        match open[o][key as usize & 0x7F].pop_front() {
                            Some(on) if tick - on > MAX_GATE => {
                                s.report.warn("note longer than 65535 ticks (shortened, as Domino does)")
                            }
                            Some(_) => {}
                            None => s.report.warn("note-off without a matching note-on (ignored)"),
                        }
                        continue;
                    }
                    Midi::Control { .. } => CONTROL_LEN,
                    Midi::Program(_) => PROGRAM_LEN,
                    Midi::Dropped(w) => {
                        s.report.warn(w);
                        continue;
                    }
                };
                tick32(tick)?;
                s.outs[o].events_len += len as u64;
                s.outs[o].last_tick = tick;
            }
            Ev::Meta(0x03, d) => {
                if s.name.is_empty() {
                    s.name = d.to_vec();
                }
            }
            Ev::Meta(0x21, d) if d.len() == 1 => s.port = d[0] as u16,
            Ev::Meta(0x2F, _) => s.end = tick,
            Ev::Meta(0x02, d) => {
                s.copyright.get_or_insert_with(|| d.to_vec());
            }
            Ev::Meta(0x20, d) => {
                if d.len() == 1 && prefix.is_none() {
                    prefix = Some(d[0] & 0x0F);
                }
            }
            ev if is_tempo_map(&ev) => {
                buf.clear();
                match meta_event(&mut buf, &ev, tick32(tick)?) {
                    Ok(()) => s.cond.push((tick, ti, idx as u32, buf.clone())),
                    Err(w) => s.report.warn(w),
                }
            }
            ev => {
                buf.clear();
                match meta_event(&mut buf, &ev, tick32(tick)?) {
                    Ok(()) => {
                        extra_len += buf.len() as u64;
                        extra_last = tick;
                    }
                    Err(w) => s.report.warn(w),
                }
            }
        }
    }
    if s.outs.is_empty() {
        s.outs.push(OutScan { ch: prefix.unwrap_or(0), ..Default::default() });
        open.push(vec![VecDeque::new(); 128]);
    }
    if s.outs.len() > 1 {
        s.report.warn("MIDI track with several channels (split into one Domino track per channel)");
    }
    s.outs[0].events_len += extra_len;
    s.outs[0].last_tick = s.outs[0].last_tick.max(extra_last);
    for (o, keys) in s.outs.iter_mut().zip(&open) {
        o.end = s.end.max(o.last_tick);
        tick32(o.end)?;
        for on in keys.iter().flatten() {
            s.report.warn("note without a note-off (ended at the end of its track)");
            if o.end - on > MAX_GATE {
                s.report.warn("note longer than 65535 ticks (shortened, as Domino does)");
            }
        }
    }
    Ok(s)
}

/// Run `f` over `0..n` on all cores; results in index order.
fn par_map<T: Send>(n: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
    let next = AtomicUsize::new(0);
    let results: Vec<Mutex<Option<T>>> = (0..n).map(|_| Mutex::new(None)).collect();
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(n.max(1));
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= n {
                    return;
                }
                *results[i].lock().unwrap() = Some(f(i));
            });
        }
    });
    results.into_iter().map(|m| m.into_inner().unwrap().unwrap()).collect()
}

// ---------------------------------------------------------------------------
// Pass 2: generate

/// Second cursor over a track that finds note-offs ahead of the writer.
/// Pairing is first-in-first-out per key, exactly as in pass 1.
struct Lookahead<'a> {
    it: Events<'a>,
    ch: u8,
    idx: u64,
    open: [u32; 128],
    offs: Vec<VecDeque<u64>>,
}

impl<'a> Lookahead<'a> {
    fn new(track: &'a [u8], ch: u8) -> Self {
        Lookahead { it: smf::events(track), ch, idx: 0, open: [0; 128], offs: vec![VecDeque::new(); 128] }
    }

    fn step(&mut self) -> Result<bool> {
        let Some(e) = self.it.next() else { return Ok(false) };
        let (tick, ev) = e?;
        self.idx += 1;
        if let Ev::Midi(st, key, vel) = ev {
            if st & 0x0F == self.ch {
                let k = key as usize & 0x7F;
                match st & 0xF0 {
                    0x90 if vel > 0 => self.open[k] += 1,
                    0x80 | 0x90 if self.open[k] > 0 => {
                        self.open[k] -= 1;
                        self.offs[k].push_back(tick);
                    }
                    _ => {}
                }
            }
        }
        Ok(true)
    }

    /// Note-off tick for the note-on that is event number `idx` (1-based) of the track.
    fn off_for(&mut self, key: u8, idx: u64) -> Result<Option<u64>> {
        while self.idx < idx && self.step()? {}
        let k = key as usize & 0x7F;
        while self.offs[k].is_empty() && self.step()? {}
        Ok(self.offs[k].pop_front())
    }
}

const FLUSH: usize = 64 * 1024;

fn generate_output(
    track: &[u8],
    ch: u8,
    first: bool,
    end: u64,
    module: &Module,
    buf: &mut Vec<u8>,
    sink: &mut impl Write,
) -> Result<()> {
    let mut la = Lookahead::new(track, ch);
    for (i, e) in smf::events(track).enumerate() {
        let (tick, ev) = e?;
        let pos = tick as u32; // checked in pass 1
        match ev {
            Ev::Midi(st, d1, d2) if st & 0x0F == ch => match midi(st, d1, d2, module) {
                Midi::NoteOn { key, vel } => {
                    let gate = match la.off_for(key, i as u64 + 1)? {
                        Some(off) => off - tick,
                        None => end - tick,
                    };
                    dms::note(buf, pos, key, vel, gate.min(MAX_GATE) as u32);
                }
                Midi::Control { id, gate, value } => dms::control(buf, pos, id, gate as f32, value as f32),
                Midi::Program(p) => dms::program(buf, pos, p),
                Midi::NoteOff { .. } | Midi::Dropped(_) => {}
            },
            Ev::Midi(..) => {}
            Ev::Meta(0x02 | 0x03 | 0x20 | 0x21 | 0x2F, _) => {}
            ev if !first || is_tempo_map(&ev) => {}
            ev => {
                let _ = meta_event(buf, &ev, pos);
            }
        }
        if buf.len() >= FLUSH {
            sink.write_all(buf)?;
            buf.clear();
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------

/// Convert SMF bytes into a complete `.dms` file written to `out`.
pub fn convert<W: Write + Send>(data: &[u8], module: &Module, out: W) -> Result<(W, Report)> {
    convert_with(data, module, out, Compression::default(), pipe::BLOCK)
}

pub fn convert_with<W: Write + Send>(
    data: &[u8],
    module: &Module,
    out: W,
    level: Compression,
    block: usize,
) -> Result<(W, Report)> {
    let h = smf::index(data)?;
    if h.tracks.is_empty() {
        bail!("the MIDI file has no tracks");
    }
    let ppq = h.ppq;
    let mut has_midi = false;
    for e in smf::events(h.tracks[0]) {
        if matches!(e?.1, Ev::Midi(..)) {
            has_midi = true;
            break;
        }
    }
    let first_is_conductor = h.format != 0 && !has_midi;
    let song_name = smf::events(h.tracks[0])
        .filter_map(Result::ok)
        .find_map(|(_, e)| match e {
            Ev::Meta(0x03, d) => Some(d.to_vec()),
            _ => None,
        })
        .unwrap_or_default();

    // Pass 1.
    let scans = par_map(h.tracks.len(), |i| {
        if i == 0 && first_is_conductor {
            scan_conductor(h.tracks[0])
        } else {
            scan_track(i as u32, h.tracks[i], module)
        }
    });
    let scans = scans.into_iter().collect::<Result<Vec<_>>>()?;

    let mut report = Report::default();
    let mut copyright = None;
    let mut cond = Vec::new();
    for s in &scans {
        if copyright.is_none() {
            copyright = s.copyright.clone();
        }
    }
    let copyright = copyright.unwrap_or_default();
    let (cond_name, cond_end) = if first_is_conductor { (scans[0].name.clone(), scans[0].end) } else { (Vec::new(), 0) };
    let mut scans = scans;
    for s in &mut scans {
        cond.append(&mut s.cond);
        report.merge(std::mem::take(&mut s.report));
    }
    cond.sort_unstable_by_key(|c| (c.0, c.1, c.2));
    let cond_end = cond.iter().map(|c| c.0).max().unwrap_or(0).max(cond_end);
    let cond_len: u64 = cond.iter().map(|c| c.3.len() as u64).sum();

    // Sizes.
    let mut scratch = Vec::new();
    let track_size = |scratch: &mut Vec<u8>, name: &[u8], ch: u8, port: u16, conductor: bool, events: u64| -> Result<u32> {
        scratch.clear();
        dms::track_head(scratch, name, ch, port, conductor);
        dms::track_tail(scratch, 0, ppq);
        u32::try_from(scratch.len() as u64 + events)
            .ok()
            .context("a track is too large for a Domino file (over 4 GB uncompressed)")
    };
    let mut total = 0u64;
    scratch.clear();
    dms::song_start(&mut scratch, &song_name, &copyright, ppq);
    dms::song_tail(&mut scratch);
    total += scratch.len() as u64;
    let cond_size = track_size(&mut scratch, &cond_name, 0, 0, true, cond_len)?;
    total += 6 + cond_size as u64;
    let mut sizes = Vec::new();
    for (i, s) in scans.iter().enumerate() {
        if i == 0 && first_is_conductor {
            continue;
        }
        for o in &s.outs {
            let size = track_size(&mut scratch, &s.name, o.ch, s.port, false, o.events_len)?;
            total += 6 + size as u64;
            sizes.push(size);
        }
    }
    let Ok(total32) = u32::try_from(total) else {
        bail!("the song is too large for a Domino file ({} bytes uncompressed; the format's limit is 4 GB)", total)
    };

    // Pass 2.
    let mut header = envelope::SEQUENCE.to_vec();
    header.extend_from_slice(&total32.to_le_bytes());
    let (out, written) = pipe::compress_parallel(out, &header, level, block, |sink| {
        let mut buf = Vec::with_capacity(FLUSH + 4096);
        dms::song_start(&mut buf, &song_name, &copyright, ppq);
        dms::track_header(&mut buf, cond_size);
        dms::track_head(&mut buf, &cond_name, 0, 0, true);
        for c in &cond {
            buf.extend_from_slice(&c.3);
            if buf.len() >= FLUSH {
                sink.write_all(&buf)?;
                buf.clear();
            }
        }
        dms::track_tail(&mut buf, cond_end as u32, ppq);
        let mut size = sizes.iter();
        for (i, s) in scans.iter().enumerate() {
            if i == 0 && first_is_conductor {
                continue;
            }
            for (k, o) in s.outs.iter().enumerate() {
                dms::track_header(&mut buf, *size.next().unwrap());
                dms::track_head(&mut buf, &s.name, o.ch, s.port, false);
                generate_output(h.tracks[i], o.ch, k == 0, o.end, module, &mut buf, sink)?;
                dms::track_tail(&mut buf, o.end as u32, ppq);
            }
        }
        dms::song_tail(&mut buf);
        sink.write_all(&buf)?;
        Ok(())
    })?;
    if written != total {
        bail!("internal error: wrote {written} bytes, expected {total}");
    }
    report.tracks = 1 + scans.iter().enumerate().filter(|(i, _)| !(*i == 0 && first_is_conductor)).map(|(_, s)| s.outs.len()).sum::<usize>();
    report.notes = scans.iter().flat_map(|s| &s.outs).map(|o| o.notes).sum();
    Ok((out, report))
}

fn meta_name(ty: u8) -> String {
    match ty {
        0x00 => "sequence number".into(),
        0x04 => "instrument name".into(),
        0x08 => "program name".into(),
        0x09 => "device name".into(),
        0x54 => "SMPTE offset".into(),
        0x7F => "sequencer-specific event".into(),
        _ => format!("meta event 0x{ty:02X}"),
    }
}
