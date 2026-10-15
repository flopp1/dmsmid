//! Standard MIDI File → Domino file(s), mimicking Domino's own MIDI import.
//!
//! - Tempo, time signature and key signature from any track go to the conductor
//!   track. SMF track 0 *is* the conductor when it has no channel messages;
//!   otherwise an empty conductor is created in front.
//! - Domino holds one channel per track, so an SMF track using several channels
//!   (e.g. every format-0 file) is split into one Domino track per channel.
//! - A song too large for one `.dms` file can be split into several (see [`crate::split`]).
//!
//! Passes over the memory-mapped input, none of which copies it:
//! 1. every SMF track is scanned in parallel to learn the exact size of each
//!    output track (Domino chunks carry their size up front), the tempo map and
//!    all warnings;
//! 2. only when splitting: the split is planned from size histograms;
//! 3. each output file is generated in order and compressed on all cores
//!    ([`pipe::compress_parallel`]). A note's length is found by a second cursor
//!    reading ahead in the same track, so nothing is buffered per note.

use crate::dms::{self, CONTROL_LEN, NOTE_LEN, PROGRAM_LEN, TEXT_EVENTS};
use crate::envelope;
use crate::module::{Module, ID_CHANNEL_PRESSURE, ID_PITCH_BEND};
use crate::pipe;
use crate::progress::Progress;
use crate::smf::{self, Ev, Events};
use crate::split::{self, Part, Piece};
use crate::Report;
use anyhow::{bail, Context, Result};
use flate2::Compression;
use std::collections::{HashMap, VecDeque};
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

const MAX_GATE: u64 = 65535; // Domino caps note length on import

/// Largest uncompressed size a `.dms` file can declare (its size field is 32-bit).
pub const MAX_UNPACKED: u64 = u32::MAX as u64;

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

/// How to split a song that is too large for one `.dms` file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SplitMethod {
    /// Time-based: cut at the last whole measure that fits (by tick inside a measure
    /// that is too large on its own).
    Measure,
    /// Time-based: cut at the exact tick where the file would overflow.
    Tick,
    /// Track-based, whole tracks only. A track too large for a file by itself is
    /// split by time into files of its own.
    Track,
    /// Track-based, packing files tightly: the track at each file boundary is split by time.
    TrackPartial,
}

/// How to cut a track by time in the track-based methods.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TimeCut {
    Measure,
    Tick,
}

/// Decisions the conversion may need, asked only when they matter.
pub trait Choices {
    /// How to read velocity-0 note-ons; asked only if the file has any.
    fn vel0(&mut self, summary: &Vel0Summary) -> Vel0;
    /// The song needs `size` bytes but a file may hold `limit`: how to split it
    /// (`None` refuses, and the conversion fails).
    fn split(&mut self, size: u64, limit: u64) -> Option<SplitMethod>;
    /// When splitting by time: keep each part's absolute positions, so the empty
    /// time before its start stays (`true`), or move the part to start at 0.
    fn keep_offset(&mut self) -> bool;
    /// Track-based splits: how to cut a track that has to be split.
    fn partial_cut(&mut self) -> TimeCut;
}

/// Fixed answers, for scripted use and tests.
#[derive(Clone, Copy, Debug)]
pub struct Answers {
    pub vel0: Vel0,
    pub split: Option<SplitMethod>,
    pub keep_offset: bool,
    pub partial_cut: TimeCut,
}

impl Default for Answers {
    fn default() -> Self {
        Answers { vel0: Vel0::NoteOff, split: None, keep_offset: true, partial_cut: TimeCut::Measure }
    }
}

impl Choices for Answers {
    fn vel0(&mut self, _: &Vel0Summary) -> Vel0 {
        self.vel0
    }
    fn split(&mut self, _: u64, _: u64) -> Option<SplitMethod> {
        self.split
    }
    fn keep_offset(&mut self) -> bool {
        self.keep_offset
    }
    fn partial_cut(&mut self) -> TimeCut {
        self.partial_cut
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// Largest uncompressed size of one output file; a larger song is split.
    pub split_size: u64,
    /// Write one file per this many Domino tracks (counted after multi-channel
    /// tracks are separated), each with the whole conductor.
    pub track_split: Option<usize>,
    pub level: Compression,
    pub block: usize,
}

impl Default for Options {
    fn default() -> Self {
        Options { split_size: MAX_UNPACKED, track_split: None, level: Compression::default(), block: pipe::BLOCK }
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

/// Meta events that never become track events (song/track properties).
fn is_property(e: &Ev) -> bool {
    matches!(e, Ev::Meta(0x02 | 0x03 | 0x20 | 0x21 | 0x2F, _))
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

/// Event iterator that reports bytes consumed to a [`Progress`].
struct Tracked<'a, 'p> {
    it: Events<'a>,
    last: usize,
    n: u32,
    progress: &'p Progress,
}

impl<'a, 'p> Tracked<'a, 'p> {
    fn new(track: &'a [u8], progress: &'p Progress) -> Self {
        Tracked { it: smf::events(track), last: 0, n: 0, progress }
    }

    /// Count the rest of the track as done (after stopping early).
    fn finish(&mut self, track_len: usize) {
        self.progress.add((track_len - self.last) as u64);
        self.last = track_len;
    }
}

impl<'a> Iterator for Tracked<'a, '_> {
    type Item = Result<(u64, Ev<'a>)>;
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let r = self.it.next();
        self.n += 1;
        if self.n == 1 << 14 || r.is_none() {
            let o = self.it.offset();
            self.progress.add((o - self.last) as u64);
            self.last = o;
            self.n = 0;
        }
        r
    }
}

/// A channel event replayed at the start of a track piece that begins mid-song,
/// so the track sounds as it would at that point (see [`Song::cc_state`]).
#[derive(Clone, Copy, Debug)]
pub(crate) enum Carried {
    Control { id: u16, gate: i32, value: i32 },
    Program(u8),
}

impl Carried {
    pub fn len(&self) -> u64 {
        match self {
            Carried::Control { .. } => CONTROL_LEN as u64,
            Carried::Program(_) => PROGRAM_LEN as u64,
        }
    }

    fn write(&self, buf: &mut Vec<u8>, pos: u32) {
        match *self {
            Carried::Control { id, gate, value } => dms::control(buf, pos, id, gate as f32, value as f32),
            Carried::Program(p) => dms::program(buf, pos, p),
        }
    }
}

/// Controllers that are momentary commands rather than state: data increment and
/// decrement, all sound off, all notes off and the mode messages. Replaying them
/// at a piece's start would act again, so they are not carried.
fn is_momentary_cc(cc: u8) -> bool {
    matches!(cc, 96 | 97 | 120 | 123..=127)
}

// ---------------------------------------------------------------------------
// Pass 1: scan

#[derive(Default)]
pub(crate) struct OutScan {
    pub ch: u8,
    pub notes: usize,
    /// Velocity-0 note-ons are silent notes, not note-offs (decided after pass 1).
    pub vel0_notes: bool,
    /// Bytes of all this output's events.
    pub events_len: u64,
    last_tick: u64,
    /// Position of the end of the track (chunk 2009).
    pub end: u64,
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
pub(crate) struct Scan {
    pub name: Vec<u8>,
    pub port: u16,
    pub outs: Vec<OutScan>,
    /// Tempo-map events (and, for a conductor track 0, all its events).
    cond: Vec<CondEvent>,
    copyright: Option<Vec<u8>>,
    end: u64,
    report: Report,
}

impl Scan {
    /// Channel → output index, and channel → velocity-0 reading.
    fn routes(&self) -> ([usize; 16], [bool; 16]) {
        let (mut route, mut vel0) = ([usize::MAX; 16], [false; 16]);
        for (k, o) in self.outs.iter().enumerate() {
            route[o.ch as usize] = k;
            vel0[o.ch as usize] = o.vel0_notes;
        }
        (route, vel0)
    }
}

/// A conductor event: its tick, its order in the source, and its Domino bytes.
pub(crate) struct CondEvent {
    pub tick: u64,
    order: (u32, u32),
    pub bytes: Vec<u8>,
}

impl CondEvent {
    pub fn id(&self) -> u16 {
        u16::from_le_bytes([self.bytes[0], self.bytes[1]])
    }

    /// (numerator, denominator) of a time signature event.
    pub fn time_sig(&self) -> Option<(u8, u8)> {
        (self.id() == dms::EV_TIME_SIG).then(|| (self.bytes[22], self.bytes[29]))
    }
}

fn scan_conductor(track: &[u8], progress: &Progress) -> Result<Scan> {
    let mut s = Scan::default();
    let mut buf = Vec::new();
    for (idx, e) in Tracked::new(track, progress).enumerate() {
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
                    Ok(()) => s.cond.push(CondEvent { tick, order: (0, idx as u32), bytes: buf.clone() }),
                    Err(w) => s.report.warn(w),
                }
            }
        }
    }
    Ok(s)
}

fn scan_track(ti: u32, track: &[u8], module: &Module, progress: &Progress) -> Result<Scan> {
    let mut s = Scan::default();
    let mut buf = Vec::new();
    // Per output: note pairing under both readings of velocity-0 note-ons.
    let mut pairs: Vec<[Pairing; 2]> = Vec::new();
    let mut slot = [u8::MAX; 16]; // channel → output index
    // Meta events go to the first output, which may not be known yet.
    let (mut extra_len, mut extra_last) = (0u64, 0u64);
    let mut prefix = None;

    for (idx, e) in Tracked::new(track, progress).enumerate() {
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
                    Ok(()) => s.cond.push(CondEvent { tick, order: (ti, idx as u32), bytes: buf.clone() }),
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
pub(crate) fn par_map<T: Send>(n: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
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
// The scanned song

/// Everything known after pass 1.
pub(crate) struct Song<'a> {
    pub tracks: Vec<&'a [u8]>,
    pub module: &'a Module,
    pub ppq: u16,
    song_name: Vec<u8>,
    copyright: Vec<u8>,
    cond_name: Vec<u8>,
    /// Conductor events, sorted by tick (then source order).
    pub cond: Vec<CondEvent>,
    pub cond_end: u64,
    /// One per SMF track; empty for a conductor track 0.
    pub scans: Vec<Scan>,
    /// Domino output tracks in file order: (SMF track, output index within it).
    pub outputs: Vec<(usize, usize)>,
}

impl Song<'_> {
    pub fn out(&self, j: usize) -> &OutScan {
        let (i, k) = self.outputs[j];
        &self.scans[i].outs[k]
    }

    /// Song header and trailer bytes.
    pub fn song_fixed(&self) -> u64 {
        let mut b = Vec::new();
        dms::song_start(&mut b, &self.song_name, &self.copyright, self.ppq);
        dms::song_tail(&mut b);
        b.len() as u64
    }

    /// Conductor track chunk without its events, header included.
    pub fn cond_fixed(&self) -> u64 {
        let mut b = Vec::new();
        dms::track_head(&mut b, &self.cond_name, 0, 0, true);
        dms::track_tail(&mut b, 0, self.ppq);
        6 + b.len() as u64
    }

    /// Output track chunk `j` without its events, header included.
    pub fn track_fixed(&self, j: usize) -> u64 {
        let (i, k) = self.outputs[j];
        let s = &self.scans[i];
        let mut b = Vec::new();
        dms::track_head(&mut b, &s.name, s.outs[k].ch, s.port, false);
        dms::track_tail(&mut b, 0, self.ppq);
        6 + b.len() as u64
    }

    /// One past the last tick anything happens at.
    pub fn end(&self) -> u64 {
        (0..self.outputs.len()).map(|j| self.out(j).end).max().unwrap_or(0).max(self.cond_end) + 1
    }

    /// Conductor events of a part: the state in effect at its start (tempo, key
    /// signature, and time signatures: all earlier ones when keeping the offset, so
    /// bars line up, otherwise the last), then every event inside its span.
    /// Returns (absolute tick, event).
    pub fn part_cond(&self, from: u64, to: u64, keep: bool) -> Vec<(u64, &CondEvent)> {
        let mut out = Vec::new();
        let start = self.cond.partition_point(|e| e.tick < from);
        if from > 0 {
            let at = &self.cond[start..self.cond[start..].partition_point(|e| e.tick == from) + start];
            let has_at = |id: u16| at.iter().any(|e| e.id() == id);
            let before = &self.cond[..start];
            let last = |id: u16| before.iter().rev().find(|e| e.id() == id);
            if keep {
                out.extend(before.iter().filter(|e| e.id() == dms::EV_TIME_SIG).map(|e| (e.tick, e)));
            } else if !has_at(dms::EV_TIME_SIG) {
                out.extend(last(dms::EV_TIME_SIG).map(|e| (from, e)));
            }
            for id in [dms::EV_TEMPO, dms::EV_KEY_SIG] {
                if !has_at(id) {
                    out.extend(last(id).map(|e| (from, e)));
                }
            }
        }
        let stop = self.cond.partition_point(|e| e.tick < to);
        out.extend(self.cond[start..stop.max(start)].iter().map(|e| (e.tick, e)));
        out
    }

    /// Channel state of each output of SMF track `ti` just before tick `from`: the
    /// last value of every controller (per CCM ID and, for poly pressure, key),
    /// program, pitch bend and pressure, in their original order, so RPN/NRPN
    /// selections stay ahead of their data entry and bank selects ahead of the
    /// program change. Momentary commands are left out ([`is_momentary_cc`]).
    pub fn cc_state(&self, ti: usize, from: u64) -> Result<Vec<Vec<Carried>>> {
        let scan = &self.scans[ti];
        let (route, vel0) = scan.routes();
        let mut last: Vec<HashMap<(bool, u16, i32), (usize, Carried)>> = vec![HashMap::new(); scan.outs.len()];
        if from > 0 {
            for (n, e) in smf::events(self.tracks[ti]).enumerate() {
                let (tick, ev) = e?;
                if tick >= from {
                    break;
                }
                let Ev::Midi(st, d1, d2) = ev else { continue };
                let ch = (st & 0x0F) as usize;
                let (key, c) = match midi(st, d1, d2, self.module, vel0[ch]) {
                    Midi::Control { .. } if st & 0xF0 == 0xB0 && is_momentary_cc(d1) => continue,
                    Midi::Control { id, gate, value } => ((false, id, gate), Carried::Control { id, gate, value }),
                    Midi::Program(p) => ((true, 0, 0), Carried::Program(p)),
                    _ => continue,
                };
                last[route[ch]].insert(key, (n, c));
            }
        }
        Ok(last
            .into_iter()
            .map(|m| {
                let mut v: Vec<(usize, Carried)> = m.into_values().collect();
                v.sort_unstable_by_key(|e| e.0);
                v.into_iter().map(|e| e.1).collect()
            })
            .collect())
    }

    /// Bytes of the channel state carried into pieces of outputs `outs` starting at `from`.
    pub fn cc_bytes(&self, outs: &[usize], from: u64) -> Result<u64> {
        if from == 0 {
            return Ok(0);
        }
        let mut tracks: Vec<usize> = outs.iter().map(|&j| self.outputs[j].0).collect();
        tracks.sort_unstable();
        tracks.dedup();
        let sums = par_map(tracks.len(), |n| -> Result<u64> {
            let ti = tracks[n];
            let state = self.cc_state(ti, from)?;
            Ok(outs
                .iter()
                .filter(|&&j| self.outputs[j].0 == ti)
                .map(|&j| state[self.outputs[j].1].iter().map(Carried::len).sum::<u64>())
                .sum())
        });
        sums.into_iter().sum()
    }

    /// Walk SMF track `ti` (a regular track) and report, for every Domino event it
    /// produces, its tick, output index and size in bytes. Stops at tick `stop`.
    /// This is the single definition of event sizes used for splitting.
    pub fn walk_sizes(&self, ti: usize, stop: u64, mut f: impl FnMut(u64, usize, u64)) -> Result<()> {
        let scan = &self.scans[ti];
        let (route, vel0) = scan.routes();
        let mut scratch = Vec::new();
        for e in smf::events(self.tracks[ti]) {
            let (tick, ev) = e?;
            if tick >= stop {
                break;
            }
            match ev {
                Ev::Midi(st, d1, d2) => {
                    let ch = (st & 0x0F) as usize;
                    let len = match midi(st, d1, d2, self.module, vel0[ch]) {
                        Midi::NoteOn { .. } => NOTE_LEN,
                        Midi::Control { .. } => CONTROL_LEN,
                        Midi::Program(_) => PROGRAM_LEN,
                        Midi::NoteOff | Midi::Dropped(_) => continue,
                    };
                    f(tick, route[ch], len as u64);
                }
                ev if is_property(&ev) || is_tempo_map(&ev) => {}
                ev => {
                    scratch.clear();
                    if meta_event(&mut scratch, &ev, 0).is_ok() {
                        f(tick, 0, scratch.len() as u64);
                    }
                }
            }
        }
        Ok(())
    }

    /// Exact event bytes of each piece of `part`.
    pub fn piece_bytes(&self, part: &Part) -> Result<Vec<u64>> {
        let whole = |p: &Piece| p.from == 0 && p.to == u64::MAX;
        // SMF tracks with partial pieces need counting.
        let mut need: Vec<usize> = part.pieces.iter().filter(|p| !whole(p)).map(|p| self.outputs[p.out].0).collect();
        need.dedup();
        let counted = par_map(need.len(), |n| -> Result<Vec<(usize, u64)>> {
            let ti = need[n];
            let mine: Vec<(usize, &Piece)> = part
                .pieces
                .iter()
                .enumerate()
                .filter(|(_, p)| self.outputs[p.out].0 == ti && !whole(p))
                .collect();
            let stop = mine.iter().map(|(_, p)| p.to).max().unwrap_or(0);
            let mut sums = vec![0u64; mine.len()];
            self.walk_sizes(ti, stop, |tick, k, len| {
                for (n, (_, p)) in mine.iter().enumerate() {
                    if self.outputs[p.out].1 == k && tick >= p.from && tick < p.to {
                        sums[n] += len;
                    }
                }
            })?;
            Ok(mine.iter().map(|(idx, _)| *idx).zip(sums).collect())
        });
        let mut bytes: Vec<u64> = part.pieces.iter().map(|p| if whole(p) { self.out(p.out).events_len } else { 0 }).collect();
        for c in counted {
            for (idx, n) in c? {
                bytes[idx] = n;
            }
        }
        // Channel state carried into pieces starting mid-song: one walk per
        // (SMF track, start tick), shared by all outputs of that track.
        let mut starts: Vec<(usize, u64)> =
            part.pieces.iter().filter(|p| p.from > 0).map(|p| (self.outputs[p.out].0, p.from)).collect();
        starts.sort_unstable();
        starts.dedup();
        let states = par_map(starts.len(), |n| self.cc_state(starts[n].0, starts[n].1));
        let states: HashMap<(usize, u64), Vec<Vec<Carried>>> =
            starts.into_iter().zip(states).map(|(k, v)| v.map(|v| (k, v))).collect::<Result<_>>()?;
        for (idx, p) in part.pieces.iter().enumerate().filter(|(_, p)| p.from > 0) {
            let (ti, k) = self.outputs[p.out];
            bytes[idx] += states[&(ti, p.from)][k].iter().map(Carried::len).sum::<u64>();
        }
        Ok(bytes)
    }
}

// ---------------------------------------------------------------------------
// Generation

/// Second cursor over a track that finds note-offs ahead of the writer, for the
/// written channels of the track at once. Pairing is first-in-first-out per
/// (channel, key), exactly as in pass 1.
struct Lookahead<'a> {
    it: Events<'a>,
    active: [bool; 16],
    vel0_notes: [bool; 16],
    idx: u64,
    open: Vec<u32>,           // per channel × key
    offs: Vec<VecDeque<u64>>, // per channel × key: note-off ticks found ahead
}

impl<'a> Lookahead<'a> {
    fn new(track: &'a [u8], active: [bool; 16], vel0_notes: [bool; 16]) -> Self {
        Lookahead {
            it: smf::events(track),
            active,
            vel0_notes,
            idx: 0,
            open: vec![0; 16 * 128],
            offs: vec![VecDeque::new(); 16 * 128],
        }
    }

    fn step(&mut self) -> Result<bool> {
        let Some(e) = self.it.next() else { return Ok(false) };
        let (tick, ev) = e?;
        self.idx += 1;
        if let Ev::Midi(st, key, vel) = ev {
            let ch = (st & 0x0F) as usize;
            if !self.active[ch] {
                return Ok(true);
            }
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

/// One piece to write: output index within its SMF track, tick range, end position
/// and event bytes.
struct Job {
    k: usize,
    from: u64,
    to: u64,
    end: u64,
    bytes: u64,
}

/// Write the given pieces of one SMF track, in one pass over it. The first piece
/// goes straight to the main stream; the others are built as segments and spliced
/// in after it. Positions are moved back by `shift`.
fn generate_pieces(
    song: &Song,
    ti: usize,
    jobs: &[Job],
    shift: u64,
    sink: &mut pipe::ParSink,
    progress: &Progress,
) -> Result<()> {
    let scan = &song.scans[ti];
    let track = song.tracks[ti];
    let (_, vel0) = scan.routes();
    let mut slot = [usize::MAX; 16]; // channel → job
    let mut active = [false; 16];
    for (n, j) in jobs.iter().enumerate() {
        let ch = scan.outs[j.k].ch as usize;
        slot[ch] = n;
        active[ch] = true;
    }
    let meta_job = jobs.iter().position(|j| j.k == 0);
    let stop = jobs.iter().map(|j| j.to).max().unwrap_or(0);

    let mut bufs: Vec<Vec<u8>> = jobs.iter().map(|_| Vec::with_capacity(FLUSH + 4096)).collect();
    let mut segs: Vec<pipe::Segment> = (1..jobs.len()).map(|_| sink.segment()).collect();
    let flush = |n: usize, buf: &mut Vec<u8>, segs: &mut Vec<pipe::Segment>, sink: &mut pipe::ParSink| -> Result<()> {
        if n == 0 {
            sink.write_all(buf)?;
        } else {
            sink.write_segment(&mut segs[n - 1], buf)?;
        }
        buf.clear();
        Ok(())
    };
    let mut states: HashMap<u64, Vec<Vec<Carried>>> = HashMap::new();
    for (n, j) in jobs.iter().enumerate() {
        let size = song_track_size(song, ti, j.k, j.bytes)?;
        dms::track_header(&mut bufs[n], size);
        dms::track_head(&mut bufs[n], &scan.name, scan.outs[j.k].ch, scan.port, false);
        // A piece starting mid-song first restores the track's channel state.
        if j.from > 0 {
            if !states.contains_key(&j.from) {
                states.insert(j.from, song.cc_state(ti, j.from)?);
            }
            for c in &states[&j.from][j.k] {
                c.write(&mut bufs[n], (j.from - shift) as u32);
            }
        }
    }

    let mut la = Lookahead::new(track, active, vel0);
    let mut it = Tracked::new(track, progress);
    let mut i = 0u64;
    while let Some(e) = it.next() {
        i += 1;
        let (tick, ev) = e?;
        if tick >= stop {
            break;
        }
        let pos = tick.wrapping_sub(shift) as u32; // in range: checked when planning
        let n = match ev {
            Ev::Midi(st, d1, d2) => {
                let ch = st & 0x0F;
                let n = slot[ch as usize];
                if n == usize::MAX {
                    continue;
                }
                let j = &jobs[n];
                let o = &scan.outs[j.k];
                let inside = tick >= j.from && tick < j.to;
                let buf = &mut bufs[n];
                match midi(st, d1, d2, song.module, o.vel0_notes) {
                    Midi::NoteOn { key, vel } => {
                        // Pair every note, written or not, to keep the pairing in step.
                        let off = la.off_for(ch, key, i)?;
                        if !inside {
                            continue;
                        }
                        let gate = match off {
                            Some(off) => off - tick,
                            None => o.end - tick,
                        };
                        dms::note(buf, pos, key, vel, gate.min(MAX_GATE) as u32);
                    }
                    _ if !inside => continue,
                    Midi::Control { id, gate, value } => dms::control(buf, pos, id, gate as f32, value as f32),
                    Midi::Program(p) => dms::program(buf, pos, p),
                    Midi::NoteOff | Midi::Dropped(_) => continue,
                }
                n
            }
            ev if is_property(&ev) || is_tempo_map(&ev) => continue,
            ev => {
                // Other meta events belong to the first output.
                let Some(n) = meta_job else { continue };
                if tick < jobs[n].from || tick >= jobs[n].to {
                    continue;
                }
                let _ = meta_event(&mut bufs[n], &ev, pos);
                n
            }
        };
        if bufs[n].len() >= FLUSH {
            flush(n, &mut bufs[n], &mut segs, sink)?;
        }
    }
    it.finish(track.len());
    for (n, j) in jobs.iter().enumerate() {
        dms::track_tail(&mut bufs[n], (j.end - shift) as u32, song.ppq);
        flush(n, &mut bufs[n], &mut segs, sink)?;
    }
    sink.splice(segs)?;
    Ok(())
}

/// Size field of an output track chunk holding `bytes` of events.
fn song_track_size(song: &Song, ti: usize, k: usize, bytes: u64) -> Result<u32> {
    let j = song.outputs.iter().position(|&o| o == (ti, k)).expect("output exists");
    u32::try_from(song.track_fixed(j) - 6 + bytes)
        .ok()
        .context("a track is too large for a Domino file (over 4 GB uncompressed)")
}

/// Uncompressed size of `part`, given its pieces' event bytes.
pub(crate) fn part_size(song: &Song, part: &Part, piece_bytes: &[u64]) -> u64 {
    let cond: u64 = song.part_cond(part.from, part.to, part.keep).iter().map(|(_, e)| e.bytes.len() as u64).sum();
    song.song_fixed()
        + song.cond_fixed()
        + cond
        + part.pieces.iter().zip(piece_bytes).map(|(p, b)| song.track_fixed(p.out) + b).sum::<u64>()
}

/// Write one output file.
fn write_part<W: Write + Send>(
    song: &Song,
    part: &Part,
    piece_bytes: &[u64],
    total: u64,
    out: W,
    opts: &Options,
    progress: &Progress,
) -> Result<W> {
    let shift = part.shift();
    let mut header = envelope::SEQUENCE.to_vec();
    header.extend_from_slice(&(total as u32).to_le_bytes());
    let (out, written) = pipe::compress_parallel(out, &header, opts.level, opts.block, |sink| {
        let mut buf = Vec::with_capacity(FLUSH + 4096);
        dms::song_start(&mut buf, &song.song_name, &song.copyright, song.ppq);
        let cond = song.part_cond(part.from, part.to, part.keep);
        let cond_bytes: u64 = cond.iter().map(|(_, e)| e.bytes.len() as u64).sum();
        let cond_size = song.cond_fixed() - 6 + cond_bytes;
        dms::track_header(&mut buf, cond_size as u32);
        dms::track_head(&mut buf, &song.cond_name, 0, 0, true);
        for (tick, e) in cond {
            let at = buf.len();
            buf.extend_from_slice(&e.bytes);
            buf[at + 12..at + 16].copy_from_slice(&((tick - shift) as u32).to_le_bytes());
            if buf.len() >= FLUSH {
                sink.write_all(&buf)?;
                buf.clear();
            }
        }
        let cond_end = if part.to == u64::MAX { song.cond_end.max(part.from) } else { part.to };
        dms::track_tail(&mut buf, (cond_end - shift) as u32, song.ppq);
        sink.write_all(&buf)?;
        buf.clear();

        // Pieces, grouped by SMF track (pieces of one track are consecutive).
        let mut p = 0;
        while p < part.pieces.len() {
            let ti = song.outputs[part.pieces[p].out].0;
            let mut jobs = Vec::new();
            while p < part.pieces.len() && song.outputs[part.pieces[p].out].0 == ti {
                let piece = &part.pieces[p];
                let o = song.out(piece.out);
                let end = if piece.to == u64::MAX { o.end.max(piece.from) } else { piece.to };
                jobs.push(Job { k: song.outputs[piece.out].1, from: piece.from, to: piece.to, end, bytes: piece_bytes[p] });
                p += 1;
            }
            generate_pieces(song, ti, &jobs, shift, sink, progress)?;
        }
        dms::song_tail(&mut buf);
        sink.write_all(&buf)?;
        Ok(())
    })?;
    if written != total {
        bail!("internal error: wrote {written} bytes, expected {total}");
    }
    Ok(out)
}

// ---------------------------------------------------------------------------

/// Convert SMF bytes into one or more `.dms` files. `open` supplies the writer for
/// each file, told which file it is (see [`FilePlan`]).
pub fn convert<W: Write + Send>(
    data: &[u8],
    module: &Module,
    opts: &Options,
    choices: &mut dyn Choices,
    mut open: impl FnMut(&FilePlan) -> Result<W>,
    progress: &Progress,
) -> Result<(Vec<W>, Report)> {
    if opts.split_size > MAX_UNPACKED {
        bail!("the split size can be at most {MAX_UNPACKED} bytes (the .dms format's limit)");
    }
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
    progress.start("Scanning", h.tracks.iter().map(|t| t.len() as u64).sum());
    let scans = par_map(h.tracks.len(), |i| {
        if i == 0 && first_is_conductor {
            scan_conductor(h.tracks[0], progress)
        } else {
            scan_track(i as u32, h.tracks[i], module, progress)
        }
    });
    let mut scans = scans.into_iter().collect::<Result<Vec<_>>>()?;

    let mut report = Report::default();
    let copyright = scans.iter().find_map(|s| s.copyright.clone()).unwrap_or_default();
    let (cond_name, cond_end) =
        if first_is_conductor { (std::mem::take(&mut scans[0].name), scans[0].end) } else { (Vec::new(), 0) };
    let mut cond = Vec::new();
    for s in &mut scans {
        cond.append(&mut s.cond);
        report.merge(std::mem::take(&mut s.report));
    }
    cond.sort_unstable_by_key(|c| (c.tick, c.order));

    // Velocity-0 note-ons: summarise both readings and let the caller choose.
    if first_is_conductor {
        scans[0].outs.clear();
    }
    let mut summary = Vel0Summary::default();
    for o in scans.iter().flat_map(|s| &s.outs) {
        let [spec, silent] = o.readings;
        summary.vel0_note_ons += silent.silent;
        summary.unpaired_as_note_off += spec.unpaired();
        summary.unpaired_as_silent_note += silent.unpaired();
        summary.tracks_better_as_silent += (silent.silent > 0 && silent.unpaired() < spec.unpaired()) as usize;
    }
    let vel0 = if summary.vel0_note_ons > 0 { choices.vel0(&summary) } else { Vel0::NoteOff };
    for o in scans.iter_mut().flat_map(|s| &mut s.outs) {
        finalize(o, vel0, &mut report)?;
    }
    let cond_end = cond.iter().map(|c| c.tick).max().unwrap_or(0).max(cond_end);
    let outputs: Vec<(usize, usize)> =
        scans.iter().enumerate().flat_map(|(i, s)| (0..s.outs.len()).map(move |k| (i, k))).collect();
    report.tracks = 1 + outputs.len();
    report.notes = scans.iter().flat_map(|s| &s.outs).map(|o| o.notes).sum();

    let song = Song {
        tracks: h.tracks,
        module,
        ppq,
        song_name,
        copyright,
        cond_name,
        cond,
        cond_end,
        scans,
        outputs,
    };

    // Groups of output tracks: all of them, or N at a time with --track-split.
    let n_out = song.outputs.len();
    let groups: Vec<Vec<usize>> = match opts.track_split {
        Some(0) => bail!("--track-split needs at least 1 track per file"),
        Some(n) => (0..n_out).collect::<Vec<_>>().chunks(n).map(<[usize]>::to_vec).collect(),
        None => vec![(0..n_out).collect()],
    };
    // The overflow split is chosen once and reused for every group that needs it.
    let mut method: Option<SplitMethod> = None;
    let mut outs = Vec::new();
    for group in &groups {
        let tracks = opts.track_split.map(|_| (group.first().map_or(0, |j| j + 1), group.last().map_or(0, |j| j + 1)));
        let whole = Part {
            from: 0,
            to: u64::MAX,
            keep: true,
            pieces: group.iter().map(|&j| Piece { out: j, from: 0, to: u64::MAX }).collect(),
        };
        let whole_bytes: Vec<u64> = group.iter().map(|&j| song.out(j).events_len).collect();
        let total = part_size(&song, &whole, &whole_bytes);
        let parts = if total <= opts.split_size {
            vec![whole]
        } else {
            if method.is_none() {
                method = choices.split(total, opts.split_size);
            }
            let Some(m) = method else {
                let what = match tracks {
                    Some((a, b)) => format!("tracks {a}-{b} need"),
                    None => "the song needs".into(),
                };
                bail!(
                    "{what} {total} bytes but a .dms file can hold at most {} (uncompressed); \
                     choose how to split it with --split measure|tick|track|track-partial",
                    opts.split_size
                );
            };
            progress.start("Planning the split", 0);
            split::plan(&song, group, opts.split_size, m, choices)?
        };

        // Write them.
        let n = parts.len();
        for (i, part) in parts.iter().enumerate() {
            let bytes = song.piece_bytes(part)?;
            let size = part_size(&song, part, &bytes);
            if size > opts.split_size {
                bail!("internal error: part {} needs {size} bytes, over the limit of {}", i + 1, opts.split_size);
            }
            if part.to != u64::MAX && part.to - part.shift() > u32::MAX as u64 {
                bail!("song is too long for a Domino file (tick overflow)");
            }
            let walked: u64 = {
                let mut ts: Vec<usize> = part.pieces.iter().map(|p| song.outputs[p.out].0).collect();
                ts.dedup();
                ts.iter().map(|&t| song.tracks[t].len() as u64).sum()
            };
            let file = FilePlan { tracks, part: (n > 1).then_some((i + 1, n)) };
            progress.start(file.describe("Writing"), walked);
            let out = open(&file)?;
            outs.push(write_part(&song, part, &bytes, size, out, opts, progress)?);
        }
    }
    Ok((outs, report))
}

/// Which output file is being written, for naming it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FilePlan {
    /// With `--track-split`: the 1-based Domino output tracks it holds (first, last).
    pub tracks: Option<(usize, usize)>,
    /// When the song (or track group) is split: part number and part count.
    pub part: Option<(usize, usize)>,
}

impl FilePlan {
    /// The name suffix, e.g. `Track 1-10, part 2 of 3` or `Track 41`; empty for a
    /// plain conversion.
    pub fn suffix(&self) -> String {
        let mut s = Vec::new();
        if let Some((a, b)) = self.tracks {
            // A group of one track is named by that track alone.
            s.push(if a == b { format!("Track {a}") } else { format!("Track {a}-{b}") });
        }
        if let Some((c, d)) = self.part {
            s.push(format!("part {c} of {d}"));
        }
        s.join(", ")
    }

    fn describe(&self, verb: &str) -> String {
        match self.suffix() {
            s if s.is_empty() => verb.to_string(),
            s => format!("{verb} {s}"),
        }
    }
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
