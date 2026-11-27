//! Version 1.1 root signature payloads (RTS0): read them, write them in the
//! layout vkd3d-proton's `D3D12SerializeVersionedRootSignature` produces, and
//! attach one to a root-less program like `shader_cache::with_root`.

use tuxbox_dxbc::Dxbc;

/// `D3D12_DESCRIPTOR_RANGE_FLAG_DESCRIPTORS_VOLATILE | DATA_VOLATILE`.
pub const VOLATILE: u32 = 3;
const VERSION_1_1: u32 = 2;
/// `shader_cache::MAX_BYTES`.
const MAX_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub enum Param {
    /// A descriptor table. Each range is `[type, count, register, space, flags,
    /// offset in table]`.
    Table { visibility: u32, ranges: Vec<[u32; 6]> },
    /// Root constants (kind 1: register, space, count) or a root descriptor
    /// (kind 2 CBV, 3 SRV, 4 UAV: register, space, flags).
    Words { kind: u32, visibility: u32, words: [u32; 3] },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Root {
    pub flags: u32,
    pub params: Vec<Param>,
    /// Static samplers, 13 words each.
    pub samplers: Vec<[u32; 13]>,
}

/// The RTS0 payload of a serialized root (a DXBC container, as captured), or
/// `data` itself when it is already a payload.
pub fn payload(data: &[u8]) -> Result<&[u8], String> {
    if data.get(..4) != Some(b"DXBC") {
        return Ok(data);
    }
    let dxbc = Dxbc::parse(data)?;
    let chunk = dxbc.chunk(b"RTS0").ok_or("the container has no RTS0 part")?;
    data.get(chunk.offset..chunk.offset + chunk.size).ok_or_else(|| "RTS0 part out of bounds".into())
}

fn word(data: &[u8], at: usize) -> Result<u32, String> {
    data.get(at..at + 4)
        .map(|w| u32::from_le_bytes(w.try_into().unwrap()))
        .ok_or_else(|| format!("RTS0 truncated at byte {at}"))
}

pub fn read(data: &[u8]) -> Result<Root, String> {
    let version = word(data, 0)?;
    if version != VERSION_1_1 {
        return Err(format!("root version {version} (only 1.1 is rebuilt)"));
    }
    let (count, param_offset) = (word(data, 4)? as usize, word(data, 8)? as usize);
    let (sampler_count, sampler_offset) = (word(data, 12)? as usize, word(data, 16)? as usize);
    let mut params = Vec::with_capacity(count.min(64));
    for i in 0..count {
        let at = param_offset + 12 * i;
        let (kind, visibility, offset) = (word(data, at)?, word(data, at + 4)?, word(data, at + 8)? as usize);
        params.push(if kind == 0 {
            let (n, ranges_at) = (word(data, offset)? as usize, word(data, offset + 4)? as usize);
            let ranges = (0..n)
                .map(|r| {
                    let mut range = [0; 6];
                    for (k, slot) in range.iter_mut().enumerate() {
                        *slot = word(data, ranges_at + 24 * r + 4 * k)?;
                    }
                    Ok(range)
                })
                .collect::<Result<_, String>>()?;
            Param::Table { visibility, ranges }
        } else {
            let words = [word(data, offset)?, word(data, offset + 4)?, word(data, offset + 8)?];
            Param::Words { kind, visibility, words }
        });
    }
    let samplers = (0..sampler_count)
        .map(|s| {
            let mut sampler = [0; 13];
            for (k, slot) in sampler.iter_mut().enumerate() {
                *slot = word(data, sampler_offset + 52 * s + 4 * k)?;
            }
            Ok(sampler)
        })
        .collect::<Result<_, String>>()?;
    Ok(Root { flags: word(data, 20)?, params, samplers })
}

/// Header, parameter headers, each parameter's data in order (a table's ranges
/// right after its own header), then the static samplers. A rebuilt root then
/// has the bytes the provider serializes, so its variant digest matches.
pub fn write(root: &Root) -> Vec<u8> {
    fn put(out: &mut Vec<u8>, w: u32) {
        out.extend_from_slice(&w.to_le_bytes());
    }
    let mut out = Vec::new();
    for w in [VERSION_1_1, root.params.len() as u32, 24, root.samplers.len() as u32, 0, root.flags] {
        put(&mut out, w);
    }
    for param in &root.params {
        let (kind, visibility) = match param {
            Param::Table { visibility, .. } => (0, *visibility),
            Param::Words { kind, visibility, .. } => (*kind, *visibility),
        };
        for w in [kind, visibility, 0] {
            put(&mut out, w);
        }
    }
    for (i, param) in root.params.iter().enumerate() {
        let at = out.len() as u32;
        out[24 + 12 * i + 8..24 + 12 * i + 12].copy_from_slice(&at.to_le_bytes());
        match param {
            Param::Table { ranges, .. } => {
                put(&mut out, ranges.len() as u32);
                put(&mut out, at + 8);
                ranges.iter().flatten().for_each(|&w| put(&mut out, w));
            }
            Param::Words { words, .. } => words.iter().for_each(|&w| put(&mut out, w)),
        }
    }
    let at = out.len() as u32;
    out[16..20].copy_from_slice(&at.to_le_bytes());
    root.samplers.iter().flatten().for_each(|&w| put(&mut out, w));
    out
}

/// The translator request for a program without an embedded root: the
/// program's container with `root` appended as an RTS0 part.
pub fn with_root(shader: &[u8], root: &[u8]) -> Result<Vec<u8>, String> {
    let dxbc = Dxbc::parse(shader)?;
    if dxbc.chunk(b"RTS0").is_some() {
        return Err("the program embeds a root; the provider replaces it instead (not rebuilt)".into());
    }
    let count = dxbc.chunks.len();
    let end = 32 + count * 4;
    let root_offset = (shader.len() + 4).next_multiple_of(4);
    let size = root_offset + 8 + root.len();
    if size > MAX_BYTES || root.is_empty() {
        return Err("request too large or root empty".into());
    }
    let mut input = Vec::with_capacity(size);
    input.extend_from_slice(&shader[..24]);
    input[4..20].fill(0);
    input.extend_from_slice(&(size as u32).to_le_bytes());
    input.extend_from_slice(&((count + 1) as u32).to_le_bytes());
    for part in &dxbc.chunks {
        input.extend_from_slice(&((part.offset - 8 + 4) as u32).to_le_bytes());
    }
    input.extend_from_slice(&(root_offset as u32).to_le_bytes());
    input.extend_from_slice(&shader[end..]);
    input.resize(root_offset, 0);
    input.extend_from_slice(b"RTS0");
    input.extend_from_slice(&(root.len() as u32).to_le_bytes());
    input.extend_from_slice(root);
    Ok(input)
}

const RANGE_TYPES: [&str; 4] = ["SRV", "UAV", "CBV", "Sampler"];
const RANGE_LETTERS: [char; 4] = ['t', 'u', 'b', 's'];

/// Each table as runs of ranges of one type and space, with slot spans and
/// registers in slot order; one line per parameter otherwise.
pub fn describe(root: &Root) -> String {
    let mut out = format!(
        "version 1.1, flags {:#x}, {} parameters, {} static samplers{}\n",
        root.flags,
        root.params.len(),
        root.samplers.len(),
        match (root.samplers.iter().map(|s| s[10]).min(), root.samplers.iter().map(|s| s[10]).max()) {
            (Some(lo), Some(hi)) => {
                let mut spaces: Vec<_> = root.samplers.iter().map(|s| s[11]).collect();
                spaces.sort_unstable();
                spaces.dedup();
                format!(" (s{lo}-s{hi}, spaces {spaces:?})")
            }
            _ => String::new(),
        }
    );
    for (p, param) in root.params.iter().enumerate() {
        match param {
            Param::Words { kind: 1, words: [reg, space, count], .. } => {
                out += &format!("  param {p}: {count} root constants b{reg}, space{space}\n");
            }
            Param::Words { kind, words: [reg, space, _], .. } => {
                let (name, letter) = match kind { 2 => ("CBV", 'b'), 3 => ("SRV", 't'), 4 => ("UAV", 'u'), _ => ("?", '?') };
                out += &format!("  param {p}: root {name} {letter}{reg}, space{space}\n");
            }
            Param::Table { ranges, .. } => {
                let mut runs: Vec<(u32, u32, u32, u32, u32, Vec<u32>)> = Vec::new();
                let mut end = 0u32;
                for &[ty, count, reg, space, _, offset] in ranges {
                    let start = if offset == u32::MAX { end } else { offset };
                    end = start.saturating_add(if count == u32::MAX { 1 } else { count });
                    match runs.last_mut() {
                        Some(run) if (run.0, run.1, run.2) == (ty, space, count) && run.4 == start => {
                            run.4 = end;
                            run.5.push(reg);
                        }
                        _ => runs.push((ty, space, count, start, end, vec![reg])),
                    }
                }
                out += &format!("  param {p}: table, {end} slots\n");
                for (ty, space, count, start, stop, regs) in runs {
                    let letter = RANGE_LETTERS.get(ty as usize).copied().unwrap_or('?');
                    let each = match count { 1 => String::new(), u32::MAX => " (unbounded)".into(), n => format!(" ({n} each)") };
                    let regs: Vec<_> = regs.iter().map(|r| format!("{letter}{r}")).collect();
                    out += &format!(
                        "    slots {start}-{}: {}{each}, space{space}: {}\n",
                        stop - 1,
                        RANGE_TYPES.get(ty as usize).unwrap_or(&"?"),
                        regs.join(",")
                    );
                }
            }
        }
    }
    out
}

#[cfg(test)]
#[path = "tests/root.rs"]
mod tests;
