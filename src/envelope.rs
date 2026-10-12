//! `.dms` / `.dmp` / clipboard envelope: ASCII magic, u32 LE unpacked size, zlib stream.

use anyhow::{bail, Context, Result};
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;
use std::io::{Read, Write};

pub const SEQUENCE: &[u8] = b"PortalSequenceData";
pub const PART: &[u8] = b"PortalPartData";

pub fn magic_of(data: &[u8]) -> Option<&'static [u8]> {
    [SEQUENCE, PART].into_iter().find(|m| data.starts_with(m))
}

pub fn decode(data: &[u8]) -> Result<Vec<u8>> {
    let Some(magic) = magic_of(data) else { bail!("not Domino data (unknown header)") };
    let rest = &data[magic.len()..];
    if rest.len() < 4 {
        bail!("file is truncated");
    }
    let size = u32::from_le_bytes(rest[..4].try_into().unwrap()) as usize;
    let mut out = Vec::with_capacity(size);
    // The decoder stops at the end of the zlib stream, so trailing padding is ignored.
    ZlibDecoder::new(&rest[4..])
        .read_to_end(&mut out)
        .context("compressed data is damaged")?;
    if out.len() != size {
        bail!("unpacked size mismatch: header says {size}, got {}", out.len());
    }
    Ok(out)
}

pub fn encode(unpacked: &[u8]) -> Vec<u8> {
    let mut out = SEQUENCE.to_vec();
    out.extend_from_slice(&(unpacked.len() as u32).to_le_bytes());
    let mut z = ZlibEncoder::new(out, Compression::default());
    z.write_all(unpacked).expect("writing to memory");
    z.finish().expect("writing to memory")
}
