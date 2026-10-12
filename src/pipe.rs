//! Streaming building blocks with bounded memory:
//! - [`Inflater`]: zlib decompression on a background thread, read through [`BlockSource`];
//! - [`compress_parallel`]: pigz-style parallel zlib compression with in-order output.

use anyhow::{anyhow, bail, Result};
use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};
use std::io::Write;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::Mutex;

pub const BLOCK: usize = 1 << 20;
const DICT: usize = 32 * 1024;

// ---------------------------------------------------------------------------
// Decompression

/// Decompress `zlib` (a complete zlib stream, possibly followed by padding) into
/// blocks sent over `tx`. Blocks come back through `recycle` so only a fixed
/// number ever exist. Checks that exactly `expected` bytes come out.
pub fn inflate(zlib: &[u8], expected: u64, tx: SyncSender<Result<Vec<u8>>>, recycle: Receiver<Vec<u8>>) {
    let mut d = Decompress::new(true);
    let mut total = 0u64;
    loop {
        let mut buf = recycle.try_recv().unwrap_or_default();
        buf.clear();
        buf.reserve_exact(BLOCK);
        let mut end = false;
        while buf.len() < BLOCK {
            let input = &zlib[(d.total_in() as usize).min(zlib.len())..];
            let (before_in, before_out) = (d.total_in(), buf.len());
            match d.decompress_vec(input, &mut buf, FlushDecompress::None) {
                Ok(Status::StreamEnd) => {
                    end = true;
                    break;
                }
                Ok(_) if buf.len() == before_out && d.total_in() == before_in => {
                    // No progress despite free output space: the input ran out.
                    let _ = tx.send(Err(anyhow!("compressed data is truncated")));
                    return;
                }
                Ok(_) => {}
                Err(e) => {
                    let _ = tx.send(Err(anyhow!("compressed data is damaged: {e}")));
                    return;
                }
            }
        }
        total += buf.len() as u64;
        if total > expected {
            let _ = tx.send(Err(anyhow!("unpacked size mismatch: header says {expected}, got more")));
            return;
        }
        if end && total != expected {
            let _ = tx.send(Err(anyhow!("unpacked size mismatch: header says {expected}, got {total}")));
            return;
        }
        if tx.send(Ok(buf)).is_err() || end {
            return;
        }
    }
}

/// Reads the block stream produced by [`inflate`].
pub struct BlockSource {
    rx: Receiver<Result<Vec<u8>>>,
    recycle: SyncSender<Vec<u8>>,
    cur: Vec<u8>,
    pos: usize,
    scratch: Vec<u8>,
    /// Bytes consumed so far.
    pub offset: u64,
}

impl BlockSource {
    pub fn new(rx: Receiver<Result<Vec<u8>>>, recycle: SyncSender<Vec<u8>>) -> Self {
        BlockSource { rx, recycle, cur: Vec::new(), pos: 0, scratch: Vec::new(), offset: 0 }
    }

    fn refill(&mut self) -> Result<bool> {
        match self.rx.recv() {
            Ok(Ok(b)) => {
                let old = std::mem::replace(&mut self.cur, b);
                let _ = self.recycle.try_send(old);
                self.pos = 0;
                Ok(true)
            }
            Ok(Err(e)) => Err(e),
            Err(_) => Ok(false),
        }
    }

    /// The next `n` bytes as one contiguous slice.
    #[inline]
    pub fn take(&mut self, n: usize) -> Result<&[u8]> {
        self.offset += n as u64;
        if self.cur.len() - self.pos >= n {
            self.pos += n;
            return Ok(&self.cur[self.pos - n..self.pos]);
        }
        // Spans blocks: assemble in scratch.
        self.scratch.clear();
        while self.scratch.len() < n {
            if self.pos == self.cur.len() && !self.refill()? {
                bail!("file is truncated");
            }
            let k = (n - self.scratch.len()).min(self.cur.len() - self.pos);
            self.scratch.extend_from_slice(&self.cur[self.pos..self.pos + k]);
            self.pos += k;
        }
        Ok(&self.scratch)
    }

    /// Skip `n` bytes without assembling them.
    pub fn skip(&mut self, mut n: u64) -> Result<()> {
        self.offset += n;
        while n > 0 {
            if self.pos == self.cur.len() && !self.refill()? {
                bail!("file is truncated");
            }
            let k = (n.min((self.cur.len() - self.pos) as u64)) as usize;
            self.pos += k;
            n -= k as u64;
        }
        Ok(())
    }

    /// Wait for the decompressor to finish, surfacing any error it reports.
    pub fn finish(mut self) -> Result<()> {
        while self.refill()? {}
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Compression

struct Job {
    idx: u64,
    data: Vec<u8>,
    dict: Vec<u8>,
    last: bool,
}

struct Done {
    idx: u64,
    data: Result<Vec<u8>>,
    adler: u32,
    len: u64,
}

/// The `Write` end of [`compress_parallel`]: cuts the stream into blocks and hands them out.
pub struct ParSink {
    cur: Vec<u8>,
    block: usize,
    idx: u64,
    jobs: SyncSender<Job>,
    permits: Receiver<()>,
    dict: Vec<u8>,
    pub total: u64,
}

impl ParSink {
    fn send(&mut self, last: bool) -> std::io::Result<()> {
        let data = std::mem::replace(&mut self.cur, Vec::with_capacity(self.block));
        // This block's dictionary is the last 32 KB of everything before it.
        let job_dict = std::mem::take(&mut self.dict);
        let tail = &data[data.len().saturating_sub(DICT)..];
        let mut next = Vec::with_capacity(DICT);
        let keep = DICT - tail.len();
        next.extend_from_slice(&job_dict[job_dict.len().saturating_sub(keep)..]);
        next.extend_from_slice(tail);
        self.dict = next;
        let broken = || std::io::Error::other("compression worker stopped");
        self.permits.recv().map_err(|_| broken())?;
        self.jobs.send(Job { idx: self.idx, data, dict: job_dict, last }).map_err(|_| broken())?;
        self.idx += 1;
        Ok(())
    }
}

impl Write for ParSink {
    fn write(&mut self, mut buf: &[u8]) -> std::io::Result<usize> {
        let n = buf.len();
        self.total += n as u64;
        while !buf.is_empty() {
            let k = (self.block - self.cur.len()).min(buf.len());
            self.cur.extend_from_slice(&buf[..k]);
            buf = &buf[k..];
            if self.cur.len() == self.block {
                self.send(false)?;
            }
        }
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Write `header`, then a zlib stream of everything `produce` writes to its sink.
/// Blocks are compressed on all cores; at most a fixed number are in flight.
/// Returns `out` and the number of uncompressed bytes.
pub fn compress_parallel<W: Write + Send>(
    mut out: W,
    header: &[u8],
    level: Compression,
    block: usize,
    produce: impl FnOnce(&mut ParSink) -> Result<()>,
) -> Result<(W, u64)> {
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get());
    let in_flight = 2 * workers + 2;
    let (job_tx, job_rx) = sync_channel::<Job>(in_flight);
    let job_rx = Mutex::new(job_rx);
    let (done_tx, done_rx) = sync_channel::<Done>(in_flight);
    let (permit_tx, permit_rx) = sync_channel::<()>(in_flight);
    for _ in 0..in_flight {
        permit_tx.send(()).unwrap();
    }

    std::thread::scope(|s| {
        for _ in 0..workers {
            let done_tx = done_tx.clone();
            let job_rx = &job_rx;
            s.spawn(move || loop {
                let job = match job_rx.lock().unwrap().recv() {
                    Ok(j) => j,
                    Err(_) => return,
                };
                let data = deflate_block(&job.data, &job.dict, job.last, level);
                let done = Done { idx: job.idx, data, adler: adler32(&job.data), len: job.data.len() as u64 };
                if done_tx.send(done).is_err() {
                    return;
                }
            });
        }
        drop(done_tx);

        let writer = s.spawn(move || -> Result<(W, u64)> {
            out.write_all(header)?;
            out.write_all(&[0x78, 0x9C])?;
            let mut pending = std::collections::BTreeMap::new();
            let mut next = 0u64;
            let mut adler = 1u32;
            let mut total = 0u64;
            for d in done_rx {
                pending.insert(d.idx, d);
                while let Some(d) = pending.remove(&next) {
                    out.write_all(&d.data?)?;
                    adler = adler32_combine(adler, d.adler, d.len);
                    total += d.len;
                    next += 1;
                    let _ = permit_tx.send(());
                }
            }
            if !pending.is_empty() {
                bail!("compression stopped early");
            }
            out.write_all(&adler.to_be_bytes())?;
            out.flush()?;
            Ok((out, total))
        });

        let mut sink = ParSink {
            cur: Vec::with_capacity(block),
            block,
            idx: 0,
            jobs: job_tx,
            permits: permit_rx,
            dict: Vec::new(),
            total: 0,
        };
        let produced = produce(&mut sink).and_then(|()| Ok(sink.send(true)?));
        drop(sink); // closes the job queue, so workers and writer finish
        let written = writer.join().map_err(|_| anyhow!("writer thread panicked"))?;
        produced?;
        written
    })
}

fn deflate_block(data: &[u8], dict: &[u8], last: bool, level: Compression) -> Result<Vec<u8>> {
    let mut c = Compress::new(level, false);
    if !dict.is_empty() {
        c.set_dictionary(dict)?;
    }
    let flush = if last { FlushCompress::Finish } else { FlushCompress::Sync };
    let mut out = Vec::with_capacity(data.len() / 4 + 1024);
    loop {
        if out.capacity() - out.len() < 1024 {
            out.reserve(out.capacity().max(4096));
        }
        let status = c.compress_vec(&data[c.total_in() as usize..], &mut out, flush)?;
        let consumed = c.total_in() as usize == data.len();
        match status {
            Status::StreamEnd => break,
            // A sync flush is complete once it no longer fills the output buffer.
            _ if !last && consumed && out.len() < out.capacity() => break,
            _ => {}
        }
    }
    Ok(out)
}

const MOD: u32 = 65521;

pub fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    // 5552 is the largest n with 255n(n+1)/2 + (n+1)(MOD-1) < 2^32.
    for chunk in data.chunks(5552) {
        for &x in chunk {
            a += x as u32;
            b += a;
        }
        a %= MOD;
        b %= MOD;
    }
    (b << 16) | a
}

/// Adler-32 of A‖B from adler(A), adler(B) and len(B) (zlib's adler32_combine).
pub fn adler32_combine(a1: u32, a2: u32, len2: u64) -> u32 {
    let m = MOD as u64;
    let rem = len2 % m;
    let (a1, a2) = (a1 as u64, a2 as u64);
    let mut sum1 = a1 & 0xFFFF;
    let mut sum2 = (rem * sum1) % m;
    sum1 += (a2 & 0xFFFF) + m - 1;
    sum2 += ((a1 >> 16) & 0xFFFF) + ((a2 >> 16) & 0xFFFF) + m - rem;
    sum1 %= m;
    sum2 %= m;
    ((sum2 << 16) | sum1) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn parallel_stream_decodes_with_small_blocks() {
        // Compressible but irregular data, cut into many tiny blocks.
        let data: Vec<u8> = (0..300_000u32).map(|i| ((i * 7919) % 251 ^ (i / 1000)) as u8).collect();
        let (out, n) = compress_parallel(Vec::new(), b"HDR", Compression::default(), 1000, |w| {
            w.write_all(&data)?;
            Ok(())
        })
        .unwrap();
        assert_eq!(n, data.len() as u64);
        assert_eq!(&out[..3], b"HDR");
        let mut back = Vec::new();
        flate2::read::ZlibDecoder::new(&out[3..]).read_to_end(&mut back).unwrap();
        assert_eq!(back, data);
    }

    #[test]
    fn empty_stream() {
        let (out, _) = compress_parallel(Vec::new(), b"", Compression::default(), BLOCK, |_| Ok(())).unwrap();
        let mut back = Vec::new();
        flate2::read::ZlibDecoder::new(&out[..]).read_to_end(&mut back).unwrap();
        assert!(back.is_empty());
    }

    #[test]
    fn adler_combine() {
        let (a, b) = (b"hello world, ".as_slice(), b"this is adler".as_slice());
        let whole = [a, b].concat();
        assert_eq!(adler32_combine(adler32(a), adler32(b), b.len() as u64), adler32(&whole));
    }
}
