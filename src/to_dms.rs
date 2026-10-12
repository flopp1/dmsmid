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

/// How to read a note-on with velocity 0.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Vel0 {
    /// A note-off, as the MIDI standard says (and as Domino imports it).
    #[default]
    NoteOff,
    /// A silent note that ends at its own later note-off. Some black MIDI tools
    /// write notes this way: every note-on, whatever its velocity, has its own `8n`.
    SilentNote,
    /// Per output track, whichever of the two leaves fewer notes unpaired.
    Auto,
}

/// What the two readings of velocity-0 note-ons would give, for choosing one.
#[derive(Clone, Copy, Debug, Default)]
pub struct Vel0Summary {
    /// Velocity-0 note-ons in the file.
    pub vel0_note_ons: usize,
    /// Note-offs without a note and notes without a note-off, under each reading.
    pub unpaired_as_note_off: usize,
    pub unpaired_as_silent_note: usize,
    /// Output tracks where the silent-note reading pairs better.
    pub tracks_better_as_silent: usize,
}

impl Vel0Summary {
    /// True when the readings disagree somewhere, i.e. the choice changes the result
    /// beyond turning ordinary velocity-0 note-offs into notes.
    pub fn ambiguous(&self) -> bool {
        self.tracks_better_as_silent > 0
    }
}

/// What one channel message becomes.
enum Midi {
    NoteOn { key: u8, vel: u8 },
    NoteOff,
    Control { id: u16, gate: i32, value: i32 },
    Program(u8),
    Dropped(String),
}

/// `vel0_notes`: a velocity-0 note-on starts a (silent) note that ends at its own
/// note-off, instead of being a note-off itself (see [`Pairing`]).
#[inline]
fn midi(st: u8, d1: u8, d2: u8, module: &Module, vel0_notes: bool) -> Midi {
    match st & 0xF0 {
        0x90 if d2 > 0 || vel0_notes => Midi::NoteOn { key: d1, vel: d2 },
        0x80 | 0x90 => Midi::NoteOff,
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
    /// Velocity-0 note-ons are silent notes, not note-offs (decided after pass 1).
    vel0_notes: bool,
    events_len: u64,
    last_tick: u64,
    end: u64,
    /// End of the track before notes are counted (FF 2F or the last other event).
    base_end: u64,
    /// Note pairing as [note-off reading, silent-note reading].
    readings: [PairStats; 2],
}

/// The outcome of a [`Pairing`].
#[derive(Clone, Copy, Debug, Default)]
struct PairStats {
    notes: usize,
    silent: usize,
    unmatched_offs: usize,
    unclosed: usize,
    long: usize,
    last_on: u64,
}

impl PairStats {
    fn unpaired(&self) -> usize {
        self.unmatched_offs + self.unclosed
    }
}

/// Simulates note pairing under one reading of velocity-0 note-ons:
/// - spec: a velocity-0 note-on is a note-off (what Domino does);
/// - silent notes: a velocity-0 note-on starts a silent note that ends at a
///   later explicit note-off. Black MIDI tools write notes this way, pairing
///   every note-on, whatever its velocity, with its own `8n` note-off.
///
/// Pass 1 runs both; the reading is chosen afterwards (see [`Vel0`]).
struct Pairing {
    vel0_notes: bool,
    open: Vec<VecDeque<u64>>, // per key: start ticks of sounding notes, oldest first
    notes: usize,
    silent: usize,
    unmatched_offs: usize,
    unclosed: usize,
    long: usize,
    last_on: u64,
}

impl Pairing {
    fn new(vel0_notes: bool) -> Self {
        Pairing {
            vel0_notes,
            open: vec![VecDeque::new(); 128],
            notes: 0,
            silent: 0,
            unmatched_offs: 0,
            unclosed: 0,
            long: 0,
            last_on: 0,
        }
    }

    #[inline]
    fn event(&mut self, kind: u8, key: u8, vel: u8, tick: u64) {
        let k = key as usize & 0x7F;
        if kind == 0x90 && (vel > 0 || self.vel0_notes) {
            self.open[k].push_back(tick);
            self.notes += 1;
            self.silent += (vel == 0) as usize;
            self.last_on = tick;
        } else {
            match self.open[k].pop_front() {
                Some(on) if tick - on > MAX_GATE => self.long += 1,
                Some(_) => {}
                None => self.unmatched_offs += 1,
            }
        }
    }

    /// Close notes still sounding at the end of the track.
    fn finish(mut self, base_end: u64) -> PairStats {
        let end = base_end.max(self.last_on);
        for on in self.open.iter().flatten() {
            self.unclosed += 1;
            if end - on > MAX_GATE {
                self.long += 1;
            }
        }
        PairStats {
            notes: self.notes,
            silent: self.silent,
            unmatched_offs: self.unmatched_offs,
            unclosed: self.unclosed,
            long: self.long,
            last_on: self.last_on,
        }
    }
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
    // Per output: note pairing under both readings of velocity-0 note-ons.
    let mut pairs: Vec<[Pairing; 2]> = Vec::new();
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
                    pairs.push([Pairing::new(false), Pairing::new(true)]);
                }
                let o = slot[ch as usize] as usize;
                let kind = st & 0xF0;
                if kind == 0x80 || kind == 0x90 {
                    tick32(tick)?;
                    for p in &mut pairs[o] {
                        p.event(kind, d1, d2, tick);
                    }
                    continue;
                }
                let len = match midi(st, d1, d2, module, false) {
                    Midi::NoteOn { .. } | Midi::NoteOff => unreachable!(),
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
        pairs.push([Pairing::new(false), Pairing::new(true)]);
    }
    if s.outs.len() > 1 {
        s.report.warn("MIDI track with several channels (split into one Domino track per channel)");
    }
    s.outs[0].events_len += extra_len;
    s.outs[0].last_tick = s.outs[0].last_tick.max(extra_last);
    for (o, [spec, silent]) in s.outs.iter_mut().zip(pairs) {
        o.base_end = s.end.max(o.last_tick);
        o.readings = [spec.finish(o.base_end), silent.finish(o.base_end)];
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

/// Apply the chosen reading of velocity-0 note-ons to one output track.
fn finalize(o: &mut OutScan, vel0: Vel0, report: &mut Report) -> Result<()> {
    let [spec, silent] = o.readings;
    o.vel0_notes = match vel0 {
        Vel0::NoteOff => false,
        Vel0::SilentNote => silent.silent > 0,
        Vel0::Auto => silent.silent > 0 && silent.unpaired() < spec.unpaired(),
    };
    let p = if o.vel0_notes { silent } else { spec };
    o.last_tick = o.last_tick.max(p.last_on);
    o.end = o.base_end.max(p.last_on);
    tick32(o.end)?;
    o.notes = p.notes;
    o.events_len += (p.notes * NOTE_LEN) as u64;
    report.warn_n("note-off without a matching note-on (ignored)", p.unmatched_offs);
    report.warn_n("note without a note-off (ended at the end of its track)", p.unclosed);
    report.warn_n("note longer than 65535 ticks (shortened, as Domino does)", p.long);
    if o.vel0_notes {
        report.warn_n("velocity-0 note-on read as a silent note", p.silent);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Pass 2: generate

/// Second cursor over a track that finds note-offs ahead of the writer, for every
/// channel of the track at once. Pairing is first-in-first-out per (channel, key),
/// exactly as in pass 1.
struct Lookahead<'a> {
    it: Events<'a>,
    vel0_notes: [bool; 16],
    idx: u64,
    open: Vec<u32>,              // per channel × key
    offs: Vec<VecDeque<u64>>,    // per channel × key: note-off ticks found ahead
}

impl<'a> Lookahead<'a> {
    fn new(track: &'a [u8], vel0_notes: [bool; 16]) -> Self {
        Lookahead { it: smf::events(track), vel0_notes, idx: 0, open: vec![0; 16 * 128], offs: vec![VecDeque::new(); 16 * 128] }
    }

    fn step(&mut self) -> Result<bool> {
        let Some(e) = self.it.next() else { return Ok(false) };
        let (tick, ev) = e?;
        self.idx += 1;
        if let Ev::Midi(st, key, vel) = ev {
            let ch = (st & 0x0F) as usize;
            let k = ch * 128 + (key as usize & 0x7F);
            match st & 0xF0 {
                0x90 if vel > 0 || self.vel0_notes[ch] => self.open[k] += 1,
                0x80 | 0x90 if self.open[k] > 0 => {
                    self.open[k] -= 1;
                    self.offs[k].push_back(tick);
                }
                _ => {}
            }
        }
        Ok(true)
    }

    /// Note-off tick for the note-on that is event number `idx` (1-based) of the track.
    fn off_for(&mut self, ch: u8, key: u8, idx: u64) -> Result<Option<u64>> {
        while self.idx < idx && self.step()? {}
        let k = (ch as usize & 0x0F) * 128 + (key as usize & 0x7F);
        while self.offs[k].is_empty() && self.step()? {}
        Ok(self.offs[k].pop_front())
    }
}

const FLUSH: usize = 64 * 1024;

/// Write all Domino tracks made from one SMF track, in one pass over it.
/// The first output goes straight to the main stream; the others (one per
/// further channel) are built as segments and spliced in after it.
#[allow(clippy::too_many_arguments)]
fn generate_track(
    track: &[u8],
    scan: &Scan,
    sizes: &[u32],
    ppq: u16,
    module: &Module,
    sink: &mut pipe::ParSink,
) -> Result<()> {
    let outs = &scan.outs;
    let mut route = [usize::MAX; 16];
    let mut vel0 = [false; 16];
    for (k, o) in outs.iter().enumerate() {
        route[o.ch as usize] = k;
        vel0[o.ch as usize] = o.vel0_notes;
    }
    let mut bufs: Vec<Vec<u8>> = (0..outs.len()).map(|_| Vec::with_capacity(FLUSH + 4096)).collect();
    let mut segs: Vec<pipe::Segment> = (1..outs.len()).map(|_| sink.segment()).collect();
    let flush = |k: usize, buf: &mut Vec<u8>, segs: &mut Vec<pipe::Segment>, sink: &mut pipe::ParSink| -> Result<()> {
        if k == 0 {
            sink.write_all(buf)?;
        } else {
            sink.write_segment(&mut segs[k - 1], buf)?;
        }
        buf.clear();
        Ok(())
    };
    for (k, o) in outs.iter().enumerate() {
        dms::track_header(&mut bufs[k], sizes[k]);
        dms::track_head(&mut bufs[k], &scan.name, o.ch, scan.port, false);
    }

    let mut la = Lookahead::new(track, vel0);
    for (i, e) in smf::events(track).enumerate() {
        let (tick, ev) = e?;
        let pos = tick as u32; // checked in pass 1
        let k = match ev {
            Ev::Midi(st, d1, d2) => {
                let ch = st & 0x0F;
                let k = route[ch as usize];
                let (o, buf) = (&outs[k], &mut bufs[k]);
                match midi(st, d1, d2, module, o.vel0_notes) {
                    Midi::NoteOn { key, vel } => {
                        let gate = match la.off_for(ch, key, i as u64 + 1)? {
                            Some(off) => off - tick,
                            None => o.end - tick,
                        };
                        dms::note(buf, pos, key, vel, gate.min(MAX_GATE) as u32);
                    }
                    Midi::Control { id, gate, value } => dms::control(buf, pos, id, gate as f32, value as f32),
                    Midi::Program(p) => dms::program(buf, pos, p),
                    Midi::NoteOff | Midi::Dropped(_) => continue,
                }
                k
            }
            Ev::Meta(0x02 | 0x03 | 0x20 | 0x21 | 0x2F, _) => continue,
            ev if is_tempo_map(&ev) => continue,
            ev => {
                // Other meta events belong to the first output.
                let _ = meta_event(&mut bufs[0], &ev, pos);
                0
            }
        };
        if bufs[k].len() >= FLUSH {
            flush(k, &mut bufs[k], &mut segs, sink)?;
        }
    }
    for (k, o) in outs.iter().enumerate() {
        dms::track_tail(&mut bufs[k], o.end as u32, ppq);
        flush(k, &mut bufs[k], &mut segs, sink)?;
    }
    sink.splice(segs)?;
    Ok(())
}

// ---------------------------------------------------------------------------

/// Convert SMF bytes into a complete `.dms` file written to `out`.
///
/// If the file has velocity-0 note-ons, `vel0` is asked how to read them once the
/// file has been scanned (it is not called otherwise).
pub fn convert<W: Write + Send>(
    data: &[u8],
    module: &Module,
    out: W,
    vel0: impl FnOnce(&Vel0Summary) -> Vel0,
) -> Result<(W, Report)> {
    convert_with(data, module, out, vel0, Compression::default(), pipe::BLOCK)
}

pub fn convert_with<W: Write + Send>(
    data: &[u8],
    module: &Module,
    out: W,
    vel0: impl FnOnce(&Vel0Summary) -> Vel0,
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

    // Velocity-0 note-ons: summarise both readings and let the caller choose.
    let regular = |i: usize| !(i == 0 && first_is_conductor);
    let mut summary = Vel0Summary::default();
    for (_, s) in scans.iter().enumerate().filter(|(i, _)| regular(*i)) {
        for o in &s.outs {
            let [spec, silent] = o.readings;
            summary.vel0_note_ons += silent.silent;
            summary.unpaired_as_note_off += spec.unpaired();
            summary.unpaired_as_silent_note += silent.unpaired();
            summary.tracks_better_as_silent += (silent.silent > 0 && silent.unpaired() < spec.unpaired()) as usize;
        }
    }
    let vel0 = if summary.vel0_note_ons > 0 { vel0(&summary) } else { Vel0::NoteOff };
    for (_, s) in scans.iter_mut().enumerate().filter(|(i, _)| regular(*i)) {
        for o in &mut s.outs {
            finalize(o, vel0, &mut report)?;
        }
    }
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
        sink.write_all(&buf)?;
        buf.clear();
        let mut done = 0;
        for (i, s) in scans.iter().enumerate() {
            if i == 0 && first_is_conductor {
                continue;
            }
            generate_track(h.tracks[i], s, &sizes[done..done + s.outs.len()], ppq, module, sink)?;
            done += s.outs.len();
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
