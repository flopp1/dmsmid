//! Convert Domino `.dms` projects to Standard MIDI Files and back.

pub mod chunk;
pub mod dms;
pub mod envelope;
pub mod module;
pub mod pipe;
pub mod progress;
pub mod smf;
mod split;
pub mod to_dms;
pub mod to_mid;

use std::collections::BTreeMap;
use std::io::{Seek, Write};

/// What a conversion did, plus counted warnings about anything not carried over.
#[derive(Default, Debug)]
pub struct Report {
    pub tracks: usize,
    pub notes: usize,
    pub warnings: BTreeMap<String, usize>,
}

impl Report {
    pub fn warn(&mut self, msg: impl Into<String>) {
        *self.warnings.entry(msg.into()).or_default() += 1;
    }

    /// Add `n` occurrences of a warning (nothing if `n` is 0).
    pub fn warn_n(&mut self, msg: impl Into<String>, n: usize) {
        if n > 0 {
            *self.warnings.entry(msg.into()).or_default() += n;
        }
    }

    pub fn merge(&mut self, other: Report) {
        self.tracks += other.tracks;
        self.notes += other.notes;
        for (k, n) in other.warnings {
            *self.warnings.entry(k).or_default() += n;
        }
    }
}

pub enum Kind {
    Domino,
    Midi,
}

pub fn detect(data: &[u8]) -> Option<Kind> {
    if envelope::magic_of(data).is_some() {
        Some(Kind::Domino)
    } else if data.starts_with(b"MThd") {
        Some(Kind::Midi)
    } else {
        None
    }
}

/// `.dms`/`.dmp` file bytes → `.mid` written to `out`. Never split.
pub fn dms_to_mid<W: Write + Seek>(
    file: &[u8],
    module: &module::Module,
    out: W,
    progress: &progress::Progress,
) -> anyhow::Result<(W, Report)> {
    to_mid::convert(file, module, out, progress)
}

pub use progress::Progress;
pub use to_dms::{Answers, Choices, FilePlan, Options, SplitMethod, TimeCut, Vel0, Vel0Summary, MAX_UNPACKED};

/// `.mid` file bytes → one or more `.dms` files. `open` supplies the writer for each
/// file; there is one file unless the song is too large for `opts.split_size` (and
/// `choices` picks a way to split it) or `opts.track_split` is set.
pub fn mid_to_dms<W: Write + Send>(
    file: &[u8],
    module: &module::Module,
    opts: &Options,
    choices: &mut dyn Choices,
    open: impl FnMut(&FilePlan) -> anyhow::Result<W>,
    progress: &progress::Progress,
) -> anyhow::Result<(Vec<W>, Report)> {
    to_dms::convert(file, module, opts, choices, open, progress)
}
