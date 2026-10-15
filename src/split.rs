//! Splitting a song that is too large for one `.dms` file.
//!
//! A *part* (one output file) covers a tick span and holds *pieces*: output
//! tracks, whole or limited to a tick range. Its conductor holds the global
//! events inside its span plus the state in effect at its start
//! ([`Song::part_cond`]).
//!
//! Planning needs the size of arbitrary tick ranges. Event sizes are summed per
//! cell of a grid (measures, or fixed tick buckets), and the one cell where a cut
//! falls is then examined tick by tick. Memory stays bounded by the grid and by
//! the events of a single cell.

use crate::to_dms::{par_map, Choices, SplitMethod, Song, TimeCut};
use anyhow::{bail, Result};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// Most cells a grid may have (8 bytes of memory each, per histogram).
const MAX_CELLS: usize = 4 << 20;
/// Cells of a tick grid.
const TICK_CELLS: u64 = 1 << 20;

pub(crate) struct Piece {
    /// Output track index.
    pub out: usize,
    /// Events with `from <= tick < to`; `to == u64::MAX` means to the end.
    pub from: u64,
    pub to: u64,
}

pub(crate) struct Part {
    pub from: u64,
    pub to: u64,
    /// Keep absolute positions (otherwise the part is moved to start at 0).
    pub keep: bool,
    pub pieces: Vec<Piece>,
}

impl Part {
    pub fn shift(&self) -> u64 {
        if self.keep {
            0
        } else {
            self.from
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum Src {
    /// Every output track.
    All,
    /// One output track.
    One(usize),
}

/// Answers size questions about tick ranges of a song.
struct Sizer<'s, 'a> {
    song: &'s Song<'a>,
    /// Output tracks that `Src::All` stands for.
    group: Vec<usize>,
    limit: u64,
    cut: TimeCut,
    /// Cell starts, ascending, from 0; the last entry is the song's end.
    grid: Vec<u64>,
    /// Per source: `pre[i]` = event bytes before `grid[i]`.
    prefix: HashMap<Src, Vec<u64>>,
    /// Per source and cell: (tick, bytes) of the events inside the cell.
    cells: HashMap<(Src, usize), Vec<(u64, u64)>>,
    cond_ticks: Vec<u64>,
    cond_pre: Vec<u64>,
}

impl<'s, 'a> Sizer<'s, 'a> {
    fn new(song: &'s Song<'a>, limit: u64, cut: TimeCut, group: Vec<usize>) -> Result<Self> {
        let end = song.end();
        let grid = match cut {
            TimeCut::Measure => measure_grid(song, end)?,
            TimeCut::Tick => {
                let width = end.div_ceil(TICK_CELLS).max(1);
                let mut g: Vec<u64> = (0..).map(|i| i * width).take_while(|&t| t < end).collect();
                g.push(end);
                g
            }
        };
        let mut cond_pre = vec![0];
        for e in &song.cond {
            cond_pre.push(cond_pre.last().unwrap() + e.bytes.len() as u64);
        }
        Ok(Sizer {
            song,
            group,
            limit,
            cut,
            grid,
            prefix: HashMap::new(),
            cells: HashMap::new(),
            cond_ticks: song.cond.iter().map(|e| e.tick).collect(),
            cond_pre,
        })
    }

    /// SMF tracks to walk for `src`, each with which of its outputs count.
    fn sources(&self, src: Src) -> Vec<(usize, Vec<bool>)> {
        let outs: &[usize] = match &src {
            Src::All => &self.group,
            Src::One(j) => std::slice::from_ref(j),
        };
        let mut v: Vec<(usize, Vec<bool>)> = Vec::new();
        for &j in outs {
            let (ti, k) = self.song.outputs[j];
            if v.last().map(|e| e.0) != Some(ti) {
                v.push((ti, vec![false; self.song.scans[ti].outs.len()]));
            }
            v.last_mut().unwrap().1[k] = true;
        }
        v
    }

    fn end(&self) -> u64 {
        *self.grid.last().unwrap()
    }

    /// Cell containing tick `t` (`t` < end).
    fn cell(&self, t: u64) -> usize {
        self.grid.partition_point(|&g| g <= t) - 1
    }

    /// Conductor event bytes before tick `t`.
    fn cond_before(&self, t: u64) -> u64 {
        self.cond_pre[self.cond_ticks.partition_point(|&c| c < t)]
    }

    fn prefix(&mut self, src: Src) -> Result<&Vec<u64>> {
        if !self.prefix.contains_key(&src) {
            let cells = self.grid.len() - 1;
            let sums: Vec<AtomicU64> = (0..cells).map(|_| AtomicU64::new(0)).collect();
            let srcs = self.sources(src);
            let grid = &self.grid;
            let song = self.song;
            let walk = |ti: usize, keep: &[bool]| -> Result<()> {
                // Ticks only grow within a track: accumulate per cell, publish on change.
                let (mut c, mut acc) = (0usize, 0u64);
                song.walk_sizes(ti, u64::MAX, |tick, k, len| {
                    if !keep[k] {
                        return;
                    }
                    while c + 1 < cells && grid[c + 1] <= tick {
                        if acc > 0 {
                            sums[c].fetch_add(acc, Ordering::Relaxed);
                            acc = 0;
                        }
                        c += 1;
                    }
                    acc += len;
                })?;
                sums[c].fetch_add(acc, Ordering::Relaxed);
                Ok(())
            };
            par_map(srcs.len(), |n| walk(srcs[n].0, &srcs[n].1)).into_iter().collect::<Result<()>>()?;
            let mut pre = Vec::with_capacity(cells + 1);
            pre.push(0u64);
            for s in &sums {
                pre.push(pre.last().unwrap() + s.load(Ordering::Relaxed));
            }
            self.prefix.insert(src, pre);
        }
        Ok(&self.prefix[&src])
    }

    /// (tick, bytes) of the events of `src` inside cell `c`, ascending.
    fn cell_events(&mut self, src: Src, c: usize) -> Result<&Vec<(u64, u64)>> {
        if !self.cells.contains_key(&(src, c)) {
            let (lo, hi) = (self.grid[c], self.grid[c + 1]);
            let srcs = self.sources(src);
            let song = self.song;
            let collect = |ti: usize, keep: &[bool]| -> Result<Vec<(u64, u64)>> {
                let mut v: Vec<(u64, u64)> = Vec::new();
                song.walk_sizes(ti, hi, |tick, k, len| {
                    if tick < lo || !keep[k] {
                        return;
                    }
                    match v.last_mut() {
                        Some(last) if last.0 == tick => last.1 += len,
                        _ => v.push((tick, len)),
                    }
                })?;
                Ok(v)
            };
            let mut v = Vec::new();
            for p in par_map(srcs.len(), |n| collect(srcs[n].0, &srcs[n].1)) {
                v.extend(p?);
            }
            v.sort_unstable_by_key(|e| e.0);
            v.dedup_by(|b, a| {
                if a.0 == b.0 {
                    a.1 += b.1;
                    true
                } else {
                    false
                }
            });
            self.cells.insert((src, c), v);
        }
        Ok(&self.cells[&(src, c)])
    }

    /// Event bytes of `src` before tick `t`.
    fn before(&mut self, src: Src, t: u64) -> Result<u64> {
        if t >= self.end() {
            return Ok(*self.prefix(src)?.last().unwrap());
        }
        let c = self.cell(t);
        let base = self.prefix(src)?[c];
        if t == self.grid[c] {
            return Ok(base);
        }
        let partial: u64 = self.cell_events(src, c)?.iter().take_while(|e| e.0 < t).map(|e| e.1).sum();
        Ok(base + partial)
    }

    /// Size of everything from `from` to the end: `base` + conductor (if
    /// `with_cond`) + events of `src`.
    fn rest(&mut self, src: Src, from: u64, base: u64, with_cond: bool) -> Result<u64> {
        let events = self.before(src, u64::MAX)? - self.before(src, from)?;
        let cond = if with_cond { self.cond_before(u64::MAX) - self.cond_before(from) } else { 0 };
        Ok(base + cond + events)
    }

    /// Largest `t > from` such that a part covering `[from, t)` fits: `base` +
    /// conductor events in range (if `with_cond`) + events of `src` in range.
    /// Measure cuts land on measure starts unless a single measure does not fit,
    /// then on the tick where it would overflow. Returns `from` if nothing fits and
    /// `allow_empty`, otherwise fails.
    fn cut(&mut self, src: Src, from: u64, base: u64, with_cond: bool, allow_empty: bool) -> Result<u64> {
        let pf = self.before(src, from)?;
        let cf = self.cond_before(from);
        let c = self.cell(from);
        let size_at = |s: &mut Self, i: usize| -> Result<u64> {
            let t = s.grid[i];
            let cond = if with_cond { s.cond_before(t) - cf } else { 0 };
            Ok(base + cond + s.prefix(src)?[i] - pf)
        };
        // Last grid boundary after `from` that still fits.
        let mut best = None;
        for i in c + 1..self.grid.len() {
            if size_at(self, i)? <= self.limit {
                best = Some(i);
            } else {
                break;
            }
        }
        if best == Some(self.grid.len() - 1) {
            return Ok(u64::MAX); // everything fits
        }
        if let (TimeCut::Measure, Some(i)) = (self.cut, best) {
            return Ok(self.grid[i]);
        }
        // Find the exact tick inside the cell where the part overflows.
        let (cell, lo, mut size) = match best {
            Some(i) => (i, self.grid[i], size_at(self, i)?),
            None => (c, from, base),
        };
        let hi = self.grid[cell + 1];
        let mut ticks: Vec<(u64, u64)> = self.cell_events(src, cell)?.iter().filter(|e| e.0 >= lo).copied().collect();
        if with_cond {
            let a = self.cond_ticks.partition_point(|&t| t < lo);
            let b = self.cond_ticks.partition_point(|&t| t < hi);
            for e in &self.song.cond[a..b] {
                ticks.push((e.tick, e.bytes.len() as u64));
            }
            ticks.sort_by_key(|e| e.0);
        }
        let mut i = 0;
        while i < ticks.len() {
            let tick = ticks[i].0;
            let mut add = 0;
            while i < ticks.len() && ticks[i].0 == tick {
                add += ticks[i].1;
                i += 1;
            }
            if size + add > self.limit {
                if tick > from {
                    return Ok(tick);
                }
                if allow_empty {
                    return Ok(from);
                }
                bail!(
                    "the events at tick {tick} alone need more than the split size of {} bytes; \
                     use a larger --split-size or a track-based split",
                    self.limit
                );
            }
            size += add;
        }
        Ok(hi)
    }
}

/// Measure starts from the time signatures in the conductor, up to `end`.
fn measure_grid(song: &Song, end: u64) -> Result<Vec<u64>> {
    let sigs: Vec<(u64, u64)> = song
        .cond
        .iter()
        .filter_map(|e| e.time_sig().map(|(n, d)| (e.tick, bar_len(song.ppq, n, d))))
        .collect();
    let mut grid = Vec::new();
    let (mut cur, mut bar, mut s) = (0u64, 4 * song.ppq as u64, 0);
    while cur < end {
        while s < sigs.len() && sigs[s].0 <= cur {
            bar = sigs[s].1;
            s += 1;
        }
        grid.push(cur);
        if grid.len() > MAX_CELLS {
            bail!("the song has too many measures to split by measure; split by tick instead");
        }
        let mut next = cur + bar;
        if s < sigs.len() && sigs[s].0 < next {
            next = sigs[s].0; // a signature change starts a new measure
        }
        cur = next;
    }
    grid.push(end);
    Ok(grid)
}

fn bar_len(ppq: u16, num: u8, denom: u8) -> u64 {
    (num.max(1) as u64 * 4 * ppq as u64 / denom.max(1) as u64).max(1)
}

/// Bytes of the state carried into a part starting at `from`: conductor state
/// (tempo, signatures) plus the channel state of the outputs `outs`.
fn carried(song: &Song, from: u64, keep: bool, outs: &[usize]) -> Result<u64> {
    if from == 0 {
        return Ok(0);
    }
    let cond: u64 = song.part_cond(from, from, keep).iter().map(|(_, e)| e.bytes.len() as u64).sum();
    Ok(cond + song.cc_bytes(outs, from)?)
}

/// Plan the files for the output tracks `group` (in file order).
pub(crate) fn plan(
    song: &Song,
    group: &[usize],
    limit: u64,
    method: SplitMethod,
    choices: &mut dyn Choices,
) -> Result<Vec<Part>> {
    match method {
        SplitMethod::Measure | SplitMethod::Tick => {
            let keep = choices.keep_offset();
            let cut = if method == SplitMethod::Measure { TimeCut::Measure } else { TimeCut::Tick };
            plan_time(song, group, limit, cut, keep)
        }
        SplitMethod::Track => plan_tracks(song, group, limit, false, choices),
        SplitMethod::TrackPartial => plan_tracks(song, group, limit, true, choices),
    }
}

/// Split by time: every part holds every track of the group.
fn plan_time(song: &Song, group: &[usize], limit: u64, cut: TimeCut, keep: bool) -> Result<Vec<Part>> {
    let fixed = song.song_fixed() + song.cond_fixed() + group.iter().map(|&j| song.track_fixed(j)).sum::<u64>();
    if fixed > limit {
        bail!("the split size is too small: even a part without events needs {fixed} bytes");
    }
    let mut sz = Sizer::new(song, limit, cut, group.to_vec())?;
    let all = |from: u64, to: u64| -> Vec<Piece> { group.iter().map(|&out| Piece { out, from, to }).collect() };
    let mut parts = Vec::new();
    let mut from = 0;
    loop {
        let base = fixed + carried(song, from, keep, group)?;
        if sz.rest(Src::All, from, base, true)? <= limit {
            parts.push(Part { from, to: u64::MAX, keep, pieces: all(from, u64::MAX) });
            return Ok(parts);
        }
        let t = sz.cut(Src::All, from, base, true, false)?;
        parts.push(Part { from, to: t, keep, pieces: all(from, t) });
        from = t;
    }
}

/// Ask how to cut tracks (and whether to keep offsets) the first time it is needed.
fn ensure<'s, 'a>(
    sizer: &mut Option<Sizer<'s, 'a>>,
    keep: &mut bool,
    song: &'s Song<'a>,
    group: &[usize],
    limit: u64,
    choices: &mut dyn Choices,
) -> Result<()> {
    if sizer.is_none() {
        let cut = choices.partial_cut();
        *keep = choices.keep_offset();
        *sizer = Some(Sizer::new(song, limit, cut, group.to_vec())?);
    }
    Ok(())
}

/// Split by track. `pack`: fill every file, cutting the track at each boundary;
/// otherwise keep tracks whole unless one is too large for a file by itself.
fn plan_tracks<'s, 'a>(
    song: &'s Song<'a>,
    group: &[usize],
    limit: u64,
    pack: bool,
    choices: &mut dyn Choices,
) -> Result<Vec<Part>> {
    let all_cond = song.cond.iter().map(|e| e.bytes.len() as u64).sum::<u64>();
    // A part spanning the whole song carries the whole conductor.
    let whole_base = song.song_fixed() + song.cond_fixed() + all_cond;
    // A part holding only one cut piece carries the conductor of its span.
    let span_base = song.song_fixed() + song.cond_fixed();
    let whole_size = |j: usize| song.track_fixed(j) + song.out(j).events_len;
    if span_base + group.iter().map(|&j| song.track_fixed(j)).max().unwrap_or(0) > limit {
        bail!("the split size is too small to hold even one empty track");
    }

    // Asked the first time a track has to be cut.
    let mut sizer: Option<Sizer> = None;
    let mut keep = true;
    let mut parts = Vec::new();

    // Size of a part holding only output j from tick `from` to the end, and the
    // fixed bytes of such a part (everything but its conductor events and notes).
    let alone_base = |j: usize, from: u64, keep: bool| -> Result<u64> {
        Ok(span_base + carried(song, from, keep, &[j])? + song.track_fixed(j))
    };
    // Split output `j` by time into parts of its own, from tick `from`.
    let alone = |sz: &mut Sizer, j: usize, mut from: u64, keep: bool, parts: &mut Vec<Part>| -> Result<()> {
        loop {
            let base = alone_base(j, from, keep)?;
            if sz.rest(Src::One(j), from, base, true)? <= limit {
                parts.push(Part { from, to: u64::MAX, keep, pieces: vec![Piece { out: j, from, to: u64::MAX }] });
                return Ok(());
            }
            let t = sz.cut(Src::One(j), from, base, true, false)?;
            parts.push(Part { from, to: t, keep, pieces: vec![Piece { out: j, from, to: t }] });
            from = t;
        }
    };

    if !pack {
        let mut cur: Vec<Piece> = Vec::new();
        let mut size = whole_base;
        for &j in group {
            let s = whole_size(j);
            if whole_base + s > limit {
                if !cur.is_empty() {
                    parts.push(Part { from: 0, to: u64::MAX, keep: true, pieces: std::mem::take(&mut cur) });
                }
                size = whole_base;
                ensure(&mut sizer, &mut keep, song, group, limit, choices)?;
                alone(sizer.as_mut().unwrap(), j, 0, keep, &mut parts)?;
            } else if size + s > limit {
                parts.push(Part { from: 0, to: u64::MAX, keep: true, pieces: std::mem::take(&mut cur) });
                cur.push(Piece { out: j, from: 0, to: u64::MAX });
                size = whole_base + s;
            } else {
                cur.push(Piece { out: j, from: 0, to: u64::MAX });
                size += s;
            }
        }
        if !cur.is_empty() {
            parts.push(Part { from: 0, to: u64::MAX, keep: true, pieces: cur });
        }
        return Ok(parts);
    }

    // Pack tightly. `g` indexes `group`; the current track continues from tick `f`.
    let (mut g, mut f) = (0usize, 0u64);
    while g < group.len() {
        let j = group[g];
        // Rest of track j from tick f, including the channel state carried to f.
        let rest_events = if f == 0 {
            song.out(j).events_len
        } else {
            let sz = sizer.as_mut().unwrap();
            sz.before(Src::One(j), u64::MAX)? - sz.before(Src::One(j), f)? + song.cc_bytes(&[j], f)?
        };
        let alone_size = if f == 0 {
            whole_base + song.track_fixed(j) + rest_events
        } else {
            let base = alone_base(j, f, keep)?;
            sizer.as_mut().unwrap().rest(Src::One(j), f, base, true)?
        };
        if alone_size > limit {
            // Not even the rest of this track fits: give it a part of its own.
            ensure(&mut sizer, &mut keep, song, group, limit, choices)?;
            let base = alone_base(j, f, keep)?;
            let t = sizer.as_mut().unwrap().cut(Src::One(j), f, base, true, false)?;
            parts.push(Part { from: f, to: t, keep, pieces: vec![Piece { out: j, from: f, to: t }] });
            f = t;
            continue;
        }
        // The rest fits. Adding more tracks makes the part span the whole song.
        let mut pieces = vec![Piece { out: j, from: f, to: u64::MAX }];
        let mut size = whole_base + song.track_fixed(j) + rest_events;
        let first_from = f;
        g += 1;
        f = 0;
        if size <= limit {
            while g < group.len() {
                let j = group[g];
                let s = whole_size(j);
                if size + s <= limit {
                    pieces.push(Piece { out: j, from: 0, to: u64::MAX });
                    size += s;
                    g += 1;
                    continue;
                }
                // Fill the rest of this file with the start of track j.
                let base = size + song.track_fixed(j);
                if base < limit {
                    ensure(&mut sizer, &mut keep, song, group, limit, choices)?;
                    let sz = sizer.as_mut().unwrap();
                    let t = sz.cut(Src::One(j), 0, base, false, true)?;
                    if t > 0 && sz.before(Src::One(j), t)? > 0 {
                        pieces.push(Piece { out: j, from: 0, to: t });
                        f = t;
                    }
                }
                break;
            }
        }
        let from = if pieces.len() == 1 { first_from } else { 0 };
        parts.push(Part { from, to: u64::MAX, keep: keep || from == 0, pieces });
    }
    Ok(parts)
}
