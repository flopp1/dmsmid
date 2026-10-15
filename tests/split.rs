//! Splitting a song across several .dms files.

use dmsmid::chunk::{self, Fields};
use dmsmid::module::Module;
use dmsmid::{envelope, Answers, FilePlan, Options, Progress, SplitMethod, TimeCut, MAX_UNPACKED};
use std::collections::BTreeMap;

const PPQ: u64 = 96;
const BAR44: u64 = 4 * PPQ; // bars 0-3 are 4/4
const BAR34: u64 = 3 * PPQ; // from tick 4 * BAR44 on, 3/4

fn vlq(mut n: u64, out: &mut Vec<u8>) {
    let mut tmp = vec![(n & 0x7F) as u8];
    n >>= 7;
    while n > 0 {
        tmp.push(0x80 | (n & 0x7F) as u8);
        n >>= 7;
    }
    tmp.reverse();
    out.extend(tmp);
}

fn smf(tracks: Vec<Vec<(u64, Vec<u8>)>>) -> Vec<u8> {
    let mut f = b"MThd".to_vec();
    f.extend(6u32.to_be_bytes());
    f.extend(1u16.to_be_bytes());
    f.extend((tracks.len() as u16).to_be_bytes());
    f.extend((PPQ as u16).to_be_bytes());
    for mut t in tracks {
        t.sort_by_key(|e| e.0); // stable
        let mut b = Vec::new();
        let mut last = 0;
        for (tick, ev) in &t {
            vlq(tick - last, &mut b);
            b.extend(ev);
            last = *tick;
        }
        b.extend([0, 0xFF, 0x2F, 0]);
        f.extend(b"MTrk");
        f.extend((b.len() as u32).to_be_bytes());
        f.extend(b);
    }
    f
}

fn name(n: &str) -> (u64, Vec<u8>) {
    let mut e = vec![0xFF, 0x03, n.len() as u8];
    e.extend(n.as_bytes());
    (0, e)
}

fn note(t: &mut Vec<(u64, Vec<u8>)>, tick: u64, ch: u8, key: u8, vel: u8, len: u64) {
    t.push((tick, vec![0x90 | ch, key, vel]));
    t.push((tick + len, vec![0x80 | ch, key, 0]));
}

/// 12 bars: tempo, time and key changes, a marker, four tracks (one with two channels).
fn song() -> Vec<u8> {
    let bar_start = |b: u64| if b <= 4 { b * BAR44 } else { 4 * BAR44 + (b - 4) * BAR34 };
    let end = bar_start(12);
    let tempo = |us: u32| [vec![0xFF, 0x51, 3], us.to_be_bytes()[1..].to_vec()].concat();
    let cond = vec![
        name("Cond"),
        (0, tempo(500_000)),
        (0, vec![0xFF, 0x58, 4, 4, 2, 24, 8]),
        (0, vec![0xFF, 0x59, 2, 0, 0]),
        (500, [vec![0xFF, 0x06, 4], b"Mark".to_vec()].concat()),
        (bar_start(3), tempo(400_000)),
        (bar_start(4), vec![0xFF, 0x58, 4, 3, 2, 24, 8]),
        (bar_start(6), vec![0xFF, 0x59, 2, 2, 0]),
        (bar_start(9), tempo(300_000)),
    ];
    let mut t1 = vec![name("Busy")];
    let mut t = 0;
    while t < end {
        note(&mut t1, t, 0, 60 + (t / 12 % 12) as u8, 100, 30); // overlaps the next ones
        t += 12;
    }
    let mut t2 = vec![name("Bass")];
    // Channel state before the notes: program, an RPN (pitch-bend range) in order,
    // a pitch bend, and momentary commands that must not be replayed.
    t2.push((0, vec![0xC1, 33]));
    for (cc, v) in [(101, 0), (100, 0), (6, 12)] {
        t2.push((0, vec![0xB1, cc, v]));
    }
    t2.push((5, vec![0xB1, 123, 0])); // all notes off
    t2.push((6, vec![0xB1, 96, 0])); // data increment
    t2.push((10, vec![0xE1, 0x00, 0x50])); // pitch bend
    for i in 0..end / 24 {
        note(&mut t2, i * 24, 1, 36, 90, 24);
        t2.push((i * 24, vec![0xB1, 7, (i % 128) as u8]));
    }
    let mut t3 = vec![name("Duo")];
    for i in 0..end / 48 {
        note(&mut t3, i * 48, 2, 72, 80, 40);
        note(&mut t3, i * 48 + 6, 3, 74, 70, 40);
    }
    let mut t4 = vec![name("Sparse")];
    for b in [0, 5, 11] {
        note(&mut t4, bar_start(b), 4, 50, 60, PPQ);
    }
    smf(vec![cond, t1, t2, t3, t4])
}

struct Track {
    name: Vec<u8>,
    ch: u8,
    conductor: bool,
    end: u32,
    /// (id, position, whole event chunk size, body)
    events: Vec<(u16, u32, usize, Vec<u8>)>,
}

/// Decode a .dms file: unpacked size and tracks.
fn parse(file: &[u8]) -> (usize, Vec<Track>) {
    let raw = envelope::decode(file).unwrap();
    let mut tracks = Vec::new();
    for (id, body) in chunk::chunks(&raw) {
        if id != 1003 {
            continue;
        }
        let f = Fields::new(body);
        let mut t = Track {
            name: f.get(1002).unwrap().to_vec(),
            ch: f.uint(1001).unwrap() as u8,
            conductor: f.uint(1003) == Some(1),
            end: 0,
            events: Vec::new(),
        };
        for (cid, cb) in chunk::chunks(body) {
            if cid == 2009 {
                t.end = Fields::new(cb).uint(1001).unwrap();
            } else if cid >= 2001 {
                t.events.push((cid, Fields::new(cb).uint(1001).unwrap(), cb.len() + 6, cb.to_vec()));
            }
        }
        tracks.push(t);
    }
    (raw.len(), tracks)
}

type Note = (Vec<u8>, u8, u64, u8, u8, u32);

/// Every note of a file as (track name, channel, tick + shift, key, velocity, gate).
fn notes(tracks: &[Track], shift: u64) -> Vec<Note> {
    let mut out = Vec::new();
    for t in tracks {
        for (id, pos, _, body) in &t.events {
            if *id == 2001 {
                let f = Fields::new(body);
                out.push((
                    t.name.clone(),
                    t.ch,
                    *pos as u64 + shift,
                    f.uint(2001).unwrap() as u8,
                    f.uint(2002).unwrap() as u8,
                    f.uint(2003).unwrap(),
                ));
            }
        }
    }
    out
}

fn sorted(mut v: Vec<Note>) -> Vec<Note> {
    v.sort();
    v
}

fn split(limit: u64, answers: Answers) -> anyhow::Result<Vec<Vec<u8>>> {
    Ok(split_with(limit, None, answers)?.into_iter().map(|f| f.1).collect())
}

/// Convert the test song; returns each file with its plan (for naming).
fn split_with(limit: u64, track_split: Option<usize>, answers: Answers) -> anyhow::Result<Vec<(FilePlan, Vec<u8>)>> {
    let opts = Options { split_size: limit, track_split, ..Options::default() };
    let mut a = answers;
    let mut plans = Vec::new();
    let (files, _) = dmsmid::mid_to_dms(
        &song(),
        &Module::builtin(),
        &opts,
        &mut a,
        |p| {
            plans.push(*p);
            Ok(Vec::new())
        },
        &Progress::default(),
    )?;
    Ok(plans.into_iter().zip(files).collect())
}

fn whole() -> (usize, Vec<Track>) {
    let files = split(MAX_UNPACKED, Answers::default()).unwrap();
    assert_eq!(files.len(), 1);
    parse(&files[0])
}

fn answers(split: SplitMethod, keep: bool, cut: TimeCut) -> Answers {
    Answers { split: Some(split), keep_offset: keep, partial_cut: cut, ..Answers::default() }
}

/// Parts: (size, tracks, where the part starts in song time).
fn parts(files: &[Vec<u8>], keep: bool) -> Vec<(usize, Vec<Track>, u64)> {
    let mut out = Vec::new();
    let mut from = 0u64;
    for f in files {
        let (size, tracks) = parse(f);
        let cond_end = tracks.iter().find(|t| t.conductor).unwrap().end as u64;
        out.push((size, tracks, from));
        // A part's conductor ends where the next part starts (relative if moved to 0).
        from = if keep { cond_end } else { from + cond_end };
    }
    out
}

fn measure_starts() -> Vec<u64> {
    let mut v: Vec<u64> = (0..=4).map(|b| b * BAR44).collect();
    v.extend((1..=8).map(|b| 4 * BAR44 + b * BAR34));
    v
}

#[test]
fn time_splits_keep_every_note_and_respect_the_limit() {
    let (total, reference) = whole();
    let limit = total as u64 / 3;
    for method in [SplitMethod::Measure, SplitMethod::Tick] {
        for keep in [true, false] {
            let files = split(limit, answers(method, keep, TimeCut::Measure)).unwrap();
            assert!(files.len() >= 3, "{method:?}: {} parts", files.len());
            let ps = parts(&files, keep);
            let mut all = Vec::new();
            for (k, (size, tracks, from)) in ps.iter().enumerate() {
                assert!(*size as u64 <= limit, "{method:?} part {k}: {size} > {limit}");
                assert_eq!(tracks.len(), reference.len(), "every part holds every track");
                all.extend(notes(tracks, if keep { 0 } else { *from }));
                if k > 0 {
                    // The tempo, time and key signature in effect are carried to the start.
                    let at = if keep { *from as u32 } else { 0 };
                    let cond = tracks.iter().find(|t| t.conductor).unwrap();
                    for id in [2008u16, 2015, 2016] {
                        assert!(
                            cond.events.iter().any(|e| e.0 == id && e.1 <= at),
                            "{method:?} keep={keep} part {k}: no event {id} at its start"
                        );
                    }
                }
            }
            assert_eq!(sorted(all), sorted(notes(&reference, 0)), "{method:?} keep={keep}: notes differ");

            let cuts: Vec<u64> = ps.iter().skip(1).map(|p| p.2).collect();
            if method == SplitMethod::Measure {
                for c in &cuts {
                    assert!(measure_starts().contains(c), "measure split cut at {c}, not a measure start");
                }
            } else {
                // Each cut is where adding the next tick's events would overflow.
                for (k, c) in cuts.iter().enumerate() {
                    let at: usize = reference
                        .iter()
                        .flat_map(|t| &t.events)
                        .filter(|e| e.1 as u64 == *c)
                        .map(|e| e.2)
                        .sum();
                    assert!(ps[k].0 + at > limit as usize, "tick cut {c} is not as late as possible");
                }
            }
        }
    }
}

#[test]
fn measure_split_falls_back_to_ticks_inside_a_measure_too_big_for_a_file() {
    let (total, reference) = whole();
    // A limit below one measure of "Busy" alone forces cuts inside measures.
    let limit = total as u64 / 12;
    let files = split(limit, answers(SplitMethod::Measure, true, TimeCut::Measure)).unwrap();
    let ps = parts(&files, true);
    assert!(ps.iter().all(|p| p.0 as u64 <= limit));
    assert!(ps.iter().skip(1).any(|p| !measure_starts().contains(&p.2)), "expected a cut inside a measure");
    let all: Vec<Note> = ps.iter().flat_map(|p| notes(&p.1, 0)).collect();
    assert_eq!(sorted(all), sorted(notes(&reference, 0)));
}

#[test]
fn track_split_keeps_tracks_whole_and_cuts_only_oversized_ones() {
    let (total, reference) = whole();
    let limit = total as u64 / 4;
    let files = split(limit, answers(SplitMethod::Track, true, TimeCut::Tick)).unwrap();
    let ps = parts(&files, true);
    let mut all = Vec::new();
    let mut seen: BTreeMap<(Vec<u8>, u8), usize> = BTreeMap::new();
    for (size, tracks, _) in &ps {
        assert!(*size as u64 <= limit);
        for t in tracks.iter().filter(|t| !t.conductor) {
            *seen.entry((t.name.clone(), t.ch)).or_default() += 1;
        }
        all.extend(notes(tracks, 0));
    }
    let split_tracks: Vec<_> = seen.iter().filter(|(_, n)| **n > 1).map(|(k, _)| k.clone()).collect();
    assert!(!split_tracks.is_empty() && split_tracks.len() < seen.len(), "{seen:?}");
    for (_, tracks, _) in &ps {
        let regular: Vec<_> = tracks.iter().filter(|t| !t.conductor).collect();
        if regular.iter().any(|t| split_tracks.contains(&(t.name.clone(), t.ch))) {
            assert_eq!(regular.len(), 1, "a split track gets parts of its own");
        }
    }
    // Whole tracks are complete wherever they appear.
    for t in ps.iter().flat_map(|p| &p.1).filter(|t| !t.conductor && !split_tracks.contains(&(t.name.clone(), t.ch))) {
        let r = reference.iter().find(|r| r.name == t.name && r.ch == t.ch).unwrap();
        assert_eq!(t.events.len(), r.events.len());
    }
    assert_eq!(sorted(all), sorted(notes(&reference, 0)));
}

#[test]
fn packed_track_split_fills_files_and_keeps_every_note() {
    let (total, reference) = whole();
    for cut in [TimeCut::Measure, TimeCut::Tick] {
        let limit = total as u64 / 3;
        let files = split(limit, answers(SplitMethod::TrackPartial, true, cut)).unwrap();
        let ps = parts(&files, true);
        assert!(ps.iter().all(|p| p.0 as u64 <= limit));
        let all: Vec<Note> = ps.iter().flat_map(|p| notes(&p.1, 0)).collect();
        assert_eq!(sorted(all), sorted(notes(&reference, 0)), "{cut:?}");
        // Packing never needs more files than the total size requires, plus one.
        assert!(files.len() as u64 <= total as u64 / limit + 2, "{cut:?}: {} files", files.len());
    }
}

#[test]
fn songs_that_fit_are_never_split() {
    let files = split(MAX_UNPACKED, answers(SplitMethod::Tick, true, TimeCut::Tick)).unwrap();
    assert_eq!(files.len(), 1);
}

#[test]
fn refuses_without_a_split_method_or_with_impossible_sizes() {
    let (total, _) = whole();
    let e = split(total as u64 / 2, Answers::default()).unwrap_err().to_string();
    assert!(e.contains("--split"), "{e}");
    let e = split(MAX_UNPACKED + 1, Answers::default()).unwrap_err().to_string();
    assert!(e.contains("at most"), "{e}");
    assert!(split(100, answers(SplitMethod::Tick, true, TimeCut::Tick)).is_err());
}

/// Control events (CCM id, value) of a track at position `pos`, in order.
fn controls_at(t: &Track, pos: u32) -> Vec<(u32, i64)> {
    t.events
        .iter()
        .filter(|e| e.1 == pos && (e.0 == 2003 || e.0 == 2002))
        .map(|e| {
            let f = Fields::new(&e.3);
            if e.0 == 2002 {
                (1000, f.uint(2003).unwrap() as i64) // program, 1-based
            } else {
                (f.uint(2001).unwrap(), f.float(2003).unwrap() as i64)
            }
        })
        .collect()
}

#[test]
fn channel_state_is_carried_into_parts_that_start_mid_song() {
    let (total, _) = whole();
    for keep in [true, false] {
        let files = split(total as u64 / 3, answers(SplitMethod::Measure, keep, TimeCut::Measure)).unwrap();
        let ps = parts(&files, keep);
        for (k, (_, tracks, from)) in ps.iter().enumerate().skip(1) {
            let bass = tracks.iter().find(|t| t.name == b"Bass").unwrap();
            let at = if keep { *from as u32 } else { 0 };
            let state = controls_at(bass, at);
            // Program 33 (stored 1-based), the RPN in its original order, pitch bend
            // 0x2800 - 8192, and CC7 as last set before the part (value = index of the
            // 24-tick step before `from`); then the events of the part's first tick.
            let cc7 = ((*from - 1) / 24 % 128) as i64;
            let expected = vec![(1000, 34), (101, 0), (100, 0), (6, 12), (130, 0x2800 - 8192), (7, cc7)];
            assert_eq!(&state[..6], &expected[..], "keep={keep} part {k}: {state:?}");
            // Momentary commands are not replayed.
            assert!(!state.iter().any(|c| c.0 == 123 || c.0 == 96), "keep={keep} part {k}");
        }
    }
}

#[test]
fn track_split_writes_n_tracks_per_file_with_all_global_events() {
    let (_, reference) = whole();
    let regular: Vec<&Track> = reference.iter().filter(|t| !t.conductor).collect();
    assert_eq!(regular.len(), 5); // Busy, Bass, Duo (2 channels), Sparse
    let files = split_with(MAX_UNPACKED, Some(2), Answers::default()).unwrap();
    let plans: Vec<FilePlan> = files.iter().map(|f| f.0).collect();
    assert_eq!(
        plans,
        [(1, 2), (3, 4), (5, 5)].map(|t| FilePlan { tracks: Some(t), part: None }),
        "file plans"
    );
    assert_eq!(plans[0].suffix(), "Track 1-2");
    assert_eq!(plans[2].suffix(), "Track 5", "a single-track group is named by its track");
    let cond = reference.iter().find(|t| t.conductor).unwrap();
    for (n, (plan, file)) in files.iter().enumerate() {
        let (_, tracks) = parse(file);
        let (a, b) = plan.tracks.unwrap();
        let mine: Vec<&Track> = tracks.iter().filter(|t| !t.conductor).collect();
        assert_eq!(mine.len(), b - a + 1, "file {n}");
        for (t, r) in mine.iter().zip(&regular[a - 1..b]) {
            assert_eq!((t.name.as_slice(), t.ch, t.events.len()), (r.name.as_slice(), r.ch, r.events.len()));
        }
        // Every file has the whole conductor.
        let c = tracks.iter().find(|t| t.conductor).unwrap();
        assert_eq!(c.events.iter().map(|e| (e.0, e.1)).collect::<Vec<_>>(), cond.events.iter().map(|e| (e.0, e.1)).collect::<Vec<_>>());
    }
}

#[test]
fn track_split_groups_too_large_are_split_further() {
    let (_, reference) = whole();
    // Group 1 (Busy + Bass) is too large for this limit; the others fit.
    let busy_bass: usize = reference.iter().filter(|t| t.name == b"Busy" || t.name == b"Bass").flat_map(|t| &t.events).map(|e| e.2).sum();
    let limit = busy_bass as u64 * 2 / 3;
    let files = split_with(limit, Some(2), answers(SplitMethod::Tick, true, TimeCut::Tick)).unwrap();
    let names: Vec<String> = files.iter().map(|f| f.0.suffix()).collect();
    let d = files.iter().filter(|f| f.0.tracks == Some((1, 2))).count();
    assert!(d >= 2, "{names:?}");
    for (c, f) in files.iter().filter(|f| f.0.tracks == Some((1, 2))).enumerate() {
        assert_eq!(f.0.suffix(), format!("Track 1-2, part {} of {d}", c + 1));
    }
    assert!(names.contains(&"Track 3-4".to_string()) && names.contains(&"Track 5".to_string()), "{names:?}");
    let mut all = Vec::new();
    for (_, file) in &files {
        let (size, tracks) = parse(file);
        assert!(size as u64 <= limit);
        all.extend(notes(&tracks, 0));
    }
    assert_eq!(sorted(all), sorted(notes(&reference, 0)));
}
