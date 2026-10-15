//! dmsmid: pick a Domino .dms or a .mid file; it is converted to the other format
//! and saved next to the original (never overwriting an existing file).
//!
//!   dmsmid                         open a file dialog
//!   dmsmid FILE...                 convert the given files (also: drag files onto the exe)
//!   dmsmid --module X.xml [FILE]   use another Domino module definition (default: built-in GM Level1)
//!   dmsmid --vel0 MODE [FILE]      how to read velocity-0 note-ons in MIDI files:
//!                                  note-off (default), silent (a silent note) or auto.
//!
//! MIDI files too large for one .dms file (4 GB uncompressed) can be split:
//!   --split measure|tick|track|track-partial   how to split (asked if not given)
//!   --split-size BYTES             largest uncompressed .dms size (default and maximum: 4294967295)
//!   --split-offset keep|remove     time-based parts keep their position or start at 0 (default keep)
//!   --split-cut measure|tick       track-based splits: how to cut a track (default measure)
//!
//!   --track-split N                write one .dms per N tracks (counted after multi-channel
//!                                  tracks are separated), each with all global events:
//!                                  song (Track 1-N).dms, song (Track N+1-2N).dms, ...
//!                                  A file still too large is split as above:
//!                                  song (Track 1-N, part 1 of 2).dms
//!
//! Without these options, you are asked on the console when a choice matters.

use anyhow::{bail, Context, Result};
use dmsmid::module::Module;
use dmsmid::{detect, Choices, FilePlan, Kind, Options, Progress, SplitMethod, TimeCut, Vel0, Vel0Summary, MAX_UNPACKED};
use memmap2::Mmap;
use std::fs::File;
use std::io::{BufWriter, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn main() {
    let interactive = std::env::args().len() <= 1;
    let code = match run() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("Error: {e:#}");
            1
        }
    };
    if interactive {
        // Started by double-click: keep the console window open.
        eprintln!("\nPress Enter to close.");
        let _ = std::io::stdin().read_line(&mut String::new());
    }
    std::process::exit(code);
}

/// Settings from the command line; `None` means "ask if it matters".
#[derive(Clone, Copy, Default)]
struct Flags {
    vel0: Option<Vel0>,
    split: Option<SplitMethod>,
    split_size: Option<u64>,
    keep_offset: Option<bool>,
    cut: Option<TimeCut>,
    track_split: Option<usize>,
}

fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut module = None;
    let mut flags = Flags::default();
    let mut files = Vec::new();
    while let Some(a) = args.next() {
        let mut value = |what: &str| args.next().with_context(|| format!("{a} needs {what}"));
        match a.as_str() {
            "--module" => {
                let p = value("a file")?;
                let xml = std::fs::read(&p).with_context(|| format!("reading {p}"))?;
                module = Some(Module::parse(&xml).with_context(|| format!("reading module {p}"))?);
            }
            "--vel0" => {
                let m = value("note-off, silent or auto")?;
                flags.vel0 = Some(parse_vel0(&m).with_context(|| format!("--vel0 {m}: use note-off, silent or auto"))?);
            }
            "--split" => {
                let m = value("measure, tick, track or track-partial")?;
                flags.split = Some(
                    parse_split(&m).with_context(|| format!("--split {m}: use measure, tick, track or track-partial"))?,
                );
            }
            "--split-size" => {
                let m = value("a size in bytes")?;
                let n: u64 = m.trim().parse().with_context(|| format!("--split-size {m}: not a number of bytes"))?;
                if n > MAX_UNPACKED {
                    bail!("--split-size {n}: the .dms format holds at most {MAX_UNPACKED} bytes");
                }
                flags.split_size = Some(n);
            }
            "--split-offset" => {
                let m = value("keep or remove")?;
                flags.keep_offset = Some(match m.as_str() {
                    "keep" => true,
                    "remove" => false,
                    _ => bail!("--split-offset {m}: use keep or remove"),
                });
            }
            "--track-split" => {
                let m = value("a number of tracks")?;
                let n: usize = m.trim().parse().with_context(|| format!("--track-split {m}: not a number"))?;
                if n == 0 {
                    bail!("--track-split needs at least 1 track per file");
                }
                flags.track_split = Some(n);
            }
            "--split-cut" => {
                let m = value("measure or tick")?;
                flags.cut = Some(parse_cut(&m).with_context(|| format!("--split-cut {m}: use measure or tick"))?);
            }
            _ if a.starts_with("--") => bail!("unknown option {a}"),
            _ => files.push(PathBuf::from(a)),
        }
    }
    let module = module.unwrap_or_else(Module::builtin);

    if files.is_empty() {
        match rfd::FileDialog::new()
            .set_title("Choose a Domino or MIDI file to convert")
            .add_filter("Domino or MIDI", &["dms", "dmp", "mid", "midi"])
            .add_filter("All files", &["*"])
            .pick_files()
        {
            Some(f) => files = f,
            None => bail!("no file chosen"),
        }
    }

    let mut failed = 0;
    for f in &files {
        if let Err(e) = convert_file(f, &module, flags) {
            eprintln!("Error: {}: {e:#}", f.display());
            failed += 1;
        }
    }
    if failed > 0 {
        bail!("{failed} of {} file(s) failed", files.len());
    }
    Ok(())
}

fn convert_file(path: &Path, module: &Module, flags: Flags) -> Result<()> {
    let file = File::open(path).with_context(|| format!("reading {}", path.display()))?;
    if file.metadata()?.len() == 0 {
        bail!("the file is empty");
    }
    // SAFETY: the mapping is read-only; the file must not be changed while converting.
    let data = unsafe { Mmap::map(&file) }.with_context(|| format!("reading {}", path.display()))?;
    let Some(kind) = detect(&data) else { bail!("not a Domino (.dms) or Standard MIDI (.mid) file") };

    let progress = Progress::default();
    let stop = AtomicBool::new(false);
    let started = Instant::now();
    let mut created: Vec<PathBuf> = Vec::new();
    let mut waited = Duration::ZERO;
    let result = std::thread::scope(|s| {
        if std::io::stderr().is_terminal() {
            s.spawn(|| draw_progress(&progress, &stop, started));
        }
        let r = match kind {
            Kind::Domino => {
                // Never split; split options do not apply.
                let out = free_path(path, "", "mid");
                created.push(out.clone());
                File::create(&out)
                    .with_context(|| format!("writing {}", out.display()))
                    .and_then(|f| dmsmid::dms_to_mid(&data, module, f, &progress))
                    .map(|r| r.1)
            }
            Kind::Midi => {
                let opts = Options {
                    split_size: flags.split_size.unwrap_or(MAX_UNPACKED),
                    track_split: flags.track_split,
                    ..Options::default()
                };
                let mut ask = Ask { flags, progress: &progress, name: display_name(path), waited: Duration::ZERO };
                let created = &mut created;
                let open = |file: &FilePlan| -> Result<BufWriter<File>> {
                    let out = free_path(path, &file.suffix(), "dms");
                    created.push(out.clone());
                    let f = File::create(&out).with_context(|| format!("writing {}", out.display()))?;
                    Ok(BufWriter::with_capacity(1 << 20, f))
                };
                let r = dmsmid::mid_to_dms(&data, module, &opts, &mut ask, open, &progress).map(|r| r.1);
                waited = ask.waited;
                r
            }
        };
        stop.store(true, Ordering::Relaxed);
        r
    });
    let elapsed = started.elapsed().saturating_sub(waited);
    let report = match result {
        Ok(r) => r,
        Err(e) => {
            for p in &created {
                let _ = std::fs::remove_file(p);
            }
            return Err(e);
        }
    };
    println!("{}", path.display());
    for p in &created {
        println!("  -> {}", p.display());
    }
    println!(
        "  {} track(s), {} note(s), module '{}', {:.2} s",
        report.tracks,
        report.notes,
        module.name,
        elapsed.as_secs_f64()
    );
    for (msg, n) in &report.warnings {
        println!("  note: {msg} x{n}");
    }
    Ok(())
}

fn display_name(path: &Path) -> String {
    path.file_name().unwrap_or_default().to_string_lossy().into_owned()
}

/// `song.mid` → `song.dms`, or with a suffix `song (Track 1-4, part 2 of 3).dms`;
/// adds ` (2)`, ` (3)`, ... if the name is taken.
fn free_path(src: &Path, suffix: &str, ext: &str) -> PathBuf {
    let stem = src.file_stem().unwrap_or_default().to_string_lossy();
    let stem = if suffix.is_empty() { stem.into_owned() } else { format!("{stem} ({suffix})") };
    let dir = src.parent().unwrap_or(Path::new("."));
    let mut p = dir.join(format!("{stem}.{ext}"));
    let mut n = 2;
    while p.exists() {
        p = dir.join(format!("{stem} ({n}).{ext}"));
        n += 1;
    }
    p
}

// ---------------------------------------------------------------------------
// Questions

/// Answers from flags, or from the console when a choice matters and a person is there.
struct Ask<'p> {
    flags: Flags,
    progress: &'p Progress,
    name: String,
    /// Time spent waiting for answers (not counted as conversion time).
    waited: Duration,
}

impl Ask<'_> {
    fn interactive() -> bool {
        std::io::stdin().is_terminal()
    }

    /// Show `lines`, then read a choice until `parse` accepts it; Enter picks the default.
    fn question<T>(&mut self, lines: &[String], prompt: &str, parse: impl Fn(&str) -> Option<T>, default: T) -> T {
        let asked = Instant::now();
        self.progress.set_paused(true);
        std::thread::sleep(Duration::from_millis(120)); // let the bar finish its last frame
        clear_line();
        println!();
        for l in lines {
            println!("{l}");
        }
        let answer = loop {
            print!("{prompt}");
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                break default; // Enter, or no input available
            }
            match parse(&line) {
                Some(v) => break v,
                None => println!("Please type one of the numbers shown."),
            }
        };
        self.progress.set_paused(false);
        self.waited += asked.elapsed();
        answer
    }
}

impl Choices for Ask<'_> {
    fn vel0(&mut self, s: &Vel0Summary) -> Vel0 {
        match self.flags.vel0 {
            Some(v) => v,
            None if s.ambiguous() && Self::interactive() => {
                let lines = [
                    format!(
                        "{} has {} velocity-0 note-on(s), and the two ways of reading them disagree:",
                        self.name,
                        thousands(s.vel0_note_ons as u64)
                    ),
                    format!(
                        "  1) note-off     (MIDI standard, what Domino does)   {:>14} note(s) left unpaired",
                        thousands(s.unpaired_as_note_off as u64)
                    ),
                    format!(
                        "  2) silent note  (ends at its own later note-off)    {:>14} note(s) left unpaired",
                        thousands(s.unpaired_as_silent_note as u64)
                    ),
                    format!(
                        "  3) auto         (best fit per track: {} track(s) read as silent notes)",
                        s.tracks_better_as_silent
                    ),
                ];
                self.question(&lines, "How should they be read? [1]: ", parse_vel0, Vel0::NoteOff)
            }
            None => Vel0::NoteOff,
        }
    }

    fn split(&mut self, size: u64, limit: u64) -> Option<SplitMethod> {
        if self.flags.split.is_some() || !Self::interactive() {
            return self.flags.split;
        }
        let lines = [
            format!(
                "{} needs {} bytes as a .dms file, but one file can hold at most {} (uncompressed).",
                self.name,
                thousands(size),
                thousands(limit)
            ),
            "It can be split into several .dms files:".into(),
            "  1) by measure        (time: cut at the last whole measure that fits)".into(),
            "  2) by tick           (time: cut exactly where the file would overflow)".into(),
            "  3) by track          (whole tracks; a track too large on its own is split by time)".into(),
            "  4) by track, packed  (fill every file; the track at each boundary is split by time)".into(),
            "  0) don't convert".into(),
        ];
        let parse = |s: &str| match s.trim() {
            "0" => Some(None),
            other => parse_split(other).map(Some),
        };
        self.question(&lines, "How should it be split? [1]: ", parse, Some(SplitMethod::Measure))
    }

    fn keep_offset(&mut self) -> bool {
        if let Some(k) = self.flags.keep_offset {
            return k;
        }
        if !Self::interactive() {
            return true;
        }
        let lines = [
            "Parts that start later in the song can:".into(),
            "  1) keep their position  (the empty time before the part's start stays, so parts line up)".into(),
            "  2) start at 0           (the empty time at the beginning is removed)".into(),
        ];
        let parse = |s: &str| match s.trim().to_ascii_lowercase().as_str() {
            "1" | "keep" => Some(true),
            "2" | "remove" => Some(false),
            _ => None,
        };
        self.question(&lines, "Keep or remove the time offset? [1]: ", parse, true)
    }

    fn partial_cut(&mut self) -> TimeCut {
        if let Some(c) = self.flags.cut {
            return c;
        }
        if !Self::interactive() {
            return TimeCut::Measure;
        }
        let lines = [
            "Some tracks have to be split across files. Cut them:".into(),
            "  1) by measure  (at the last whole measure that fits)".into(),
            "  2) by tick     (exactly where the file would overflow)".into(),
        ];
        self.question(&lines, "How should tracks be cut? [1]: ", parse_cut, TimeCut::Measure)
    }
}

fn parse_vel0(s: &str) -> Option<Vel0> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "note-off" | "noteoff" | "off" => Some(Vel0::NoteOff),
        "2" | "silent" | "silent-note" | "note" => Some(Vel0::SilentNote),
        "3" | "auto" => Some(Vel0::Auto),
        _ => None,
    }
}

fn parse_split(s: &str) -> Option<SplitMethod> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "measure" => Some(SplitMethod::Measure),
        "2" | "tick" => Some(SplitMethod::Tick),
        "3" | "track" => Some(SplitMethod::Track),
        "4" | "track-partial" | "packed" => Some(SplitMethod::TrackPartial),
        _ => None,
    }
}

fn parse_cut(s: &str) -> Option<TimeCut> {
    match s.trim().to_ascii_lowercase().as_str() {
        "1" | "measure" => Some(TimeCut::Measure),
        "2" | "tick" => Some(TimeCut::Tick),
        _ => None,
    }
}

fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

// ---------------------------------------------------------------------------
// Progress bar

/// Draw `p` on stderr every 100 ms until `stop` is set.
fn draw_progress(p: &Progress, stop: &AtomicBool, started: Instant) {
    const WIDTH: usize = 30;
    let spinner = ['|', '/', '-', '\\'];
    let mut frame = 0;
    let mut drawn = false;
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(100));
        if p.is_paused() || stop.load(Ordering::Relaxed) {
            continue;
        }
        let (phase, done, total) = p.snapshot();
        let secs = started.elapsed().saturating_sub(p.paused_time()).as_secs_f64();
        let line = if total > 0 {
            let f = (done as f64 / total as f64).clamp(0.0, 1.0);
            let filled = (f * WIDTH as f64).round() as usize;
            format!(
                "{phase:<22} [{}{}] {:>5.1}%  {secs:>6.1} s",
                "#".repeat(filled),
                " ".repeat(WIDTH - filled),
                f * 100.0
            )
        } else {
            frame = (frame + 1) % spinner.len();
            format!("{phase:<22} [{}]  {secs:>6.1} s", spinner[frame])
        };
        eprint!("\r{line}  ");
        let _ = std::io::stderr().flush();
        drawn = true;
    }
    if drawn {
        clear_line();
    }
}

fn clear_line() {
    if std::io::stderr().is_terminal() {
        eprint!("\r{}\r", " ".repeat(78));
        let _ = std::io::stderr().flush();
    }
}
