//! Rebuild the roots the D3D12 provider translates a captured compute pipeline
//! against, and show the binding evidence of the program. Inputs are the files
//! `TUXBOX_CAPTURE_SHADER_INPUTS` and `TUXBOX_CAPTURE_RASTER_ROOTS` write. See
//! README.md.

mod records;
mod reflect;
mod root;

use std::path::Path;
use std::process::ExitCode;
use tuxbox_dxbc::rootsig::{RootParamKind, parse_rts0};

const USAGE: &str = "usage:
  capture-replay pair SHADER.dxbc ROOT.rootsig OUT_DIR
      Print the provider's decision as key=value pairs and write, for each root
      the provider tries in order (`try=`), OUT_DIR/<name>.rts0 and the
      translator request OUT_DIR/<name>.dxbc.
  capture-replay records SHADER.dxbc ROOT.rootsig
      Print FSSM used-resource and user-data rows and PSV rows, with the root
      range covering each descriptor slot.
  capture-replay root ROOT.rootsig...
      Print root layouts.
  capture-replay with-root SHADER.dxbc ROOT.rts0 OUT.dxbc
      Attach a root to a root-less program, as shader_cache::with_root does.
  capture-replay uncovered OUT.hlsl ROOT.rts0
      Print HLSL registers the root does not declare.";

fn read(path: &str) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("{path}: {e}"))
}

fn write(path: &Path, data: &[u8]) -> Result<(), String> {
    std::fs::write(path, data).map_err(|e| format!("{}: {e}", path.display()))
}

/// The DXIL part's program kind, the stage number the runtime uses (5 compute).
fn stage(shader: &[u8]) -> Option<u32> {
    let dxbc = tuxbox_dxbc::Dxbc::parse(shader).ok()?;
    let dxil = dxbc.chunk(b"DXIL")?;
    Some(u32::from_le_bytes(shader.get(dxil.offset..dxil.offset + 4)?.try_into().ok()?) >> 16)
}

fn pair(shader_path: &str, root_path: &str, out: &str) -> Result<(), String> {
    let shader = read(shader_path)?;
    let captured = read(root_path)?;
    let payload = root::payload(&captured)?;
    let signature = parse_rts0(payload)?;
    let out = Path::new(out);
    std::fs::create_dir_all(out).map_err(|e| format!("{}: {e}", out.display()))?;
    let stage = stage(&shader).ok_or("no DXIL part")?;
    let embedded = tuxbox_dxbc::Dxbc::parse(&shader)?.chunk(b"RTS0").is_some();
    let tables = signature.params.iter().filter(|p| matches!(p.kind, RootParamKind::Table(_))).count();
    let mut line = format!(
        "stage={stage} tables={tables} flags={:#x} statics={} embedded={}",
        signature.flags, signature.static_samplers.len(), if embedded { "yes" } else { "no" },
    );
    write(&out.join("captured.rts0"), payload)?;
    if stage != 5 {
        println!("{line} reflect=- arm=- try=");
        return Ok(());
    }
    let guest = match root::read(payload) {
        Ok(guest) => guest,
        Err(why) => {
            println!("{line} reflect=unread({why}) arm=- try=");
            return Ok(());
        }
    };
    let mut tries: Vec<(&str, Vec<u8>)> = Vec::new();
    let (reflect, arm) = match reflect::reflected(&shader, &signature, &guest) {
        reflect::Layout::Bindless => ("bindless".to_string(), "bindless"),
        reflect::Layout::Original { bindings } => {
            tries.push(("captured", payload.to_vec()));
            (format!("original:{bindings}"), "original")
        }
        reflect::Layout::Rewritten { bindings, root } => {
            tries.push(("reflected", root::write(&root)));
            (format!("rewrite:{bindings}"), "reflected")
        }
        reflect::Layout::Appended { ranges, root } => {
            tries.push(("reflected", root::write(&root)));
            let list: Vec<_> = ranges.iter().map(|b| {
                let letter = ['t', 'u', 'b'].get(b.kind as usize).unwrap_or(&'?');
                format!("{letter}{}:{}@{}/{}", b.register, b.space, b.table, b.slot)
            }).collect();
            (format!("append:{}", list.join(",")), "reflected")
        }
        reflect::Layout::Unsupported(why) => {
            // The last arm: the guest root with recovered samplers, then for a
            // root-less program the guest root itself (#260). A program with an
            // embedded root takes its prepared translation first (not rebuilt).
            if let Some(extended) = reflect::sampler_root(&shader, &signature, &guest) {
                tries.push(("samplers", root::write(&extended)));
            }
            if !embedded {
                tries.push(("captured", payload.to_vec()));
            }
            (format!("unsupported({})", why.replace(' ', "-")), "last")
        }
    };
    for (name, bytes) in &tries {
        write(&out.join(format!("{name}.rts0")), bytes)?;
        write(&out.join(format!("{name}.dxbc")), &root::with_root(&shader, bytes)?)?;
    }
    let names: Vec<_> = tries.iter().map(|(name, _)| *name).collect();
    line += &format!(" reflect={reflect} arm={arm} try={}", names.join(","));
    println!("{line}");
    Ok(())
}

fn run(args: &[String]) -> Result<(), String> {
    match args {
        [command, shader, rootsig, out] if command == "pair" => pair(shader, rootsig, out),
        [command, shader, rootsig] if command == "records" => {
            let shader = read(shader)?;
            let captured = read(rootsig)?;
            let signature = parse_rts0(root::payload(&captured)?)?;
            records::report(&shader, &signature).iter().for_each(|line| println!("{line}"));
            Ok(())
        }
        [command, roots @ ..] if command == "root" && !roots.is_empty() => {
            for path in roots {
                let data = read(path)?;
                println!("{path}: {}", root::describe(&root::read(root::payload(&data)?)?));
            }
            Ok(())
        }
        [command, shader, rts0, out] if command == "with-root" => {
            let (shader, data) = (read(shader)?, read(rts0)?);
            write(Path::new(out), &root::with_root(&shader, root::payload(&data)?)?)
        }
        [command, hlsl, rts0] if command == "uncovered" => {
            let hlsl = String::from_utf8_lossy(&read(hlsl)?).into_owned();
            let data = read(rts0)?;
            let missing = records::uncovered(&hlsl, &root::read(root::payload(&data)?)?);
            println!("{}", if missing.is_empty() { "all covered".into() } else { missing.join(" ") });
            Ok(())
        }
        _ => Err(USAGE.into()),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
