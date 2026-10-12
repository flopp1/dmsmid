//! Streaming building blocks with bounded memory:
//! - [`Inflater`]: zlib decompression on a background thread, read through [`BlockSource`];
//! - [`compress_parallel`]: pigz-style parallel zlib compression with in-order output,
//!   plus out-of-order [`Segment`]s spliced in later.

use anyhow::{anyhow, bail, Result};
use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
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
    stream: u32,
    idx: u64,
    data: Vec<u8>,
    dict: Vec<u8>,
    last: bool,
}

/// Messages to the writer thread.
enum Msg {
    Block { stream: u32, idx: u64, data: Result<Vec<u8>>, adler: u32, len: u64 },
    /// Segment `stream` is complete after `blocks` blocks.
    SegEnd { stream: u32, blocks: u64 },
    /// Insert these segments into the main stream before its block `at`.
    Splice { at: u64, segs: Vec<u32> },
}

/// A run of uncompressed data cut into blocks; each block's dictionary is the
/// previous 32 KB of the same run.
struct Run {
    id: u32,
    cur: Vec<u8>,
    idx: u64,
    dict: Vec<u8>,
}

impl Run {
    fn new(id: u32, block: usize) -> Self {
        Run { id, cur: Vec::with_capacity(block), idx: 0, dict: Vec::new() }
    }
}

/// A stretch of output that is written out of order and spliced into the main
/// stream later (see [`ParSink::splice`]). It is compressed like the main stream;
/// its compressed blocks wait in a temporary file, so memory stays bounded.
pub struct Segment(Run);

struct Shared {
    jobs: SyncSender<Job>,
    permits: Receiver<()>,
    block: usize,
}

impl Shared {
    fn send(&self, run: &mut Run, last: bool) -> std::io::Result<()> {
        let data = std::mem::replace(&mut run.cur, Vec::with_capacity(self.block));
        // This block's dictionary is the last 32 KB of the run before it.
        let job_dict = std::mem::take(&mut run.dict);
        let tail = &data[data.len().saturating_sub(DICT)..];
        let mut next = Vec::with_capacity(DICT);
        let keep = DICT - tail.len();
        next.extend_from_slice(&job_dict[job_dict.len().saturating_sub(keep)..]);
        next.extend_from_slice(tail);
        run.dict = next;
        let broken = || std::io::Error::other("compression worker stopped");
        self.permits.recv().map_err(|_| broken())?;
        self.jobs.send(Job { stream: run.id, idx: run.idx, data, dict: job_dict, last }).map_err(|_| broken())?;
        run.idx += 1;
        Ok(())
    }

    fn write(&self, run: &mut Run, mut buf: &[u8]) -> std::io::Result<()> {
        while !buf.is_empty() {
            let k = (self.block - run.cur.len()).min(buf.len());
            run.cur.extend_from_slice(&buf[..k]);
            buf = &buf[k..];
            if run.cur.len() == self.block {
                self.send(run, false)?;
            }
        }
        Ok(())
    }
}

/// The `Write` end of [`compress_parallel`]: cuts the stream into blocks and hands them out.
pub struct ParSink {
    shared: Shared,
    main: Run,
    msgs: SyncSender<Msg>,
    next_id: u32,
    /// Uncompressed bytes written so far, segments included.
    pub total: u64,
}

impl ParSink {
    /// Start a segment, to be filled with [`ParSink::write_segment`].
    pub fn segment(&mut self) -> Segment {
        self.next_id += 1;
        Segment(Run::new(self.next_id, self.shared.block))
    }

    pub fn write_segment(&mut self, seg: &mut Segment, buf: &[u8]) -> std::io::Result<()> {
        self.total += buf.len() as u64;
        self.shared.write(&mut seg.0, buf)
    }

    /// Place `segs`, in order, after everything written to the main stream so far.
    pub fn splice(&mut self, segs: Vec<Segment>) -> std::io::Result<()> {
        if segs.is_empty() {
            return Ok(());
        }
        let broken = || std::io::Error::other("compression writer stopped");
        if !self.main.cur.is_empty() {
            self.shared.send(&mut self.main, false)?;
        }
        let mut ids = Vec::with_capacity(segs.len());
        for mut s in segs {
            if !s.0.cur.is_empty() {
                self.shared.send(&mut s.0, false)?;
            }
            self.msgs.send(Msg::SegEnd { stream: s.0.id, blocks: s.0.idx }).map_err(|_| broken())?;
            ids.push(s.0.id);
        }
        self.msgs.send(Msg::Splice { at: self.main.idx, segs: ids }).map_err(|_| broken())?;
        // What now precedes the main stream is the last segment, not the old main
        // tail, so the next main block must not refer back into it.
        self.main.dict.clear();
        Ok(())
    }
}

impl Write for ParSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.total += buf.len() as u64;
        self.shared.write(&mut self.main, buf)?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Ordered output of one stream: compressed blocks waiting for their turn.
struct Ordered {
    pending: BTreeMap<u64, (Vec<u8>, u32, u64)>,
    next: u64,
    adler: u32,
    len: u64,
}

impl Ordered {
    fn new() -> Self {
        Ordered { pending: BTreeMap::new(), next: 0, adler: 1, len: 0 }
    }

    /// Write the blocks that are next in order; returns how many were written.
    fn drain(&mut self, out: &mut impl Write) -> std::io::Result<usize> {
        let mut n = 0;
        while let Some((data, adler, len)) = self.pending.remove(&self.next) {
            out.write_all(&data)?;
            self.adler = adler32_combine(self.adler, adler, len);
            self.len += len;
            self.next += 1;
            n += 1;
        }
        Ok(n)
    }
}

/// A segment's compressed blocks, spooled to a temporary file until spliced.
struct Spool {
    blocks: Ordered,
    file: Option<File>,
    path: PathBuf,
    end: Option<u64>,
}

impl Spool {
    fn new(id: u32) -> Self {
        static RUN: AtomicU64 = AtomicU64::new(0);
        let n = RUN.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("dmsmid-{}-{n}-{id}.tmp", std::process::id()));
        Spool { blocks: Ordered::new(), file: None, path, end: None }
    }

    fn drain(&mut self) -> std::io::Result<usize> {
        if self.blocks.pending.contains_key(&self.blocks.next) && self.file.is_none() {
            self.file = Some(File::options().read(true).write(true).create_new(true).open(&self.path)?);
        }
        match &mut self.file {
            Some(f) => self.blocks.drain(f),
            None => Ok(0),
        }
    }

    fn complete(&self) -> bool {
        self.end == Some(self.blocks.next)
    }
}

impl Drop for Spool {
    fn drop(&mut self) {
        if self.file.take().is_some() {
            let _ = std::fs::remove_file(&self.path);
        }
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
    let (msg_tx, msg_rx) = sync_channel::<Msg>(in_flight);
    let (permit_tx, permit_rx) = sync_channel::<()>(in_flight);
    for _ in 0..in_flight {
        permit_tx.send(()).unwrap();
    }

    std::thread::scope(|s| {
        for _ in 0..workers {
            let msg_tx = msg_tx.clone();
            let job_rx = &job_rx;
            s.spawn(move || loop {
                let job = match job_rx.lock().unwrap().recv() {
                    Ok(j) => j,
                    Err(_) => return,
                };
                let data = deflate_block(&job.data, &job.dict, job.last, level);
                let msg = Msg::Block {
                    stream: job.stream,
                    idx: job.idx,
                    data,
                    adler: adler32(&job.data),
                    len: job.data.len() as u64,
                };
                if msg_tx.send(msg).is_err() {
                    return;
                }
            });
        }

        let writer = s.spawn(move || -> Result<(W, u64)> {
            out.write_all(header)?;
            out.write_all(&[0x78, 0x9C])?;
            let mut main = Ordered::new();
            let mut spools: HashMap<u32, Spool> = HashMap::new();
            let mut splices: VecDeque<(u64, Vec<u32>)> = VecDeque::new();
            let release = |n: usize| {
                for _ in 0..n {
                    let _ = permit_tx.send(());
                }
            };
            for msg in msg_rx {
                match msg {
                    Msg::Block { stream, idx, data, adler, len } => {
                        let blocks = match stream {
                            0 => &mut main,
                            s => &mut spools.entry(s).or_insert_with(|| Spool::new(s)).blocks,
                        };
                        blocks.pending.insert(idx, (data?, adler, len));
                    }
                    Msg::SegEnd { stream, blocks } => {
                        spools.entry(stream).or_insert_with(|| Spool::new(stream)).end = Some(blocks);
                    }
                    Msg::Splice { at, segs } => splices.push_back((at, segs)),
                }
                for sp in spools.values_mut() {
                    release(sp.drain()?);
                }
                loop {
                    if let Some((at, ids)) = splices.front() {
                        if *at == main.next {
                            if !ids.iter().all(|id| spools.get(id).is_some_and(Spool::complete)) {
                                break; // wait for the segments to finish
                            }
                            let (_, ids) = splices.pop_front().unwrap();
                            for id in ids {
                                let mut sp = spools.remove(&id).unwrap();
                                if let Some(f) = &mut sp.file {
                                    f.seek(SeekFrom::Start(0))?;
                                    std::io::copy(f, &mut out)?;
                                }
                                main.adler = adler32_combine(main.adler, sp.blocks.adler, sp.blocks.len);
                                main.len += sp.blocks.len;
                            }
                            continue;
                        }
                    }
                    // Main blocks up to the next splice point.
                    let limit = splices.front().map_or(u64::MAX, |s| s.0);
                    match main.pending.first_key_value() {
                        Some((&k, _)) if k == main.next && k < limit => {
                            let (data, adler, len) = main.pending.remove(&k).unwrap();
                            out.write_all(&data)?;
                            main.adler = adler32_combine(main.adler, adler, len);
                            main.len += len;
                            main.next += 1;
                            release(1);
                        }
                        _ => break,
                    }
                }
            }
            if !main.pending.is_empty() || !splices.is_empty() || !spools.is_empty() {
                bail!("compression stopped early");
            }
            out.write_all(&main.adler.to_be_bytes())?;
            out.flush()?;
            Ok((out, main.len))
        });

        let mut sink = ParSink {
            shared: Shared { jobs: job_tx, permits: permit_rx, block },
            main: Run::new(0, block),
            msgs: msg_tx,
            next_id: 0,
            total: 0,
        };
        let produced = produce(&mut sink).and_then(|()| {
            let s = &mut sink;
            Ok(s.shared.send(&mut s.main, true)?)
        });
        drop(sink); // closes the job queue and the sink's message sender
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
    fn segments_are_spliced_in_order() {
        let part = |c: u8, n: usize| -> Vec<u8> { (0..n).map(|i| c.wrapping_add((i % 7) as u8)).collect() };
        let (a, b, c, d) = (part(b'a', 5000), part(b'b', 7777), part(b'c', 123), part(b'd', 4000));
        let (out, n) = compress_parallel(Vec::new(), b"", Compression::default(), 1000, |w| {
            // Interleave writes to the main stream and two segments, plus an empty one.
            let (mut s1, mut s2, s3) = (w.segment(), w.segment(), w.segment());
            w.write_all(&a[..2500])?;
            w.write_segment(&mut s2, &c)?;
            w.write_segment(&mut s1, &b[..3000])?;
            w.write_all(&a[2500..])?;
            w.write_segment(&mut s1, &b[3000..])?;
            w.splice(vec![s1, s2, s3])?;
            w.write_all(&d)?;
            Ok(())
        })
        .unwrap();
        let want = [a, b, c, d].concat();
        assert_eq!(n, want.len() as u64);
        let mut back = Vec::new();
        flate2::read::ZlibDecoder::new(&out[..]).read_to_end(&mut back).unwrap();
        assert_eq!(back, want);
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
