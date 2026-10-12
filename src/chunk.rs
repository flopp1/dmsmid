//! Domino chunk grammar: `id:u16 size:u32 body[size]`, little-endian, nestable.
//! Everything here borrows from the input; nothing is copied.

pub fn put(out: &mut Vec<u8>, id: u16, body: &[u8]) {
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
}

pub fn chunk(id: u16, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 6);
    put(&mut out, id, body);
    out
}

/// A numeric value list as used by CONTROL data and TEMPO: chunk id 0 holding an f32.
pub fn float_list(v: f32) -> [u8; 10] {
    let mut out = [0, 0, 4, 0, 0, 0, 0, 0, 0, 0];
    out[6..].copy_from_slice(&v.to_le_bytes());
    out
}

/// Decode a hex string (whitespace ignored); used for templates.
pub fn hex(s: &str) -> Vec<u8> {
    let digits: Vec<u8> = s.bytes().filter(|b| b.is_ascii_hexdigit()).collect();
    digits
        .chunks(2)
        .map(|p| u8::from_str_radix(std::str::from_utf8(p).unwrap(), 16).unwrap())
        .collect()
}

/// Iterate the chunks of `buf`. Stops at the first incomplete chunk; check
/// [`Chunks::rest`] afterwards to see whether anything was left over.
pub struct Chunks<'a> {
    buf: &'a [u8],
}

pub fn chunks(buf: &[u8]) -> Chunks<'_> {
    Chunks { buf }
}

impl<'a> Chunks<'a> {
    pub fn rest(&self) -> &'a [u8] {
        self.buf
    }
}

impl<'a> Iterator for Chunks<'a> {
    type Item = (u16, &'a [u8]);
    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        let b = self.buf;
        if b.len() < 6 {
            return None;
        }
        let id = u16::from_le_bytes([b[0], b[1]]);
        let size = u32::from_le_bytes([b[2], b[3], b[4], b[5]]) as usize;
        if size > b.len() - 6 {
            return None;
        }
        self.buf = &b[6 + size..];
        Some((id, &b[6..6 + size]))
    }
}

/// The fields of one container (an event or a track), looked up by id.
#[derive(Clone, Copy)]
pub struct Fields<'a> {
    pub body: &'a [u8],
}

impl<'a> Fields<'a> {
    pub fn new(body: &'a [u8]) -> Self {
        Fields { body }
    }

    /// True if the body parses as complete chunks with nothing left over.
    pub fn is_valid(&self) -> bool {
        let mut it = chunks(self.body);
        for _ in it.by_ref() {}
        it.rest().is_empty()
    }

    pub fn get(&self, id: u16) -> Option<&'a [u8]> {
        chunks(self.body).find(|c| c.0 == id).map(|c| c.1)
    }

    /// Unsigned integer of whatever width the chunk has (1, 2 or 4 bytes).
    pub fn uint(&self, id: u16) -> Option<u32> {
        uint(self.get(id)?)
    }

    /// Signed integer of whatever width the chunk has (1, 2 or 4 bytes).
    pub fn sint(&self, id: u16) -> Option<i32> {
        let b = self.get(id)?;
        match b.len() {
            1 => Some(b[0] as i8 as i32),
            2 => Some(i16::from_le_bytes([b[0], b[1]]) as i32),
            4 => Some(i32::from_le_bytes(b.try_into().unwrap())),
            _ => None,
        }
    }

    /// First entry of a value list (`00 00 04 00 00 00 <f32>`).
    pub fn float(&self, id: u16) -> Option<f32> {
        let (_, v) = chunks(self.get(id)?).find(|c| c.0 == 0 && c.1.len() == 4)?;
        Some(f32::from_le_bytes(v.try_into().unwrap()))
    }
}

pub fn uint(b: &[u8]) -> Option<u32> {
    match b.len() {
        1 => Some(b[0] as u32),
        2 => Some(u16::from_le_bytes([b[0], b[1]]) as u32),
        4 => Some(u32::from_le_bytes(b.try_into().unwrap())),
        _ => None,
    }
}
