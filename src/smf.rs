//! Standard MIDI File reading (zero-copy, over a memory-mapped file) and writing helpers.

use anyhow::{bail, Result};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Ev<'a> {
    /// Channel message: status, data 1, data 2 (0 for one-data-byte messages).
    Midi(u8, u8, u8),
    Meta(u8, &'a [u8]),
    Sysex(&'a [u8]),
}

pub struct Header<'a> {
    pub format: u16,
    pub ppq: u16,
    /// Body of every `MTrk` chunk, in file order.
    pub tracks: Vec<&'a [u8]>,
}

/// Locate the header and every track without parsing events.
pub fn index(data: &[u8]) -> Result<Header<'_>> {
    if !data.starts_with(b"MThd") || data.len() < 14 {
        bail!("not a Standard MIDI File");
    }
    let hlen = u32::from_be_bytes(data[4..8].try_into().unwrap()) as usize;
    let format = u16::from_be_bytes([data[8], data[9]]);
    let division = u16::from_be_bytes([data[12], data[13]]);
    if division & 0x8000 != 0 {
        bail!("SMPTE time division is not supported (Domino needs ticks per quarter note)");
    }
    let mut tracks = Vec::new();
    let mut i = 8usize.saturating_add(hlen);
    while i.saturating_add(8) <= data.len() {
        let len = u32::from_be_bytes(data[i + 4..i + 8].try_into().unwrap()) as usize;
        let end = (i + 8).saturating_add(len).min(data.len());
        if &data[i..i + 4] == b"MTrk" {
            tracks.push(&data[i + 8..end]);
        }
        i = end;
    }
    Ok(Header { format, ppq: division, tracks })
}

/// Iterator over the events of one track body. Yields `(absolute tick, event)`;
/// ends after `FF 2F` or at the end of the data.
#[derive(Clone)]
pub struct Events<'a> {
    b: &'a [u8],
    p: usize,
    tick: u64,
    running: u8,
    done: bool,
}

pub fn events(track: &[u8]) -> Events<'_> {
    Events { b: track, p: 0, tick: 0, running: 0, done: false }
}

impl<'a> Events<'a> {
    #[inline]
    fn byte(&mut self) -> Result<u8> {
        match self.b.get(self.p) {
            Some(&v) => {
                self.p += 1;
                Ok(v)
            }
            None => bail!("track data ends unexpectedly"),
        }
    }

    #[inline]
    fn vlq(&mut self) -> Result<u32> {
        let mut v = 0u32;
        for _ in 0..4 {
            let c = self.byte()?;
            v = (v << 7) | (c & 0x7F) as u32;
            if c < 0x80 {
                return Ok(v);
            }
        }
        bail!("variable-length number is too long")
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.b.len() - self.p < n {
            bail!("track data ends unexpectedly");
        }
        self.p += n;
        Ok(&self.b[self.p - n..self.p])
    }

    fn read(&mut self) -> Result<(u64, Ev<'a>)> {
        self.tick += self.vlq()? as u64;
        let first = self.byte()?;
        let ev = match first {
            0xFF => {
                let ty = self.byte()?;
                let len = self.vlq()? as usize;
                let d = self.take(len)?;
                if ty == 0x2F {
                    self.done = true;
                }
                Ev::Meta(ty, d)
            }
            0xF0 | 0xF7 => {
                let len = self.vlq()? as usize;
                Ev::Sysex(self.take(len)?)
            }
            0xF1..=0xFE => bail!("unexpected system message 0x{first:02X} in track"),
            _ => {
                let (status, d1) = if first & 0x80 != 0 {
                    self.running = first;
                    (first, self.byte()?)
                } else {
                    if self.running == 0 {
                        bail!("data byte without status");
                    }
                    (self.running, first)
                };
                let d2 = if matches!(status & 0xF0, 0xC0 | 0xD0) { 0 } else { self.byte()? };
                Ev::Midi(status, d1, d2)
            }
        };
        Ok((self.tick, ev))
    }
}

impl<'a> Iterator for Events<'a> {
    type Item = Result<(u64, Ev<'a>)>;
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.p >= self.b.len() {
            return None;
        }
        let r = self.read();
        if r.is_err() {
            self.done = true;
        }
        Some(r)
    }
}

/// Owned event, for tests and small tools.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    Midi(Vec<u8>),
    Meta(u8, Vec<u8>),
    Sysex(Vec<u8>),
}

pub struct Smf {
    pub format: u16,
    pub ppq: u16,
    pub tracks: Vec<Vec<(u64, Event)>>,
}

/// Parse a whole file into owned events (tests and diagnostics only).
pub fn parse(data: &[u8]) -> Result<Smf> {
    let h = index(data)?;
    let mut tracks = Vec::new();
    for t in &h.tracks {
        let mut out = Vec::new();
        for e in events(t) {
            let (tick, ev) = e?;
            out.push((
                tick,
                match ev {
                    Ev::Midi(s, a, _) if matches!(s & 0xF0, 0xC0 | 0xD0) => Event::Midi(vec![s, a]),
                    Ev::Midi(s, a, b) => Event::Midi(vec![s, a, b]),
                    Ev::Meta(t, d) => Event::Meta(t, d.to_vec()),
                    Ev::Sysex(d) => Event::Sysex(d.to_vec()),
                },
            ));
        }
        tracks.push(out);
    }
    Ok(Smf { format: h.format, ppq: h.ppq, tracks })
}

#[inline]
pub fn vlq(mut n: u32, out: &mut Vec<u8>) {
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

pub fn meta(ty: u8, data: &[u8]) -> Vec<u8> {
    let mut out = vec![0xFF, ty];
    vlq(data.len() as u32, &mut out);
    out.extend_from_slice(data);
    out
}
