//! Domino chunk IDs, templates and event builders, as observed in files saved by
//! Domino (see DMS_SPEC.md). Builders append to a caller-provided buffer.

use crate::chunk::{float_list, hex, put};

// Root
pub const ROOT_NAME: u16 = 1000;
pub const ROOT_COPYRIGHT: u16 = 1001;
pub const ROOT_PPQ: u16 = 1002;
pub const ROOT_TRACK: u16 = 1003;

// Track settings
pub const TR_PORT: u16 = 1000;
pub const TR_CHANNEL: u16 = 1001;
pub const TR_NAME: u16 = 1002;
pub const TR_CONDUCTOR: u16 = 1003;
pub const TR_END: u16 = 2009;

// Events (inside a track); every event starts with 1001 = u32 position
pub const POS: u16 = 1001;
pub const EV_NOTE: u16 = 2001;
pub const EV_PROGRAM: u16 = 2002;
pub const EV_CONTROL: u16 = 2003;
pub const EV_SYSEX: u16 = 2004;
pub const EV_TEXT: u16 = 2005;
pub const EV_VARIABLES: u16 = 2007;
pub const EV_TEMPO: u16 = 2008;
pub const EV_LYRIC: u16 = 2011;
pub const EV_CUE: u16 = 2012;
pub const EV_MEASURE_LINK: u16 = 2014;
pub const EV_TIME_SIG: u16 = 2015;
pub const EV_KEY_SIG: u16 = 2016;
pub const EV_MARKER: u16 = 2017;
pub const EV_SCALE: u16 = 2018;
pub const EV_CHORD: u16 = 2019;

/// Text-like events and the SMF meta type each corresponds to.
pub const TEXT_EVENTS: [(u16, u8); 4] = [(EV_TEXT, 0x01), (EV_LYRIC, 0x05), (EV_MARKER, 0x06), (EV_CUE, 0x07)];

/// Root chunks between PPQ and the first track (1013 = 0; Domino itself writes varying values).
const SONG_REST: &str = "ef030400000030000000f1030400000000000000f40308000000000000000000\
    0000f5030400000000000000f6030400000000000000fb0300000000fc030400\
    000010000000fd030100000001fe030100000001ff0304000000110000000004\
    0100000001";

/// Root chunks after the last track.
const SONG_TAIL: &str = "ee0300000000f0031a000000e80300000000e9030400000001000000ea030400\
    000001000000f903420000006400010000000065000100000000660001000000\
    006700040000006400000068000100000000690004000000640000006a000c00\
    0000050505050505050505050505";

pub fn song_start(out: &mut Vec<u8>, name: &[u8], copyright: &[u8], ppq: u16) {
    put(out, ROOT_NAME, name);
    put(out, ROOT_COPYRIGHT, copyright);
    put(out, ROOT_PPQ, &ppq.to_le_bytes());
    out.extend(hex(SONG_REST));
}

pub fn song_tail(out: &mut Vec<u8>) {
    out.extend(hex(SONG_TAIL));
}

/// Header of a track chunk whose body (head + events + tail) is `body_len` bytes.
pub fn track_header(out: &mut Vec<u8>, body_len: u32) {
    out.extend_from_slice(&ROOT_TRACK.to_le_bytes());
    out.extend_from_slice(&body_len.to_le_bytes());
}

/// Track settings that precede the events.
pub fn track_head(out: &mut Vec<u8>, name: &[u8], channel: u8, port: u16, conductor: bool) {
    put(out, TR_PORT, &port.to_le_bytes());
    put(out, TR_CHANNEL, &[channel]);
    put(out, TR_NAME, name);
    put(out, TR_CONDUCTOR, &[conductor as u8]);
    put(out, 1004, &[(channel == 9 && !conductor) as u8]); // rhythm track
    put(out, 1008, &[60]);
    put(out, 1009, b"General MIDI Drum");
    put(out, 1011, &0u16.to_le_bytes());
    put(out, 1012, &0u32.to_le_bytes());
    put(out, 1016, &100u32.to_le_bytes());
    put(out, 1017, &0u32.to_le_bytes());
    put(out, 1013, &[1]);
    put(out, 1014, &1u16.to_le_bytes());
    put(out, 1015, &[1]);
    put(out, 1018, &[255]);
    put(out, 1019, &1u32.to_le_bytes());
    put(out, 1020, &[0; 16]);
    put(out, 1021, &[0]);
    put(out, 1022, &[127]);
}

/// End-of-track chunk and the settings that follow the events.
pub fn track_tail(out: &mut Vec<u8>, end: u32, ppq: u16) {
    let mut eot = Vec::with_capacity(10);
    put(&mut eot, POS, &end.to_le_bytes());
    put(out, TR_END, &eot);
    put(out, 1005, &50u32.to_le_bytes());
    put(out, 1006, &[100]);
    put(out, 1007, &(ppq as u32).to_le_bytes()); // default gate; equals PPQ in every file seen
    put(out, 1010, &[0xE8, 0x03, 1, 0, 0, 0, 0, 0xE9, 0x03, 1, 0, 0, 0, 0]);
}

/// Start an event container with `body_len` bytes of fields after the position.
#[inline]
fn event_header(out: &mut Vec<u8>, id: u16, pos: u32, fields_len: usize) {
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&((10 + fields_len) as u32).to_le_bytes());
    out.extend_from_slice(&[0xE9, 0x03, 4, 0, 0, 0]);
    out.extend_from_slice(&pos.to_le_bytes());
}

pub const NOTE_LEN: usize = 40;
pub const PROGRAM_LEN: usize = 58;
pub const CONTROL_LEN: usize = 56;

#[inline]
pub fn note(out: &mut Vec<u8>, pos: u32, key: u8, vel: u8, gate: u32) {
    let mut b = [
        0xD1, 0x07, 34, 0, 0, 0, //
        0xE9, 0x03, 4, 0, 0, 0, 0, 0, 0, 0, //
        0xD1, 0x07, 1, 0, 0, 0, 0, //
        0xD2, 0x07, 1, 0, 0, 0, 0, //
        0xD3, 0x07, 4, 0, 0, 0, 0, 0, 0, 0,
    ];
    b[12..16].copy_from_slice(&pos.to_le_bytes());
    b[22] = key;
    b[29] = vel;
    b[36..40].copy_from_slice(&gate.to_le_bytes());
    out.extend_from_slice(&b);
}

pub fn program(out: &mut Vec<u8>, pos: u32, program: u8) {
    event_header(out, EV_PROGRAM, pos, 7 + 6 + 7 + 7 + 7 + 8);
    put(out, 2003, &[program + 1]);
    put(out, 2004, b"");
    put(out, 2005, &[0]);
    put(out, 2001, &[0xFF]);
    put(out, 2002, &[0xFF]);
    put(out, 2006, &(-1i16).to_le_bytes());
}

#[inline]
pub fn control(out: &mut Vec<u8>, pos: u32, id: u16, gate: f32, value: f32) {
    event_header(out, EV_CONTROL, pos, 8 + 16 + 16);
    put(out, 2001, &id.to_le_bytes());
    put(out, 2002, &float_list(gate));
    put(out, 2003, &float_list(value));
}

pub fn tempo(out: &mut Vec<u8>, pos: u32, bpm: f32) {
    event_header(out, EV_TEMPO, pos, 16);
    put(out, 2001, &float_list(bpm));
}

pub fn time_sig(out: &mut Vec<u8>, pos: u32, num: u8, denom: u8) {
    event_header(out, EV_TIME_SIG, pos, 14);
    put(out, 2001, &[num]);
    put(out, 2002, &[denom]);
}

pub fn key_sig(out: &mut Vec<u8>, pos: u32, code: u8) {
    event_header(out, EV_KEY_SIG, pos, 7);
    put(out, 2001, &[code]);
}

pub fn text(out: &mut Vec<u8>, id: u16, pos: u32, bytes: &[u8]) {
    event_header(out, id, pos, 6 + bytes.len());
    put(out, 2001, bytes);
}

/// SMF key signature (sharps/flats, minor) → Domino code:
/// sharps n → n, flats n → 8 + n, minor adds 16.
pub fn key_code(sf: i8, minor: bool) -> u8 {
    let base = if sf >= 0 { sf as u8 } else { 8 + sf.unsigned_abs() };
    base + if minor { 16 } else { 0 }
}

pub fn key_from_code(code: u8) -> Option<(i8, bool)> {
    let minor = code & 0x10 != 0;
    let n = code & 0x0F;
    let sf = match n {
        0..=7 => n as i8,
        9..=15 => -((n - 8) as i8),
        _ => return None,
    };
    (code < 32).then_some((sf, minor))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunk::Fields;

    #[test]
    fn key_codes_match_domino() {
        for (sf, mi, code) in [(0, false, 0), (1, false, 1), (-6, true, 30), (6, false, 6), (-7, false, 15), (0, true, 16), (7, true, 23), (-1, false, 9)] {
            assert_eq!(key_code(sf, mi), code);
            assert_eq!(key_from_code(code), Some((sf, mi)));
        }
    }

    #[test]
    fn event_sizes_are_consistent() {
        let mut b = Vec::new();
        let builders: [fn(&mut Vec<u8>); 7] = [
            |b| note(b, 7, 60, 100, 480),
            |b| program(b, 7, 5),
            |b| control(b, 7, 10, 0.0, 61.0),
            |b| tempo(b, 7, 120.0),
            |b| time_sig(b, 7, 3, 4),
            |b| key_sig(b, 7, 9),
            |b| text(b, EV_TEXT, 7, b"abc"),
        ];
        for f in builders {
            b.clear();
            f(&mut b);
            let size = u32::from_le_bytes(b[2..6].try_into().unwrap()) as usize;
            match u16::from_le_bytes([b[0], b[1]]) {
                EV_NOTE => assert_eq!(b.len(), NOTE_LEN),
                EV_PROGRAM => assert_eq!(b.len(), PROGRAM_LEN),
                EV_CONTROL => assert_eq!(b.len(), CONTROL_LEN),
                _ => {}
            }
            assert_eq!(size + 6, b.len());
            let ev = Fields::new(&b[6..]);
            assert!(ev.is_valid());
            assert_eq!(ev.uint(POS), Some(7));
        }
    }
}
