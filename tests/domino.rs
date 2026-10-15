//! Compare against files produced by Domino itself (tests/data):
//! probe.mid --Domino import--> probe.dms --Domino export--> probe_out.mid
//! probe2.dms (Domino-only events and edited values) --Domino export--> probe2_out.mid

use dmsmid::chunk;
use dmsmid::module::Module;
use dmsmid::smf::{self, Event};
use dmsmid::{envelope, Answers, Choices, Options, Progress, Report, SplitMethod, TimeCut, Vel0, Vel0Summary};
use flate2::Compression;
use std::io::Cursor;

fn data(name: &str) -> Vec<u8> {
    std::fs::read(format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn to_dms(mid: &[u8]) -> (Vec<u8>, Report) {
    to_dms_vel0(mid, Vel0::NoteOff)
}

fn to_dms_vel0(mid: &[u8], vel0: Vel0) -> (Vec<u8>, Report) {
    let (mut files, r) = convert(mid, &Options::default(), &mut Answers { vel0, ..Answers::default() }).unwrap();
    assert_eq!(files.len(), 1);
    (files.pop().unwrap(), r)
}

/// MIDI -> one or more .dms files.
fn convert(mid: &[u8], opts: &Options, choices: &mut dyn Choices) -> anyhow::Result<(Vec<Vec<u8>>, Report)> {
    dmsmid::mid_to_dms(mid, &Module::builtin(), opts, choices, |_| Ok(Vec::new()), &Progress::default())
}

fn to_mid(dms: &[u8]) -> (Vec<u8>, Report) {
    let (out, r) = dmsmid::dms_to_mid(dms, &Module::builtin(), Cursor::new(Vec::new()), &Progress::default()).unwrap();
    (out.into_inner(), r)
}

/// Records the velocity-0 summary it is shown.
#[derive(Default)]
struct Spy(Option<Vel0Summary>);

impl Choices for Spy {
    fn vel0(&mut self, s: &Vel0Summary) -> Vel0 {
        self.0 = Some(*s);
        Vel0::NoteOff
    }
    fn split(&mut self, _: u64, _: u64) -> Option<SplitMethod> {
        None
    }
    fn keep_offset(&mut self) -> bool {
        true
    }
    fn partial_cut(&mut self) -> TimeCut {
        TimeCut::Measure
    }
}

/// Root chunks, with root 1013 (editor state; Domino writes varying values) removed.
fn root_without_1013(unpacked: &[u8]) -> Vec<(u16, Vec<u8>)> {
    let mut it = chunk::chunks(unpacked);
    let v: Vec<_> = it.by_ref().filter(|c| c.0 != 1013).map(|(id, b)| (id, b.to_vec())).collect();
    assert!(it.rest().is_empty());
    v
}

fn assert_same_dms(ours_unpacked: &[u8], domino_unpacked: &[u8]) {
    let (a, b) = (root_without_1013(ours_unpacked), root_without_1013(domino_unpacked));
    assert_eq!(a.len(), b.len(), "root chunk count");
    for (x, y) in a.iter().zip(&b) {
        assert_eq!(x.0, y.0);
        if x.1 != y.1 && x.0 == 1003 {
            let tx: Vec<_> = chunk::chunks(&x.1).collect();
            let ty: Vec<_> = chunk::chunks(&y.1).collect();
            for (i, (cx, cy)) in tx.iter().zip(&ty).enumerate() {
                assert_eq!(cx, cy, "track chunk #{i} differs");
            }
            assert_eq!(tx.len(), ty.len(), "track chunk count");
        }
        assert_eq!(x, y, "root chunk {} differs", x.0);
    }
}

#[test]
fn midi_import_matches_domino_byte_for_byte() {
    let (ours, report) = to_dms(&data("probe.mid"));
    assert_same_dms(&envelope::decode(&ours).unwrap(), &envelope::decode(&data("probe.dms")).unwrap());
    assert_eq!(report.notes, 18);
    assert_eq!(report.tracks, 9);
    assert!(report.warnings.keys().any(|w| w.contains("poly key pressure")));
}

#[test]
fn midi_import_is_identical_with_tiny_compression_blocks() {
    // Many blocks compressed in parallel, each primed with the previous 32 KB.
    let mid = data("probe.mid");
    let opts = Options { block: 997, level: Compression::default(), ..Options::default() };
    let (small, _) = convert(&mid, &opts, &mut Answers::default()).unwrap();
    let (normal, _) = to_dms(&mid);
    assert_eq!(envelope::decode(&small[0]).unwrap(), envelope::decode(&normal).unwrap());
}

/// Events per track, key signatures excluded (Domino moves them to bar starts on export).
fn events(file: &[u8]) -> Vec<Vec<(u64, Event)>> {
    smf::parse(file)
        .unwrap()
        .tracks
        .into_iter()
        .map(|t| t.into_iter().filter(|e| !matches!(e.1, Event::Meta(0x59, _))).collect())
        .collect()
}

fn assert_same_events(ours: &[u8], domino: &[u8]) {
    let (a, b) = (events(ours), events(domino));
    assert_eq!(a.len(), b.len(), "track count");
    for (i, (ta, tb)) in a.iter().zip(&b).enumerate() {
        for (j, (ea, eb)) in ta.iter().zip(tb).enumerate() {
            assert_eq!(ea, eb, "track {i} event #{j}");
        }
        assert_eq!(ta.len(), tb.len(), "track {i} event count");
    }
}

#[test]
fn dms_export_matches_domino() {
    for (dms, mid) in [("probe.dms", "probe_out.mid"), ("probe2.dms", "probe2_out.mid")] {
        assert_same_events(&to_mid(&data(dms)).0, &data(mid));
    }
}

#[test]
fn key_signatures_keep_their_real_positions() {
    let ours = to_mid(&data("probe.dms")).0;
    let keys: Vec<(u64, Vec<u8>)> = smf::parse(&ours).unwrap().tracks[0]
        .iter()
        .filter_map(|e| match &e.1 {
            Event::Meta(0x59, d) => Some((e.0, d.clone())),
            _ => None,
        })
        .collect();
    let expected: Vec<(u64, Vec<u8>)> = [
        (0, [0x00, 0]),
        (1920, [0x01, 0]),
        (3840, [0xFA, 1]),
        (5760, [0x06, 0]),
        (7680, [0xF9, 0]),
        (9600, [0x00, 1]),
        (10080, [0x07, 1]),
        (11040, [0xFF, 0]),
    ]
    .into_iter()
    .map(|(t, d)| (t, d.to_vec()))
    .collect();
    assert_eq!(keys, expected);
}

#[test]
fn midi_round_trip_equals_domino_export() {
    let dms = to_dms(&data("probe.mid")).0;
    assert_same_events(&to_mid(&dms).0, &data("probe_out.mid"));
}

#[test]
fn clipboard_data_converts() {
    let (mid, report) = to_mid(&data("clip1.bin"));
    assert_eq!(report.tracks, 6);
    assert_eq!(smf::parse(&mid).unwrap().ppq, 480);
}

#[test]
fn trailing_padding_after_zlib_is_ignored() {
    let mut dms = data("probe.dms");
    dms.extend_from_slice(&[0; 300]);
    assert_same_events(&to_mid(&dms).0, &data("probe_out.mid"));
}

#[test]
fn damaged_files_are_rejected() {
    let dms = data("probe.dms");
    let m = Module::builtin();
    let truncated = &dms[..dms.len() / 2];
    assert!(dmsmid::dms_to_mid(truncated, &m, Cursor::new(Vec::new()), &Progress::default()).is_err());
    let mut wrong_size = dms.clone();
    wrong_size[18] ^= 1;
    assert!(dmsmid::dms_to_mid(&wrong_size, &m, Cursor::new(Vec::new()), &Progress::default()).is_err());
}

#[test]
fn format0_is_split_per_channel() {
    // One track: tempo, note on ch1, CC on ch2.
    let mut t = Vec::new();
    t.extend([0x00, 0xFF, 0x51, 0x03, 0x07, 0xA1, 0x20]);
    t.extend([0x00, 0x90, 60, 100, 0x00, 0xB1, 7, 99]);
    t.extend([0x83, 0x60, 0x80, 60, 0, 0x00, 0xFF, 0x2F, 0x00]);
    let mut f = b"MThd\0\0\0\x06\0\0\0\x01\x01\xE0MTrk".to_vec();
    f.extend((t.len() as u32).to_be_bytes());
    f.extend(t);
    let (dms, report) = to_dms(&f);
    assert_eq!(report.tracks, 3); // conductor + ch1 + ch2
    let tracks = events(&to_mid(&dms).0);
    assert!(tracks[0].iter().any(|(_, e)| matches!(e, Event::Meta(0x51, _))));
    assert!(tracks[1].contains(&(0, Event::Midi(vec![0x90, 60, 100]))));
    assert!(tracks[1].contains(&(480, Event::Midi(vec![0x80, 60, 0]))));
    assert!(tracks[2].contains(&(0, Event::Midi(vec![0xB1, 7, 99]))));
}

#[test]
fn overlapping_and_unclosed_notes() {
    // Same key twice (FIFO pairing), plus a note that never ends.
    let mut t = Vec::new();
    t.extend([0x00, 0x90, 60, 100]); // A on @0
    t.extend([0x0A, 0x90, 60, 90]); // B on @10
    t.extend([0x0A, 0x80, 60, 0]); // off @20 -> A (gate 20)
    t.extend([0x0A, 0x90, 64, 80]); // C on @30, never off
    t.extend([0x0A, 0x80, 60, 0]); // off @40 -> B (gate 30)
    t.extend([0x81, 0x00, 0xFF, 0x2F, 0x00]); // end @168
    let mut f = b"MThd\0\0\0\x06\0\0\0\x01\x01\xE0MTrk".to_vec();
    f.extend((t.len() as u32).to_be_bytes());
    f.extend(t);
    let (dms, report) = to_dms(&f);
    assert_eq!(report.notes, 3);
    assert_eq!(report.warnings.get("note without a note-off (ended at the end of its track)"), Some(&1));
    let tracks = events(&to_mid(&dms).0);
    let offs: Vec<_> = tracks[1].iter().filter(|(_, e)| matches!(e, Event::Midi(m) if m[0] == 0x80)).collect();
    assert_eq!(offs, [&(20, Event::Midi(vec![0x80, 60, 0])), &(40, Event::Midi(vec![0x80, 60, 0])), &(168, Event::Midi(vec![0x80, 64, 0]))]);
}

/// Black MIDI convention (e.g. Hypernova.mid): every note-on, even with velocity 0,
/// has its own explicit `8n` note-off. Here one key gets 4 audible and 7 silent
/// notes at tick 0, all ended by 11 note-offs at tick 1.
fn silent_note_file() -> Vec<u8> {
    let mut t = vec![0x00, 0x9F, 64, 4];
    for v in [2, 1, 1, 0, 0, 0, 0, 0, 0, 0] {
        t.extend([0x00, 64, v]); // running status
    }
    t.extend([0x01, 0x8F, 64, 64]);
    for _ in 0..10 {
        t.extend([0x00, 64, 64]);
    }
    t.extend([0x00, 0xFF, 0x2F, 0x00]);
    let mut f = b"MThd\0\0\0\x06\0\0\0\x01\x01\xE0MTrk".to_vec();
    f.extend((t.len() as u32).to_be_bytes());
    f.extend(t);
    f
}

fn notes_of(dms: &[u8]) -> (Vec<(u64, Event)>, Vec<(u64, Event)>) {
    let tracks = events(&to_mid(dms).0);
    let pick = |st: u8| tracks[1].iter().filter(|(_, e)| matches!(e, Event::Midi(m) if m[0] == st)).cloned().collect();
    (pick(0x9F), pick(0x8F))
}

#[test]
fn velocity0_choice_is_offered_with_both_readings() {
    let mut spy = Spy::default();
    convert(&silent_note_file(), &Options::default(), &mut spy).unwrap();
    let s = spy.0.expect("asked");
    assert_eq!(s.vel0_note_ons, 7);
    assert_eq!(s.unpaired_as_note_off, 14); // 3 velocity-0 + 11 note-offs match nothing
    assert_eq!(s.unpaired_as_silent_note, 0);
    assert_eq!(s.tracks_better_as_silent, 1);
    assert!(s.ambiguous());

    // An ordinary file using velocity-0 note-ons as note-offs: nothing to decide.
    let mut probe = Spy::default();
    convert(&data("probe.mid"), &Options::default(), &mut probe).unwrap();
    assert!(!probe.0.expect("probe has one velocity-0 note-off").ambiguous());
}

#[test]
fn velocity0_as_note_off_is_the_default_reading() {
    let (dms, report) = to_dms(&silent_note_file());
    assert_eq!(report.notes, 4);
    assert_eq!(report.warnings.get("note-off without a matching note-on (ignored)"), Some(&14));
    let (ons, offs) = notes_of(&dms);
    assert_eq!(ons.len(), 4);
    assert!(offs.iter().all(|(t, _)| *t == 0)); // closed by the velocity-0 note-ons
}

#[test]
fn velocity0_as_silent_notes() {
    for mode in [Vel0::SilentNote, Vel0::Auto] {
        let (dms, report) = to_dms_vel0(&silent_note_file(), mode);
        assert_eq!(report.notes, 11);
        assert!(!report.warnings.keys().any(|w| w.contains("without a matching")), "{:?}", report.warnings);
        assert_eq!(report.warnings.get("velocity-0 note-on read as a silent note"), Some(&7));
        // Every note lasts 1 tick and the original structure survives the round trip.
        let (ons, offs) = notes_of(&dms);
        assert_eq!(ons.len(), 11);
        assert!(ons.iter().all(|(t, _)| *t == 0));
        assert_eq!(ons.iter().filter(|(_, e)| matches!(e, Event::Midi(m) if m[2] == 0)).count(), 7);
        assert_eq!(offs.len(), 11);
        assert!(offs.iter().all(|(t, _)| *t == 1));
    }
}

#[test]
fn auto_reads_each_track_on_its_own() {
    // probe.mid uses a velocity-0 note-on as a note-off: auto must keep that reading
    // there, so the output still equals Domino's import.
    let (auto, _) = to_dms_vel0(&data("probe.mid"), Vel0::Auto);
    assert_same_dms(&envelope::decode(&auto).unwrap(), &envelope::decode(&data("probe.dms")).unwrap());
}
