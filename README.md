# dmsmid

Converts Domino `.dms` projects to Standard MIDI Files and back.

Run `dmsmid.exe` and pick one or more files in the dialog. Each file is identified by its contents: `.dms`/`.dmp` becomes `.mid`, and `.mid` becomes `.dms`. The result is saved next to the original; existing files are never overwritten (you get `song (2).dms` and so on). You can also drag files onto the exe or pass them as arguments:

```
dmsmid song.mid other.dms
dmsmid --module "C:\...\Domino64Bit\Module\ok_XG2k.xml" song.mid
```

## Behaviour (matches Domino's own import/export; see tests/domino.rs)

- **MIDI → .dms** is byte-identical to Domino's own import of the same file. Tempo, time and key signatures go to the conductor track. A MIDI track that uses several channels (for example in format-0 files) is split into one Domino track per channel. Notes longer than 65535 ticks are shortened, as in Domino.
- **.dms → MIDI** is event-identical to Domino's own export, except that key signatures stay at their real positions. Domino moves them to bar starts and keeps only the last one in each bar.
- **Controllers** are Domino CCM macros, defined by a module XML. The controller behaviour of Domino's GM Level1 module is built in. Use `--module` with an XML from Domino's `Module` folder if your project uses a different module, because it decides what macro IDs ≥ 128 mean, and which values are offset (pan, pitch bend, and so on).
- **Not carried over:**
  - **.dms → MIDI:** Domino-only events (Measure Link, Variables, Chord, Scale) and system exclusive.
  - **MIDI → .dms:** meta events Domino has no place for (sequence number, SMPTE offset, instrument/program/device name, sequencer-specific), sysex, and poly aftertouch if the module has no `@PKP` macro.
  - Each conversion prints what it dropped.

## Performance

Both directions stream with near-constant memory, independent of file size:

- `.dms` → MIDI: the input is memory-mapped and inflated on a background thread into a few recycled 1 MB blocks. Each event is written as soon as it is decoded; only notes waiting for their note-off are held.
- MIDI → `.dms`: the input is memory-mapped. Pass 1 sizes every track on all cores (Domino chunks need their size up front). Pass 2 streams the output into a pigz-style parallel compressor: 1 MB blocks, each primed with the previous 32 KB, written in order.

25.6 M notes (230 MB MIDI, 12 threads): MIDI → `.dms` 2.3 s, `.dms` → MIDI 1.4 s, both under 25 MB of private memory. `cargo run --release --example gen_big -- out.mid TRACKS NOTES` makes test files.

## Build

```
cargo build --release      # target/release/dmsmid.exe
cargo test
```

The file format is documented in [DMS_SPEC.md](DMS_SPEC.md).

## Credits

Domino is a MIDI sequencer by Takabo Soft. This project is not affiliated with Takabo Soft. It contains no Domino files: the format was worked out from files saved by Domino, and the built-in GM controller table is written from scratch.
