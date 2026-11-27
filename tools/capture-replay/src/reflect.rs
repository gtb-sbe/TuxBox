//! The compute half of the provider's root reflection, ported from
//! `runtime/d3d12/src/device/compute_table.rs` (`reflected_root`,
//! `append_corroborated_ranges`, `sampler_root`, `literal_samplers`) at `main`
//! `ab644f0`. It decides which root a compute pipeline is translated against.
//! Keep it in step with that file; it is a copy, not shared code.
//!
//! Not ported: the bindless arm's index map (#235), the heap stride table retry
//! (`reflected_with_strides`), and device-side results such as `lengths` and
//! `accessed_views`.

use crate::root::{Param, Root, VOLATILE};
use tuxbox_dxbc::compute_table::{self, Binding, ROOT_SRV};
use tuxbox_dxbc::inline_sampler;
use tuxbox_dxbc::rootsig::{RootParamKind, RootSig};

/// The provider's `TableLayout` for a compute program, with the root it builds.
pub enum Layout {
    /// `bindless_compute_root`: Reflected with an index map (not rebuilt here).
    Bindless,
    /// The guest root fits the verified bindings: `TableLayout::Original`.
    Original { bindings: usize },
    /// The table rewrite: `TableLayout::Reflected`.
    Rewritten { bindings: usize, root: Root },
    /// `append_corroborated_ranges`: `TableLayout::Reflected`.
    Appended { ranges: Vec<Binding>, root: Root },
    /// `TableLayout::Unsupported`, and why.
    Unsupported(&'static str),
}

/// `layout_view`: hide an embedded RTS0 from the binding reader.
fn layout_view(shader: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    let tag = tuxbox_dxbc::Dxbc::parse(shader).ok()
        .and_then(|dxbc| dxbc.chunk(b"RTS0").map(|root| root.offset))
        .and_then(|offset| offset.checked_sub(8))
        .filter(|&tag| shader.get(tag..tag + 4) == Some(b"RTS0"));
    match tag {
        Some(tag) => {
            let mut copy = shader.to_vec();
            copy[tag..tag + 4].copy_from_slice(b"RTSX");
            copy.into()
        }
        None => shader.into(),
    }
}

fn missing_literal_samplers(shader: &[u8], stage: u32, existing: &[[u32; 13]]) -> Vec<[u32; 13]> {
    let Some(private) = inline_sampler::private(shader, stage) else { return Vec::new() };
    let covered = |p: &[u32; 13]| existing.iter().any(|s| s[..10] == p[..10] && (s[12] == 0 || s[12] == p[12]));
    if private.iter().all(covered) { Vec::new() } else { private }
}

pub fn literal_samplers(shader: &[u8], stage: u32, signature: &RootSig) -> Vec<[u32; 13]> {
    if let Some(unmatched) = inline_sampler::unmatched(shader, stage, signature) {
        return unmatched;
    }
    if signature.flags & 0x800 == 0 {
        missing_literal_samplers(shader, stage, &signature.static_samplers)
    } else {
        Vec::new()
    }
}

/// `sampler_root`: the guest root with the literal samplers it lacks, tried in
/// the last arm before the guest root itself.
pub fn sampler_root(shader: &[u8], signature: &RootSig, root: &Root) -> Option<Root> {
    let recovered = literal_samplers(shader, 5, signature);
    if recovered.is_empty() {
        return None;
    }
    let mut extended = root.clone();
    extended.samplers.extend(recovered);
    Some(extended)
}

fn bindless_compute_root(signature: &RootSig) -> bool {
    let cost = signature.params.iter().try_fold(0u32, |cost, p| match p.kind {
        RootParamKind::Constants { num32, .. } => Some(cost.saturating_add(num32)),
        RootParamKind::Descriptor { .. } => Some(cost.saturating_add(2)),
        _ => None,
    });
    signature.flags & 0x400 != 0 && cost.is_some_and(|cost| cost <= 62)
}

/// `reflected_root` for `Program::Compute` (strides off).
pub fn reflected(shader: &[u8], signature: &RootSig, root: &Root) -> Layout {
    if bindless_compute_root(signature) {
        return Layout::Bindless;
    }
    let params = signature.params.as_slice();
    let tables = match params {
        [tables @ .., last] if matches!(last.kind, RootParamKind::Constants { num32: 1..=4, .. }) => tables,
        _ => params,
    };
    let leading = params.iter().take_while(|p| matches!(p.kind, RootParamKind::Table(_))).count();
    let simple = (1..=2).contains(&tables.len()) && tables.iter().all(|p| matches!(p.kind, RootParamKind::Table(_)));
    let extended = !simple && leading >= 1 && params[leading..].iter()
        .all(|p| matches!(p.kind, RootParamKind::Constants { .. } | RootParamKind::Descriptor { .. }));
    if !simple && !extended {
        return Layout::Unsupported("root shape");
    }
    let recovered_samplers = if signature.static_samplers.is_empty() && signature.flags & 0x800 == 0 {
        inline_sampler::private(shader, 5).unwrap_or_default()
    } else {
        literal_samplers(shader, 5, signature)
    };
    let view = layout_view(shader);
    let mut resolve_constants = None;
    let mut verified_compute = false;
    let checked = compute_table::compute_root_bindings(&view, signature).map(|(bindings, constants)| {
        resolve_constants = constants;
        verified_compute = true;
        bindings
    });
    let bindings = if tables.len() == 2 || extended {
        checked
    } else {
        checked.or_else(|| compute_table::bindings(&view, 5))
    };
    let Some(bindings) = bindings else {
        return appended(shader, signature, root);
    };
    let (root_srvs, bindings): (Vec<_>, Vec<_>) = bindings.into_iter().partition(|b| b.kind == ROOT_SRV);
    let mut srv_registers = Vec::new();
    for b in &root_srvs {
        let Some(RootParamKind::Descriptor { kind: 3, register, space }) =
            signature.params.get(b.table as usize).map(|p| &p.kind) else {
            return Layout::Unsupported("root SRV binding names no SRV parameter");
        };
        if (*register, *space) != (b.register, b.space) {
            srv_registers.push((b.table as usize, b.register, b.space));
        }
    }
    if signature.version != 2 || bindings.is_empty() {
        return Layout::Unsupported("no table bindings");
    }
    let mut rewrite = vec![false; signature.params.len().max(2)];
    for param in &signature.params {
        if let RootParamKind::Table(ranges) = &param.kind
            && (param.visibility != 0 || ranges.is_empty() || ranges.iter().any(|r| r.range_type > 2 || r.num == u32::MAX))
        {
            return Layout::Unsupported("table shape");
        }
    }
    for b in &bindings {
        let Some(RootParamKind::Table(ranges)) = signature.params.get(b.table as usize).map(|p| &p.kind) else {
            return Layout::Unsupported("binding outside a table");
        };
        let mut next = 0u32;
        let covered = ranges.iter().any(|r| {
            let start = if r.offset_in_table == u32::MAX { next } else { r.offset_in_table };
            next = start.saturating_add(r.num);
            (start..next).contains(&b.slot) && r.range_type == b.kind && r.space == b.space
                && r.base_register.checked_add(b.slot - start) == Some(b.register)
        });
        rewrite[b.table as usize] |= !covered;
    }
    let rewrite_constants = resolve_constants.filter(|c| {
        !matches!(signature.params.get(c.param as usize).map(|p| &p.kind),
            Some(RootParamKind::Constants { register, space, .. }) if (*register, *space) == (c.register, c.space))
    });
    if !rewrite.contains(&true) && srv_registers.is_empty() && rewrite_constants.is_none() && recovered_samplers.is_empty() {
        return Layout::Original { bindings: bindings.len() };
    }
    let mut built = root.clone();
    for (index, param) in built.params.iter_mut().enumerate() {
        let Param::Table { ranges: guest, .. } = param else { continue };
        if !rewrite[index] {
            continue;
        }
        let mut ranges = Vec::new();
        for b in bindings.iter().filter(|b| b.table as usize == index) {
            let mut matched = guest.iter().filter(|r| r[0] == b.kind);
            let flags = match matched.next() {
                Some(first) if matched.any(|r| r[4] != first[4]) => return Layout::Unsupported("mixed range flags"),
                Some(first) => first[4],
                None if verified_compute || resolve_constants.is_some() || !recovered_samplers.is_empty() => VOLATILE,
                None => return Layout::Unsupported("no guest range of the binding's type"),
            };
            ranges.push([b.kind, 1, b.register, b.space, flags, b.slot]);
        }
        *guest = ranges;
    }
    for (index, register, space) in srv_registers {
        let Some(Param::Words { kind: 3, words, .. }) = built.params.get_mut(index) else {
            return Layout::Unsupported("root SRV rename");
        };
        words[0] = register;
        words[1] = space;
    }
    if let Some(constants) = rewrite_constants {
        let Some(Param::Words { kind: 1, words, .. }) = built.params.get_mut(constants.param as usize) else {
            return Layout::Unsupported("constant rename");
        };
        words[0] = constants.register;
        words[1] = constants.space;
    }
    built.samplers.extend(recovered_samplers);
    Layout::Rewritten { bindings: bindings.len(), root: built }
}

/// `append_corroborated_ranges`: keep every range, add one per corroborated
/// descriptor the root leaves out.
fn appended(shader: &[u8], signature: &RootSig, root: &Root) -> Layout {
    if signature.version != 2 {
        return Layout::Unsupported("no bindings (root version 1.0)");
    }
    let extra = compute_table::missing_root_descriptors(shader, signature);
    if extra.is_empty() {
        return Layout::Unsupported("no bindings, nothing to append");
    }
    // #269: every table needs a finite extent once the root is replaced.
    let unbounded = signature.params.iter().any(|p| matches!(&p.kind, RootParamKind::Table(ranges)
        if ranges.iter().any(|r| r.num == u32::MAX)));
    if unbounded {
        return Layout::Unsupported("append with an unbounded table");
    }
    let mut built = root.clone();
    for (index, param) in built.params.iter_mut().enumerate() {
        let Param::Table { ranges, .. } = param else { continue };
        let added: Vec<_> = extra.iter().filter(|b| b.table as usize == index).map(|b| {
            let flags = ranges.iter().find(|r| r[0] == b.kind).map_or(VOLATILE, |r| r[4]);
            [b.kind, 1, b.register, b.space, flags, b.slot]
        }).collect();
        ranges.extend(added);
    }
    Layout::Appended { ranges: extra, root: built }
}
