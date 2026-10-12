//! dmsmid: pick a Domino .dms or a .mid file; it is converted to the other format
//! and saved next to the original (never overwriting an existing file).
//!
//!   dmsmid                         open a file dialog
//!   dmsmid FILE...                 convert the given files (also: drag files onto the exe)
//!   dmsmid --module X.xml [FILE]   use another Domino module definition (default: built-in GM Level1)
//!   dmsmid --vel0 MODE [FILE]      how to read velocity-0 note-ons in MIDI files:
//!                                  note-off (default), silent (a silent note) or auto.
//!                                  Without it, you are asked when the choice matters.

use anyhow::{bail, Context, Result};
use dmsmid::module::Module;
use dmsmid::{detect, Kind, Vel0, Vel0Summary};
use memmap2::Mmap;
use std::cell::Cell;
use std::fs::File;
use std::io::{BufWriter, IsTerminal, Write};
use std::path::{Path, PathBuf};
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

fn run() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut module = None;
    let mut vel0 = None;
    let mut files = Vec::new();
    while let Some(a) = args.next() {
        if a == "--module" {
            let p = args.next().context("--module needs a file")?;
            let xml = std::fs::read(&p).with_context(|| format!("reading {p}"))?;
            module = Some(Module::parse(&xml).with_context(|| format!("reading module {p}"))?);
        } else if a == "--vel0" {
            let m = args.next().context("--vel0 needs note-off, silent or auto")?;
            vel0 = Some(parse_vel0(&m).with_context(|| format!("--vel0 {m}: use note-off, silent or auto"))?);
        } else {
            files.push(PathBuf::from(a));
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
        if let Err(e) = convert_file(f, &module, vel0) {
            eprintln!("Error: {}: {e:#}", f.display());
            failed += 1;
        }
    }
    if failed > 0 {
        bail!("{failed} of {} file(s) failed", files.len());
    }
    Ok(())
}

fn convert_file(path: &Path, module: &Module, vel0: Option<Vel0>) -> Result<()> {
    let started = Instant::now();
    let waited = Cell::new(Duration::ZERO);
    let file = File::open(path).with_context(|| format!("reading {}", path.display()))?;
    if file.metadata()?.len() == 0 {
        bail!("the file is empty");
    }
    // SAFETY: the mapping is read-only; the file must not be changed while converting.
    let data = unsafe { Mmap::map(&file) }.with_context(|| format!("reading {}", path.display()))?;
    let (kind, ext) = match detect(&data) {
        Some(Kind::Domino) => (Kind::Domino, "mid"),
        Some(Kind::Midi) => (Kind::Midi, "dms"),
        None => bail!("not a Domino (.dms) or Standard MIDI (.mid) file"),
    };
    let out = free_path(path, ext);
    let target = File::create(&out).with_context(|| format!("writing {}", out.display()))?;
    let result = match kind {
        Kind::Domino => dmsmid::dms_to_mid(&data, module, target).map(|r| r.1),
        Kind::Midi => {
            let choose = |s: &Vel0Summary| match vel0 {
                Some(v) => v,
                None if s.ambiguous() && std::io::stdin().is_terminal() => {
                    let asked = Instant::now();
                    let v = ask_vel0(path, s);
                    waited.set(asked.elapsed());
                    v
                }
                None => Vel0::NoteOff,
            };
            dmsmid::mid_to_dms(&data, module, BufWriter::with_capacity(1 << 20, target), choose).map(|r| r.1)
        }
    };
    let report = match result {
        Ok(r) => r,
        Err(e) => {
            let _ = std::fs::remove_file(&out);
            return Err(e);
        }
    };
    println!(
        "{} -> {}\n  {} track(s), {} note(s), module '{}', {:.2} s",
        path.display(),
        out.display(),
        report.tracks,
        report.notes,
        module.name,
        (started.elapsed() - waited.get()).as_secs_f64()
    );
    for (msg, n) in &report.warnings {
        println!("  note: {msg} x{n}");
    }
    Ok(())
}

/// `song.mid` → `song.dms`, or `song (2).dms`, `song (3).dms`, ... if taken.
fn free_path(src: &Path, ext: &str) -> PathBuf {
    let stem = src.file_stem().unwrap_or_default().to_string_lossy();
    let dir = src.parent().unwrap_or(Path::new("."));
    let mut p = dir.join(format!("{stem}.{ext}"));
    let mut n = 2;
    while p.exists() {
        p = dir.join(format!("{stem} ({n}).{ext}"));
        n += 1;
    }
    p
}

fn parse_vel0(s: &str) -> Option<Vel0> {
    match s.trim().to_ascii_lowercase().as_str() {
        "" | "1" | "note-off" | "noteoff" | "off" => Some(Vel0::NoteOff),
        "2" | "silent" | "silent-note" | "note" => Some(Vel0::SilentNote),
        "3" | "auto" => Some(Vel0::Auto),
        _ => None,
    }
}

/// Ask on the console how to read velocity-0 note-ons; Enter keeps the default.
fn ask_vel0(path: &Path, s: &Vel0Summary) -> Vel0 {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    println!(
        "
{name} has {} velocity-0 note-on(s), and the two ways of reading them disagree:",
        thousands(s.vel0_note_ons)
    );
    println!(
        "  1) note-off     (MIDI standard, what Domino does)   {:>14} note(s) left unpaired",
        thousands(s.unpaired_as_note_off)
    );
    println!(
        "  2) silent note  (ends at its own later note-off)    {:>14} note(s) left unpaired",
        thousands(s.unpaired_as_silent_note)
    );
    println!(
        "  3) auto         (best fit per track: {} track(s) read as silent notes)",
        s.tracks_better_as_silent
    );
    loop {
        print!("How should they be read? [1]: ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).unwrap_or(0) == 0 {
            return Vel0::NoteOff; // no input available
        }
        match parse_vel0(&line) {
            Some(v) => return v,
            None => println!("Please type 1, 2 or 3."),
        }
    }
}

fn thousands(n: usize) -> String {
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
