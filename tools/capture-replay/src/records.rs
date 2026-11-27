//! The binding evidence of a captured program: FSSM used-resource rows, FSSM
//! user-data rows and PSV0 resource rows, with the root range that covers each
//! descriptor row's table slot. Binding metadata only; no code.

use tuxbox_dxbc::compute_table;
use tuxbox_dxbc::rootsig::{RootParamKind, RootSig};

/// `PSVResourceType` names.
const PSV_TYPES: [&str; 10] = [
    "invalid", "sampler", "cbv", "srv-typed", "srv-raw", "srv-struct",
    "uav-typed", "uav-raw", "uav-struct", "uav-struct-counter",
];

fn program(shader: &[u8]) -> Option<Vec<u8>> {
    let dxbc = tuxbox_dxbc::Dxbc::parse(shader).ok()?;
    let dxil = dxbc.chunk(b"DXIL")?;
    let programs = tuxbox_dxbc::xdat::programs(shader.get(dxil.offset + 24..dxil.offset + dxil.size)?).ok()?;
    let [program] = programs.as_slice() else { return None };
    Some(program.to_vec())
}

/// The range covering `slot` of root parameter `table`: (type, register, space).
fn covering(signature: &RootSig, table: u32, slot: u32) -> Result<Option<(u32, u32, u32)>, &'static str> {
    let Some(RootParamKind::Table(ranges)) = signature.params.get(table as usize).map(|p| &p.kind) else {
        return Err("not a table");
    };
    let mut next = 0u64;
    for r in ranges {
        let start = if r.offset_in_table == u32::MAX { next } else { u64::from(r.offset_in_table) };
        next = start + if r.num == u32::MAX { u64::from(u32::MAX) } else { u64::from(r.num) };
        if (start..next).contains(&u64::from(slot)) {
            return Ok(Some((r.range_type, r.base_register + (u64::from(slot) - start) as u32, r.space)));
        }
    }
    Ok(None)
}

/// One line per row. A descriptor row (class 0x300 SRV, 0x302 CBV, 0x303 UAV)
/// also shows its table slot, the covering root range, the user-data load at
/// the same offset and table, the PSV row with its register, and `mismatch`
/// when the root range has another type than the row.
pub fn report(shader: &[u8], signature: &RootSig) -> Vec<String> {
    let Some(program) = program(shader) else { return vec!["no single XDAT program".into()] };
    let users: Vec<[u32; 5]> = compute_table::user_data_records(&program).into_iter().flatten().collect();
    let psv = tuxbox_dxbc::mesh_table::shader_resources(shader).unwrap_or_default();
    let mut lines = Vec::new();
    for [class, offset, register, shape, location] in compute_table::records(&program).into_iter().flatten() {
        let mut line = format!("fssm class={class:#x} offset={offset} reg={register} shape={shape} loc={location:#x}");
        let kind = match class { 0x300 => Some(0), 0x303 => Some(1), 0x302 => Some(2), _ => None };
        if let Some(kind) = kind {
            let table = location >> 16;
            line += &format!(" table={table} slot={}", offset / 8);
            match covering(signature, table, offset / 8) {
                Ok(Some((ty, reg, space))) => {
                    let name = ["SRV", "UAV", "CBV", "Sampler"].get(ty as usize).unwrap_or(&"?");
                    let letter = ['t', 'u', 'b', 's'].get(ty as usize).unwrap_or(&'?');
                    line += &format!(" root={name}:{letter}{reg},space{space}");
                    if ty != kind {
                        line += " mismatch";
                    }
                }
                Ok(None) => line += " root=uncovered",
                Err(why) => line += &format!(" root=({why})"),
            }
            let loads: Vec<_> = users.iter()
                .filter(|u| u[0] == 3 && u[1] == offset && u[2] == 1 && u[4] >> 8 == table)
                .map(|u| u[3].to_string()).collect();
            line += &format!(" user={}", if loads.is_empty() { "none".into() } else { loads.join(",") });
            // PSV types of the row's class: CBV 2, SRV 3-5, UAV 6-9.
            let types = match kind { 0 => 3..=5, 1 => 6..=9, _ => 2..=2 };
            let named: Vec<_> = psv.iter().filter(|r| types.contains(&r[0]) && r[2] <= register && register <= r[3])
                .map(|r| format!("{}:space{}", PSV_TYPES.get(r[0] as usize).unwrap_or(&"?"), r[1])).collect();
            line += &format!(" psv={}", if named.is_empty() { "none".into() } else { named.join(",") });
        }
        lines.push(line);
    }
    lines.extend(users.iter().map(|[class, offset, width, register, location]| {
        format!("user class={class} offset={offset} width={width} reg={register} loc={location:#x}")
    }));
    lines.extend(psv.iter().map(|[ty, space, low, high]| {
        format!("psv {} space={space} reg={low}..={high}", PSV_TYPES.get(*ty as usize).unwrap_or(&"?"))
    }));
    lines
}

/// HLSL `register(...)` declarations the root does not cover, skipping the
/// translator's private spaces (32765, 32766). This stands in for vkd3d's
/// remap, which needs real bindings.
pub fn uncovered(hlsl: &str, root: &crate::root::Root) -> Vec<String> {
    use crate::root::Param;
    let mut covered: Vec<(char, u32, u32, u64)> = Vec::new();
    for param in &root.params {
        match param {
            Param::Table { ranges, .. } => covered.extend(ranges.iter().map(|&[ty, count, reg, space, _, _]| {
                let letter = ['t', 'u', 'b', 's'].get(ty as usize).copied().unwrap_or('?');
                (letter, space, reg, if count == u32::MAX { 1 << 32 } else { u64::from(count) })
            })),
            Param::Words { kind, words: [reg, space, _], .. } => {
                let letter = match kind { 1 | 2 => 'b', 3 => 't', 4 => 'u', _ => '?' };
                covered.push((letter, *space, *reg, 1));
            }
        }
    }
    covered.extend(root.samplers.iter().map(|s| ('s', s[11], s[10], 1)));
    let mut missing = Vec::new();
    for declaration in hlsl.split("register(").skip(1) {
        let Some(body) = declaration.split(')').next() else { continue };
        let mut parts = body.split(',').map(str::trim);
        let Some(first) = parts.next() else { continue };
        let mut chars = first.chars();
        let Some(letter) = chars.next().filter(|c| "tubs".contains(*c)) else { continue };
        let Ok(reg) = chars.as_str().parse::<u32>() else { continue };
        let space = parts.next().and_then(|s| s.strip_prefix("space")).and_then(|s| s.parse().ok()).unwrap_or(0);
        if matches!(space, 32765 | 32766) {
            continue;
        }
        let hit = covered.iter().any(|&(c, s, first, n)| c == letter && s == space && first <= reg && u64::from(reg - first) < n);
        let name = format!("{letter}{reg},space{space}");
        if !hit && !missing.contains(&name) {
            missing.push(name);
        }
    }
    missing.sort();
    missing
}
