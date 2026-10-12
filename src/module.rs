//! Domino module definitions (`Domino\Module\*.xml`): the Control Change Macro
//! (CCM) list that gives CONTROL events (2003) their meaning.
//!
//! The XML is Shift_JIS, but every tag, attribute and `<Data>` token we need is
//! ASCII, and Shift_JIS trail bytes never equal `<`, `>` or `"`, so the file is
//! scanned as bytes. `<Data>` templates are compiled once, at load.

use anyhow::{bail, Result};

/// Built-in default: the controller behaviour of Domino's "GM Level1" module,
/// written out as a minimal definition (IDs, offsets and output templates only).
/// Plain CCs without an offset need no entry: undefined IDs 0–127 are sent as-is.
pub const BUILTIN_GM: &[u8] = br#"<ModuleData Name="GM Level1">
<ControlChangeMacroList>
  <CCM ID="10"><Value Offset="64" /><Data>@CC 10 #VL</Data></CCM>
  <CCM ID="121"><Data>@CC 121 0</Data></CCM>
  <CCM ID="123"><Data>@CC 123 0</Data></CCM>
  <CCM ID="128"></CCM>
  <CCM ID="130"><Value Offset="8192" /><Data>@PB #VH #VL</Data></CCM>
  <CCM ID="132"><Value /><Data>@CP #VL</Data></CCM>
  <CCM ID="140"><Value /><Data>@RPN 00H 00H #VL #NONE</Data></CCM>
  <CCM ID="141"><Value Offset="8192" /><Data>@RPN 00H 01H #VH #VL</Data></CCM>
  <CCM ID="142"><Value Offset="64" /><Data>@RPN 00H 02H #VL #NONE</Data></CCM>
  <CCM ID="200"><Data>@SYSEX F0H 7EH 7FH 09H 01H F7H</Data></CCM>
</ControlChangeMacroList>
</ModuleData>
"#;

pub const ID_PITCH_BEND: u16 = 130;
pub const ID_CHANNEL_PRESSURE: u16 = 132;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Operand {
    VL,
    VH,
    GL,
    GH,
    None,
    Lit(u8),
}

#[derive(Clone, Debug, PartialEq)]
enum Op {
    Cc(Operand, Operand),
    PitchBend { hi: Operand, lo: Operand },
    ChannelPressure(Operand),
    PolyPressure(Operand, Operand),
    /// RPN (`nrpn = false`) or NRPN: parameter MSB, LSB, data MSB, optional data LSB.
    Param { nrpn: bool, ops: [Operand; 4] },
    Sysex,
}

#[derive(Clone, Debug)]
pub struct Ccm {
    pub id: u16,
    pub value_offset: i32,
    pub gate_offset: i32,
    /// Compiled `<Data>`, or why it could not be compiled.
    ops: Result<Vec<Op>, String>,
    /// First command of the template (`@CC`, `@PKP`, …), for lookups.
    first: Option<(String, Option<u8>)>,
}

pub struct Module {
    pub name: String,
    ccms: Vec<Ccm>,
    /// CCM ID → index + 1 into `ccms` (0 = not defined).
    index: Vec<u32>,
    /// Value offset per MIDI CC number (from a CCM with the same ID that sends `@CC n`).
    cc_offsets: [i32; 128],
    /// Index of the CCM that sends polyphonic key pressure (lowest ID), if any.
    pkp: Option<usize>,
}

/// Result of expanding a CCM into MIDI.
pub enum Expansion {
    Done,
    /// The template sends system exclusive, which this tool does not write.
    Sysex,
    Error(String),
}

impl Module {
    pub fn builtin() -> Self {
        Self::parse(BUILTIN_GM).expect("built-in module definition parses")
    }

    pub fn parse(xml: &[u8]) -> Result<Self> {
        let xml = strip_comments(xml);
        let name = find(&xml, b"<ModuleData")
            .and_then(|i| attr(start_tag(&xml[i..]), "Name"))
            .unwrap_or_else(|| "unnamed".into());
        let mut ccms: Vec<Ccm> = Vec::new();
        let mut index = vec![0u32; 1 << 16];
        let mut pos = 0;
        while let Some(rel) = find(&xml[pos..], b"<CCM") {
            let start = pos + rel;
            let tag = start_tag(&xml[start..]);
            let element = if tag.ends_with(b"/>") {
                tag
            } else {
                let end = find(&xml[start..], b"</CCM>").map_or(xml.len(), |e| start + e);
                &xml[start..end]
            };
            pos = start + element.len().max(4);
            let Some(id) = attr(tag, "ID").and_then(|s| s.trim().parse::<u16>().ok()) else {
                continue;
            };
            if index[id as usize] != 0 {
                continue; // first definition wins
            }
            let offset_of = |child: &[u8]| {
                child_tag(element, child)
                    .and_then(|t| attr(t, "Offset"))
                    .and_then(|s| s.trim().parse().ok())
                    .unwrap_or(0)
            };
            let tokens: Vec<String> = find(element, b"<Data>")
                .map(|i| {
                    let body = &element[i + 6..];
                    let end = find(body, b"</Data>").unwrap_or(body.len());
                    String::from_utf8_lossy(&body[..end]).split_whitespace().map(str::to_owned).collect()
                })
                .unwrap_or_default();
            let first = tokens.first().map(|c| (c.clone(), tokens.get(1).and_then(|t| literal(t))));
            ccms.push(Ccm {
                id,
                value_offset: offset_of(b"<Value"),
                gate_offset: offset_of(b"<Gate"),
                ops: compile(&tokens).map_err(|e| format!("CCM {id}: {e}")),
                first,
            });
            index[id as usize] = ccms.len() as u32;
        }
        if ccms.is_empty() {
            bail!("no <CCM> entries found; is this a Domino module definition?");
        }
        let mut cc_offsets = [0i32; 128];
        for (cc, off) in cc_offsets.iter_mut().enumerate() {
            if let Some(&i) = index.get(cc).filter(|&&i| i != 0) {
                let m = &ccms[i as usize - 1];
                if m.first.as_ref().is_some_and(|f| f.0 == "@CC" && f.1 == Some(cc as u8)) {
                    *off = m.value_offset;
                }
            }
        }
        let pkp = (0..ccms.len())
            .filter(|&i| ccms[i].first.as_ref().is_some_and(|f| f.0 == "@PKP"))
            .min_by_key(|&i| ccms[i].id);
        Ok(Module { name, ccms, index, cc_offsets, pkp })
    }

    #[inline]
    pub fn get(&self, id: u16) -> Option<&Ccm> {
        match self.index[id as usize] {
            0 => None,
            i => Some(&self.ccms[i as usize - 1]),
        }
    }

    /// Value offset for MIDI CC `cc`, taken from the CCM with that ID if it sends `@CC cc`.
    #[inline]
    pub fn cc_offset(&self, cc: u8) -> i32 {
        self.cc_offsets[(cc & 0x7F) as usize]
    }

    pub fn offset(&self, id: u16, default: i32) -> i32 {
        self.get(id).map_or(default, |m| m.value_offset)
    }

    /// The CCM that sends polyphonic key pressure, if the module has one (lowest ID).
    #[inline]
    pub fn poly_pressure(&self) -> Option<&Ccm> {
        self.pkp.map(|i| &self.ccms[i])
    }
}

impl Ccm {
    /// Expand stored (offset-free) `value`/`gate` into MIDI messages on `ch`;
    /// each message is passed to `emit` as (bytes, length).
    #[inline]
    pub fn expand(&self, ch: u8, value: i32, gate: i32, mut emit: impl FnMut([u8; 3], usize)) -> Expansion {
        let ops = match &self.ops {
            Ok(ops) => ops,
            Err(e) => return Expansion::Error(e.clone()),
        };
        let v = (value + self.value_offset).clamp(0, 0x3FFF) as u32;
        let g = (gate + self.gate_offset).clamp(0, 0x3FFF) as u32;
        let get = |o: Operand| -> Option<u8> {
            Some(match o {
                Operand::VL => (v & 0x7F) as u8,
                Operand::VH => ((v >> 7) & 0x7F) as u8,
                Operand::GL => (g & 0x7F) as u8,
                Operand::GH => ((g >> 7) & 0x7F) as u8,
                Operand::None => return None,
                Operand::Lit(x) => x,
            })
        };
        // Operands were checked at compile time; `#NONE` only appears where optional.
        let req = |o: Operand| get(o).unwrap_or(0);
        if ops.iter().any(|o| *o == Op::Sysex) {
            return Expansion::Sysex;
        }
        for op in ops {
            match *op {
                Op::Cc(n, x) => emit([0xB0 | ch, req(n), req(x)], 3),
                Op::PitchBend { hi, lo } => emit([0xE0 | ch, req(lo), req(hi)], 3),
                Op::ChannelPressure(x) => emit([0xD0 | ch, req(x), 0], 2),
                Op::PolyPressure(k, x) => emit([0xA0 | ch, req(k), req(x)], 3),
                Op::Param { nrpn, ops: [m, l, dm, dl] } => {
                    let (pm, pl) = if nrpn { (99, 98) } else { (101, 100) };
                    emit([0xB0 | ch, pm, req(m)], 3);
                    emit([0xB0 | ch, pl, req(l)], 3);
                    emit([0xB0 | ch, 6, req(dm)], 3);
                    if let Some(x) = get(dl) {
                        emit([0xB0 | ch, 38, x], 3);
                    }
                }
                Op::Sysex => {}
            }
        }
        Expansion::Done
    }
}

fn compile(tokens: &[String]) -> Result<Vec<Op>, String> {
    let mut ops = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let cmd = tokens[i].as_str();
        let mut j = i + 1;
        while j < tokens.len() && !tokens[j].starts_with('@') {
            j += 1;
        }
        let args = &tokens[i + 1..j];
        i = j;
        if cmd == "@SYSEX" {
            ops.push(Op::Sysex);
            continue;
        }
        let parsed: Vec<Operand> = args.iter().map(|t| operand(t)).collect::<Result<_, _>>()?;
        let need = |n: usize| -> Result<Operand, String> {
            match parsed.get(n) {
                Some(Operand::None) | None => Err(format!("'{cmd}' is missing operand {}", n + 1)),
                Some(o) => Ok(*o),
            }
        };
        ops.push(match cmd {
            "@CC" => Op::Cc(need(0)?, need(1)?),
            "@PB" => Op::PitchBend { hi: need(0)?, lo: need(1)? },
            "@CP" => Op::ChannelPressure(need(0)?),
            "@PKP" => Op::PolyPressure(need(0)?, need(1)?),
            "@RPN" | "@NRPN" => Op::Param {
                nrpn: cmd == "@NRPN",
                ops: [need(0)?, need(1)?, need(2)?, parsed.get(3).copied().unwrap_or(Operand::None)],
            },
            other => return Err(format!("unsupported command '{other}'")),
        });
    }
    Ok(ops)
}

fn operand(tok: &str) -> Result<Operand, String> {
    Ok(match tok {
        "#VL" => Operand::VL,
        "#VH" => Operand::VH,
        "#GL" => Operand::GL,
        "#GH" => Operand::GH,
        "#NONE" => Operand::None,
        _ => Operand::Lit(literal(tok).ok_or_else(|| format!("unsupported operand '{tok}'"))?),
    })
}

/// `40H` (hex) or `24` (decimal).
fn literal(tok: &str) -> Option<u8> {
    match tok.strip_suffix(['H', 'h']) {
        Some(h) => u8::from_str_radix(h, 16).ok(),
        None => tok.parse().ok(),
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// The start tag beginning at `s[0]`, up to and including `>`.
fn start_tag(s: &[u8]) -> &[u8] {
    let mut quoted = false;
    for (i, &b) in s.iter().enumerate() {
        match b {
            b'"' => quoted = !quoted,
            b'>' if !quoted => return &s[..=i],
            _ => {}
        }
    }
    s
}

/// Start tag of the first child element named `name` (e.g. `<Value`) inside `element`.
fn child_tag<'a>(element: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    let mut pos = 0;
    while let Some(rel) = find(&element[pos..], name) {
        let i = pos + rel;
        if matches!(element.get(i + name.len()), Some(b' ' | b'>' | b'/' | b'\t' | b'\r' | b'\n')) {
            return Some(start_tag(&element[i..]));
        }
        pos = i + name.len();
    }
    None
}

fn attr(tag: &[u8], name: &str) -> Option<String> {
    let pat = format!(" {name}=\"");
    let i = find(tag, pat.as_bytes())? + pat.len();
    let end = tag[i..].iter().position(|&b| b == b'"')?;
    Some(String::from_utf8_lossy(&tag[i..i + end]).into_owned())
}

fn strip_comments(xml: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(xml.len());
    let mut i = 0;
    while let Some(rel) = find(&xml[i..], b"<!--") {
        out.extend_from_slice(&xml[i..i + rel]);
        let after = i + rel + 4;
        i = find(&xml[after..], b"-->").map_or(xml.len(), |e| after + e + 3);
    }
    out.extend_from_slice(&xml[i..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(m: &Module, id: u16, ch: u8, v: i32) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        assert!(matches!(m.get(id).unwrap().expand(ch, v, 0, |b, n| out.push(b[..n].to_vec())), Expansion::Done));
        out
    }

    #[test]
    fn builtin_gm() {
        let m = Module::builtin();
        assert_eq!(m.name, "GM Level1");
        assert_eq!(m.cc_offset(10), 64);
        assert_eq!(m.cc_offset(7), 0);
        assert_eq!(m.offset(ID_PITCH_BEND, 0), 8192);
        assert!(m.poly_pressure().is_none());
        assert_eq!(run(&m, 140, 2, 12), vec![vec![0xB2, 101, 0], vec![0xB2, 100, 0], vec![0xB2, 6, 12]]);
        assert_eq!(run(&m, 130, 3, -8189), vec![vec![0xE3, 0x03, 0x00]]);
        assert_eq!(run(&m, 132, 4, 33), vec![vec![0xD4, 33]]);
        assert!(matches!(m.get(200).unwrap().expand(0, 0, 0, |_, _| ()), Expansion::Sysex));
    }
}
