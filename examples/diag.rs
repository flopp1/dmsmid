//! Note pairing diagnostics for a MIDI file.
//! Usage: cargo run --release --example diag -- FILE.mid

use dmsmid::smf::{self, Ev};
use std::collections::HashMap;

fn main() {
    let path = std::env::args().nth(1).expect("file");
    let f = std::fs::File::open(&path).unwrap();
    let data = unsafe { memmap2::Mmap::map(&f).unwrap() };
    let h = smf::index(&data).unwrap();
    println!("format {} ppq {} tracks {}", h.format, h.ppq, h.tracks.len());
    let mut shown = 0;
    for (ti, t) in h.tracks.iter().enumerate() {
        let (mut on, mut off80, mut off90, mut unmatched, mut other, mut events) = (0u64, 0u64, 0u64, 0u64, 0u64, 0u64);
        let mut open = vec![0u32; 16 * 128];
        // Last few events per (ch, key) to show context of an unmatched off.
        let mut hist: HashMap<usize, Vec<(u64, u8, u8)>> = HashMap::new();
        let mut chans = [0u64; 16];
        let mut err = None;
        for e in smf::events(t) {
            let (tick, ev) = match e {
                Ok(v) => v,
                Err(e) => {
                    err = Some(e.to_string());
                    break;
                }
            };
            events += 1;
            if let Ev::Midi(st, k, v) = ev {
                let ch = (st & 0x0F) as usize;
                chans[ch] += 1;
                let slot = ch * 128 + (k as usize & 0x7F);
                let kind = st & 0xF0;
                if matches!(kind, 0x80 | 0x90) {
                    let h = hist.entry(slot).or_default();
                    if shown < 6 && kind == 0x80 || (kind == 0x90 && v == 0) {
                        // fallthrough to bookkeeping below
                    }
                    h.push((tick, st, v));
                    if h.len() > 6 {
                        h.remove(0);
                    }
                }
                match kind {
                    0x90 if v > 0 => {
                        on += 1;
                        open[slot] += 1;
                    }
                    0x80 | 0x90 => {
                        if kind == 0x80 { off80 += 1 } else { off90 += 1 }
                        if open[slot] > 0 {
                            open[slot] -= 1;
                        } else {
                            unmatched += 1;
                            if shown < 6 {
                                shown += 1;
                                println!("  track {ti} unmatched off @{tick} ch{} key {k}; recent same-key events: {:?}", ch + 1, hist[&slot]);
                            }
                        }
                    }
                    _ => other += 1,
                }
            }
        }
        let still_open: u64 = open.iter().map(|&n| n as u64).sum();
        let used: Vec<usize> = (0..16).filter(|&c| chans[c] > 0).map(|c| c + 1).collect();
        println!(
            "track {ti:2}: {} bytes, {events} events, on {on}, off(8n) {off80}, off(9n v0) {off90}, unmatched {unmatched}, still open {still_open}, other {other}, channels {used:?}{}",
            t.len(),
            err.map(|e| format!(", ERROR: {e}")).unwrap_or_default()
        );
    }
}
