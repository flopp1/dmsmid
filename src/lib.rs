//! Convert Domino `.dms` projects to Standard MIDI Files and back.

pub mod chunk;
pub mod dms;
pub mod envelope;
pub mod module;
pub mod pipe;
pub mod smf;
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

/// `.dms`/`.dmp` file bytes → `.mid` written to `out`.
pub fn dms_to_mid<W: Write + Seek>(file: &[u8], module: &module::Module, out: W) -> anyhow::Result<(W, Report)> {
    to_mid::convert(file, module, out)
}

pub use to_dms::{Vel0, Vel0Summary};

/// `.mid` file bytes → `.dms` written to `out`. `vel0` chooses how velocity-0
/// note-ons are read; it is only called if the file has any.
pub fn mid_to_dms<W: Write + Send>(
    file: &[u8],
    module: &module::Module,
    out: W,
    vel0: impl FnOnce(&Vel0Summary) -> Vel0,
) -> anyhow::Result<(W, Report)> {
    to_dms::convert(file, module, out, vel0)
}
