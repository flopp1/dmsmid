# Domino Sequence File Format (`.dms`)

| | |
|---|---|
| Applies to | Domino (Takabo Soft) MIDI sequencer: `.dms` projects, `.dmp` parts, the `MidiPortalSequence` clipboard format |
| Status | Reverse-engineered, unofficial. Verified against files written by Domino 64-bit |
| Byte order | Little-endian throughout |

## Contents

1. [Conventions](#1-conventions)
2. [File envelope](#2-file-envelope)
3. [Chunk grammar](#3-chunk-grammar)
4. [Root chunks](#4-root-chunks)
5. [Track chunk](#5-track-chunk)
6. [Event chunks](#6-event-chunks)
7. [Control events and module definitions](#7-control-events-and-module-definitions)
8. [Clipboard data](#8-clipboard-data)
9. [Mapping to Standard MIDI Files](#9-mapping-to-standard-midi-files)
10. [Open questions](#10-open-questions)

---

## 1. Conventions

**Types**

| Type | Encoding |
|---|---|
| `u8`, `u16`, `u32` | Unsigned integer, little-endian |
| `s8`, `s16`, `s32` | Two's-complement signed integer, little-endian |
| `bytes` | Raw bytes filling the whole chunk body. No length prefix, no terminator, no transcoding. Domino displays text as CP932 (Shift-JIS) but stores whatever bytes it was given. |
| `value` | A value list (§3.2): one or more chunks of id 0 holding an `f32` |
| `container` | A chunk whose body is itself a chunk sequence |

**Tick** = MIDI tick, using the PPQ in root chunk 1002. Positions are absolute ticks from the start of the song (from the start of the copied range for clipboard data, §8).

**Status of statements.** Everything in this document was verified against files saved by Domino, except where it is marked *(inferred)* or *(unknown)*.

**Readers** should decode every field by its actual chunk size, not by its documented type, and should preserve chunks they do not understand. **Writers** should produce chunks in the order given here; this is the order Domino writes.

---

## 2. File envelope

| Offset | Size | Field |
|---|---|---|
| 0 | n | Magic, ASCII, not terminated (see below) |
| n | 4 | `u32` length of the uncompressed chunk data |
| n + 4 | … | zlib stream (RFC 1950) of the chunk data (§3) |

| Magic | n | Used by |
|---|---|---|
| `PortalSequenceData` | 18 | `.dms` files and the clipboard |
| `PortalPartData` | 14 | `.dmp` part files |

Bytes after the end of the zlib stream are ignored. Clipboard memory blocks are often larger than their contents. Any zlib compression level and block structure is accepted.

---

## 3. Chunk grammar

### 3.1 Chunks

The uncompressed data is a sequence of chunks, running to the end of the data:

```
chunk = id:u16  size:u32  body:u8[size]       // size excludes the 6-byte header
```

**IDs are scoped by their parent.** The same ID means different things at the root, inside a track and inside an event: for example `1001` is *copyright* at the root, *channel* in a track and *position* in an event. Interpret chunks by path, never by ID alone.

### 3.2 Value lists

Numeric fields of control and tempo events are *value lists*: a chunk sequence whose entries are chunks of id `0` with a 4-byte `f32` body.

```
00 00  04 00 00 00  <f32>
```

Every file seen so far has exactly one entry. Domino's gradual-change and line/curve tools write ordinary single-entry events.

---

## 4. Root chunks

Written in this order:

| ID | Type | Meaning | Default |
|---|---|---|---|
| 1000 | bytes | Song title | empty |
| 1001 | bytes | Copyright | empty |
| 1002 | u16 | **PPQ** (ticks per quarter note) | 480 |
| 1007 | u32 | *(unknown)* | 48 |
| 1009 | u32 | *(unknown)* | 0 |
| 1012 | 8 bytes | *(unknown)* | all 0 |
| 1013 | u32 | *(unknown; probably editor state: changes when the song is edited)* | 0 |
| 1014 | u32 | *(unknown)* | 0 |
| 1019 | bytes | *(unknown)* | empty |
| 1020 | u32 | *(unknown)* | 16 |
| 1021 | u8 | *(unknown)* | 1 |
| 1022 | u8 | *(unknown)* | 1 |
| 1023 | u32 | *(unknown)* | 17 |
| 1024 | u8 | *(unknown)* | 1 |
| 1003 | container | **Track** (§5), repeated once per track | |
| 1006 | bytes | *(unknown)* | empty |
| 1008 | container | *(unknown)*: `1000` bytes empty, `1001` u32 1, `1002` u32 1 | |
| 1017 | container | *(unknown)*: `100` u8 0, `101` u8 0, `102` u8 0, `103` u32 100, `104` u8 0, `105` u32 100, `106` 12 × `05` | |

The first track of a project is the **conductor track** (track field 1003 = 1). It holds the tempo map (tempo, time and key signatures) and song-level text.

The module definition (§7) used to interpret control events is **not** stored in the file; it is an application setting in Domino.

---

## 5. Track chunk

Root chunk `1003` is a container. Its children are written in this order: settings, then events in ascending position, then the end-of-track chunk, then the trailing settings.

| ID | Type | Meaning | Default |
|---|---|---|---|
| 1000 | u16 | Output port (0 = A, 1 = B, …) | 0 |
| 1001 | u8 | MIDI channel, 0-based. **One channel per track.** | 0 |
| 1002 | bytes | Track name | empty |
| 1003 | u8 | Conductor track flag (1 = conductor) | 0 |
| 1004 | u8 | Rhythm (drum) track flag | 0; 1 for channel 10 |
| 1008 | u8 | *(unknown)* | 60 |
| 1009 | bytes | Rhythm set name | `General MIDI Drum` |
| 1011 | u16 | *(unknown)* | 0 |
| 1012 | u32 | Tick adjust | 0 |
| 1016 | u32 | Gate adjust (%) | 100 |
| 1017 | s32 | Transpose | 0 |
| 1013 | u8 | *(unknown)* | 1 |
| 1014 | u16 | *(unknown)* | 1 |
| 1015 | u8 | Enabled | 1 |
| 1018 | u8 | Colour (255 = none) | 255 |
| 1019 | u32 | Tick adjust measure | 1 |
| 1020 | 16 bytes | *(unknown)* | all 0 |
| 1021 | u8 | Key range low | 0 |
| 1022 | u8 | Key range high | 127 |
| 2001… | container | **Events** (§6), sorted by position | |
| 2009 | container | **End of track**: `1001` u32 = position of the end of the track | |
| 1005 | u32 | *(unknown)* | 50 |
| 1006 | u8 | Volume | 100 |
| 1007 | u32 | Default gate | equals PPQ *(inferred)* |
| 1010 | container | *(unknown)*: `1000` u8 0, `1001` u8 0 | |

---

## 6. Event chunks

Every event is a container whose first child is `1001` u32 **position**. Field order within each event is as listed.

### 6.1 Summary

| ID | Event | SMF equivalent |
|---|---|---|
| 2001 | Note | Note-on and note-off pair |
| 2002 | Program change | `Cn` |
| 2003 | Control (§7) | CC, pitch bend, pressure, RPN/NRPN, sysex macros |
| 2004 | System exclusive | `F0` |
| 2005 | Comment | `FF 01` text |
| 2007 | Variables | none |
| 2008 | Tempo | `FF 51` |
| 2011 | Lyric | `FF 05` |
| 2012 | Cue point | `FF 07` |
| 2014 | Measure link | none |
| 2015 | Time signature | `FF 58` |
| 2016 | Key signature | `FF 59` |
| 2017 | Marker | `FF 06` |
| 2018 | Scale | none |
| 2019 | Chord | none |
| 2006, 2010, 2013 | *(unknown; never seen)* | |

### 6.2 Note (2001)

| ID | Type | Field |
|---|---|---|
| 1001 | u32 | Position |
| 2001 | u8 | Key (0–127) |
| 2002 | u8 | Velocity (1–127) |
| 2003 | u32 | Gate (length in ticks). Domino limits this to 65535. |

A note is always 40 bytes including its header.

### 6.3 Program change (2002)

| ID | Type | Field |
|---|---|---|
| 1001 | u32 | Position |
| 2003 | u8 | Program, **1-based** (1–128 = MIDI program 0–127) |
| 2004 | bytes | *(unknown)*, empty |
| 2005 | u8 | *(unknown)*, 0 |
| 2001 | s8 | Bank MSB, −1 = none *(inferred)* |
| 2002 | s8 | Bank LSB, −1 = none *(inferred)* |
| 2006 | s16 | *(unknown)*, −1 |

Bank select messages read from a MIDI file are stored as separate control events (CC 0/32), not in fields 2001 and 2002.

### 6.4 Control (2003)

| ID | Type | Field |
|---|---|---|
| 1001 | u32 | Position |
| 2001 | u16 | Control Change Macro (CCM) ID (§7) |
| 2002 | value | CCM *gate* parameter (0 when the CCM has none) |
| 2003 | value | CCM *value* parameter |

### 6.5 Tempo (2008)

| ID | Type | Field |
|---|---|---|
| 1001 | u32 | Position |
| 2001 | value | Tempo in BPM, for example 120.0 |

### 6.6 Time signature (2015)

| ID | Type | Field |
|---|---|---|
| 1001 | u32 | Position, at the start of a measure |
| 2001 | u8 | Numerator |
| 2002 | u8 | Denominator, literal (2, 4, 8, 16, …) |

### 6.7 Key signature (2016)

| ID | Type | Field |
|---|---|---|
| 1001 | u32 | Position |
| 2001 | u8 | Key code |

```
key code = (sharps ? n : 8 + n) + (minor ? 16 : 0)      // n = number of sharps or flats, 0–7
```

| Code | Key | | Code | Key |
|---|---|---|---|---|
| 0 | C major | | 16 | A minor |
| 1–7 | G … C# major | | 17–23 | E … A# minor |
| 9–15 | F … Cb major | | 25–31 | D … Ab minor |

### 6.8 Text events (2005, 2011, 2012, 2017)

| ID | Type | Field |
|---|---|---|
| 1001 | u32 | Position |
| 2001 | bytes | Text |

### 6.9 Domino-only events

These have no Standard MIDI File equivalent.

| Event | Fields after position |
|---|---|
| 2007 Variables | `2001` bytes name, `2002` bytes value |
| 2014 Measure link | `2001` u32 destination measure, `2002` s32 key transpose |
| 2018 Scale | `2001` u8 key, `2002` u32 scale type (100 = Natural Major) |
| 2019 Chord | `2001` u8 root, `2002` u32 chord type (100 = Major) |

### 6.10 System exclusive (2004)

`2001` bytes comment, `2002` bytes message *(from the chunk reference; not verified)*.

---

## 7. Control events and module definitions

A control event does not hold a MIDI message. It holds a **CCM ID** plus value and gate parameters. Their meaning comes from the *module definition* selected in Domino's settings: an XML file in `Domino\Module\`, encoded in Shift_JIS.

```xml
<ControlChangeMacroList>
  <Folder Name="...">
    <CCM ID="10" Name="Part Panpot">
      <Value Min="-64" Max="63" Offset="64" />
      <Data>@CC 10 #VL</Data>
    </CCM>
    <CCM ID="129" Name="129 PAT">
      <Value />
      <Gate Default="60" Type="Key" />
      <Data>@PKP #GL #VL</Data>
    </CCM>
  </Folder>
</ControlChangeMacroList>
```

### 7.1 Stored values

```
stored value = MIDI value − Value/@Offset         (Offset defaults to 0)
stored gate  = MIDI value − Gate/@Offset
```

For example, in GMLevel1, pan 64 (centre) is stored as 0, and pitch bend 8192 (centre) is stored as 0.

### 7.2 Common CCM IDs

| ID | Meaning |
|---|---|
| 0–127 | MIDI control change of the same number |
| 128 | Rest (no output) |
| 129 | Polyphonic key pressure (some modules only) |
| 130 | Pitch bend |
| 132 | Channel pressure |
| ≥ 140 | Module-specific macros: RPN, NRPN, system exclusive |

### 7.3 `<Data>` template

`<Data>` is a whitespace-separated list of commands, each followed by its operands.

| Command | Output |
|---|---|
| `@CC n x` | `Bn n x` |
| `@PB hi lo` | `En lo hi` |
| `@CP x` | `Dn x` |
| `@PKP key x` | `An key x` |
| `@RPN msb lsb dm [dl]` | CC 101 msb, CC 100 lsb, CC 6 dm, CC 38 dl |
| `@NRPN msb lsb dm [dl]` | CC 99 msb, CC 98 lsb, CC 6 dm, CC 38 dl |
| `@SYSEX bytes…` | System exclusive. `[ … ]` marks a checksummed region. |

| Operand | Meaning |
|---|---|
| `#VL`, `#VH` | Bits 0–6 / 7–13 of (stored value + Value offset) |
| `#GL`, `#GH` | Bits 0–6 / 7–13 of (stored gate + Gate offset) |
| `#NONE` | Omit this byte (and the message that would carry it) |
| `#VF1`…`#VF4` | 4-bit nibbles of the value (sysex only) |
| `#1RCH`, `#2RCH`, `#4RCH` | Checksum variants (sysex only) *(inferred)* |
| `40H` | Hexadecimal literal |
| `24` | Decimal literal |

---

## 8. Clipboard data

Domino's clipboard format is registered as `MidiPortalSequence`. It uses the same envelope (`PortalSequenceData`) and the same chunk grammar, with these differences:

- Positions are **relative to the start of the copied range**.
- Only events whose position lies inside the range are included; a note starting before the range is not.
- Every copied track is present, including tracks with no events in the range. Each has `2009` = length of the range.
- Track settings are **not** copied: every track carries the defaults from §5.
- Root `1001` holds Domino's default copyright string. Root `1000` is empty.
- On paste, Domino places the first copied track into the highlighted track and the following ones below it, starting at the play cursor. Ticks are not rescaled to the destination PPQ.

---

## 9. Mapping to Standard MIDI Files

### 9.1 Import (SMF → `.dms`), as Domino does it

| SMF | `.dms` |
|---|---|
| Header division | Root 1002 PPQ. SMPTE timing is not supported. |
| Track 0 (no channel messages) | Conductor track. Its name also becomes root 1000. |
| `FF 02` copyright | Root 1001 |
| `FF 03` track name | Track 1002 |
| `FF 21` port prefix | Track 1000 |
| Channel of the track's messages | Track 1001. Channel 10 also sets 1004. |
| Note-on + note-off | 2001. Velocity-0 note-on counts as note-off. Note-off velocity is discarded. Gate is limited to 65535. |
| `Bn` control change | 2003, CCM ID = CC number, value − offset of that CCM |
| `En` pitch bend | 2003, CCM 130, value − 8192 |
| `Dn` channel pressure | 2003, CCM 132 |
| `An` poly key pressure | 2003 using the module's `@PKP` CCM; **dropped** if the module has none |
| `Cn` program change | 2002, program + 1 |
| `FF 51`, `FF 58`, `FF 59` | 2008 (BPM = 60 000 000 / µs), 2015, 2016 |
| `FF 01`, `FF 05`, `FF 06`, `FF 07` | 2005, 2011, 2017, 2012 |
| `FF 2F` end of track | 2009 |
| `FF 00`, `04`, `08`, `09`, `20`, `54`, `7F` | Dropped |

RPN and NRPN sequences are stored as their individual CC events, not merged into macros. Events at the same position keep the order they had in the file.

### 9.2 Export (`.dms` → SMF), as Domino does it

- Format 1, one SMF track per Domino track, same PPQ, no running status.
- Each track starts with `FF 03` name. The first track also gets `FF 02` copyright; every non-conductor track gets `FF 21` port.
- Notes become `9n key vel` and `8n key 00`.
- Control events are expanded through their CCM `<Data>` template (§7.3).
- Tempo: µs per quarter note = `trunc(f32(60 000 000 / bpm))`, computed in single precision.
- Time signature: `FF 58 nn dd 18 08`. The clocks-per-click (24) and 32nds-per-quarter (8) bytes are always written as shown.
- **Key signatures are written once per measure, at the measure start; the last one in the measure wins.**
- Domino-only events (§6.9) are dropped.
- Ordering within one tick: note-offs, then other events in stored order, then time signature, then key signature.
- `FF 2F` is written at the 2009 position.

---

## 10. Open questions

1. The meaning of the undocumented root chunks and track settings marked *(unknown)*. Root 1013 changes when a song is edited.
2. Program change fields 2001, 2002 and 2006 when a bank is chosen in Domino's program-change dialog.
3. Whether a value list (§3.2) can ever hold more than one entry.
4. Event IDs 2006, 2010 and 2013.
5. The layout of `.dmp` part files beyond the envelope.
