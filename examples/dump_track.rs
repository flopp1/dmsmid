//! Usage: dump_track FILE TRACK [SKIP] [COUNT] — print raw bytes and decoded events.
use dmsmid::smf::{self, Ev};
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let f = std::fs::File::open(&a[1]).unwrap();
    let data = unsafe { memmap2::Mmap::map(&f).unwrap() };
    let h = smf::index(&data).unwrap();
    let t = h.tracks[a[2].parse::<usize>().unwrap()];
    let skip: usize = a.get(3).map_or(0, |s| s.parse().unwrap());
    let n: usize = a.get(4).map_or(40, |s| s.parse().unwrap());
    println!("first bytes: {:02x?}", &t[..64.min(t.len())]);
    let mut stat = std::collections::BTreeMap::new();
    for (i, e) in smf::events(t).enumerate() {
        let (tick, ev) = e.unwrap();
        if let Ev::Midi(s, _, v) = ev { *stat.entry((s, v == 0)).or_insert(0u64) += 1; }
        if i >= skip && i < skip + n { println!("{i:8} @{tick:9} {ev:?}"); }
    }
    for ((s, z), c) in stat { println!("status {s:02X}{} : {c}", if z { " vel0" } else { "" }); }
}
