//! Generate a large black-MIDI-style test file.
//! Usage: cargo run --release --example gen_big -- OUT.mid TRACKS NOTES_PER_TRACK

use std::io::Write;

fn vlq(mut n: u32, out: &mut Vec<u8>) {
    let mut tmp = [0u8; 5];
    let mut i = 4;
    tmp[i] = (n & 0x7F) as u8;
    n >>= 7;
    while n > 0 {
        i -= 1;
        tmp[i] = 0x80 | (n & 0x7F) as u8;
        n >>= 7;
    }
    out.extend_from_slice(&tmp[i..]);
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (path, tracks, notes): (&str, u32, u32) = (&a[1], a[2].parse().unwrap(), a[3].parse().unwrap());
    let mut f = std::io::BufWriter::new(std::fs::File::create(path).unwrap());
    f.write_all(b"MThd\0\0\0\x06\0\x01").unwrap();
    f.write_all(&((tracks + 1) as u16).to_be_bytes()).unwrap();
    f.write_all(&960u16.to_be_bytes()).unwrap();
    // Conductor: tempo changes.
    let mut c = Vec::new();
    for i in 0..1000u32 {
        vlq(if i == 0 { 0 } else { 960 }, &mut c);
        c.extend_from_slice(&[0xFF, 0x51, 3]);
        c.extend_from_slice(&(400_000 + i * 50).to_be_bytes()[1..]);
    }
    c.extend_from_slice(&[0, 0xFF, 0x2F, 0]);
    f.write_all(b"MTrk").unwrap();
    f.write_all(&(c.len() as u32).to_be_bytes()).unwrap();
    f.write_all(&c).unwrap();

    let mut seed = 12345u64;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for t in 0..tracks {
        let ch = (t % 16) as u8;
        let mut b = Vec::with_capacity(notes as usize * 9);
        b.extend_from_slice(&[0, 0xFF, 0x03, 5]);
        b.extend_from_slice(format!("T{:04}", t % 10000).as_bytes());
        // Chords of 4 notes, 10 ticks long, one chord every 10 ticks, plus a CC every chord.
        let mut pending = 0u32;
        let mut emitted = 0;
        while emitted < notes {
            let keys: Vec<u8> = (0..4).map(|_| (rnd() % 128) as u8).collect();
            vlq(pending, &mut b);
            b.extend_from_slice(&[0xB0 | ch, 7, (rnd() % 128) as u8]);
            for &k in &keys {
                b.extend_from_slice(&[0, 0x90 | ch, k, 1 + (rnd() % 127) as u8]);
            }
            vlq(10, &mut b);
            for (i, &k) in keys.iter().enumerate() {
                if i > 0 {
                    b.push(0);
                }
                b.extend_from_slice(&[0x80 | ch, k, 0]);
            }
            pending = 0;
            emitted += 4;
        }
        b.extend_from_slice(&[0, 0xFF, 0x2F, 0]);
        f.write_all(b"MTrk").unwrap();
        f.write_all(&(b.len() as u32).to_be_bytes()).unwrap();
        f.write_all(&b).unwrap();
    }
}
