# capture-replay

Replays captured compute pipelines offline the way the D3D12 provider would translate them. For each (program, root) pair the runtime captured, `capture-replay` rebuilds the root the provider translates against in its pipeline arm. `tools/replay_captures.py` then runs `xshader` on it, and optionally DXC, DXV and dxil-spirv. Use it to reproduce a translator refusal or a binding problem without launching the title.

The root choice is a copy of the compute half of `runtime/d3d12/src/device/compute_table.rs` at `main` `ab644f0` (see `src/reflect.rs`). The provider code only builds for Windows and serializes roots through D3D12, so the logic cannot be called directly. When that file changes, update the copy.

## Capturing pairs

`compute_table::capture_stage_root` in the provider writes them when two variables name existing directories:

| Variable | File | Contents |
| --- | --- | --- |
| `TUXBOX_CAPTURE_SHADER_INPUTS=<dir>` | `<sha256>.dxbc` | The guest's stage bytes as passed to pipeline creation |
| `TUXBOX_CAPTURE_RASTER_ROOTS=<dir>` | `<stage sha256>-<RTS0 sha256>.rootsig` | The pipeline's serialized root (a DXBC container); compute roots too, despite the name |

Compute pairs are written when the `Reflected` arm finds no translation, when the last arm refuses a root-less program, and when the driver rejects the pipeline. Existing files are kept. For example:

```sh
mkdir -p <captures>/inputs <captures>/roots
TUXBOX_CAPTURE_SHADER_INPUTS=<captures>/inputs TUXBOX_CAPTURE_RASTER_ROOTS=<captures>/roots \
python tuxbox.py run --offline --proton <proton> <copy of the package>
```

Captures contain title code. Keep them, and the work directory below, out of the repository and off GitHub.

## Building

```sh
cargo build --release --locked --target x86_64-unknown-linux-gnu --manifest-path tools/capture-replay/Cargo.toml
cargo build --release --locked --target x86_64-unknown-linux-gnu --manifest-path tools/shader-compiler/Cargo.toml
```

Run from the repository root, both land in `build/cargo/x86_64-unknown-linux-gnu/release/` (`capture-replay`, `xshader`).

## One pair

```sh
capture-replay pair <captures>/inputs/<shader>.dxbc <captures>/roots/<shader>-<root>.rootsig <work>/<pair>
```

This prints the provider's decision as `key=value` pairs:

- `reflect` is the result of the root reflection:
  - `rewrite:N`: N user-data-verified bindings; the tables are rewritten with one range per binding, typed by the binding.
  - `original:N`: the guest root already describes them.
  - `append:<ranges>`: `append_corroborated_ranges`.
  - `bindless`: #235's arm; not rebuilt here.
  - `unsupported(<reason>)`.
- `arm` is `reflected`, `original` or `last`.
- `try` lists the roots the provider tries, in order:
  - `reflected`: the reflected root.
  - `samplers`: the guest root with recovered literal samplers.
  - `captured`: the guest root.

For each entry in `try`, `<name>.rts0` is the root and `<name>.dxbc` the translator request (`shader_cache::with_root`). The SHA-256 of the request is the variant digest the runtime cache and failure records use. Translate a request with `xshader <name>.dxbc --hlsl-out <name>.hlsl --profile-out <name>.profile`.

Other commands:

- `capture-replay records <shader> <rootsig>`: the binding evidence, one row per line.
  - FSSM used-resource rows. A descriptor row gets its table slot (byte offset / 8), the root range covering that slot, the user-data load at the same offset and table, and the PSV row of that class and register. `mismatch` marks a row whose root range has another type.
  - FSSM user-data rows: `[class, byte offset, width, register, parameter << 8 | SGPR]`; class 3 is a table descriptor load.
  - PSV resource rows.
- `capture-replay root <rootsig>...`: the root layout, with runs of single-descriptor ranges grouped.
- `capture-replay with-root <shader> <rts0> <out>`: attach any root.
- `capture-replay uncovered <hlsl> <rts0>`: HLSL registers the root does not declare, outside the translator's private spaces. It stands in for vkd3d-proton's remap, which needs real bindings.

## A whole capture directory

```sh
python tools/replay_captures.py <captures> --replay <capture-replay> --xshader <xshader> --work <work> \
    [--compile --dxil-spirv <dxil-spirv>] [--mismatches mismatches.tsv] [--only <digest prefix>...] > results.tsv
```

This prints one row per compute pair: digests, root shape, the reflection, the arm, the root that was used, the variant digest and the result. For the last arm, it tries the next root after a refusal, as the provider does. A refusal in the `reflected` arm is final here; the provider then falls back to the program's root-less translation, which the driver usually rejects.

- `--compile` adds `project_tuxbox.runtime_shaders.compile_dxil` (the runtime's DXC flags, then DXV; `dxc` and `dxv` from `PATH` or `--dxc`/`--dxv`), dxil-spirv `--validate`, and the coverage check.
- `--mismatches` writes every descriptor row whose FSSM class differs from the captured root's range type.
- The summary on stderr counts results by arm and refusal text.

## Not covered

- The bindless index map (#235).
- The heap stride table retry (`reflected_with_strides`).
- Programs with an embedded root (their prepared translation comes first).
- Raster pipelines.
- Device-side bookkeeping (`lengths`, `accessed_views`).
- Whether vkd3d-proton binds the result, and whether the GPU computes the right values. Translating and compiling do not show either.
