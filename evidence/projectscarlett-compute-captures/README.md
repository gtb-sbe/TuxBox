# ProjectScarlett compute captures, replayed offline

Results of `tools/replay_captures.py` (see `tools/capture-replay/README.md`) over 223 compute pipelines that failed in one ProjectScarlett launch. The files hold digests, binding metadata and translator refusal text only. The captures themselves are title code and are not published.

## Setup

- **Captures:** 2026-11-26, ProjectScarlett 1.0.2.1, an offline launch on `main` `9b31fd8`, GE-Proton11-14 (vkd3d-proton `v3.1-83-gcae8353ed`), RX 9060 XT, Mesa 26.2.6 RADV. The run reached a state with 22 draws per frame after about 46 minutes and was stopped after 51 minutes. It wrote 351 pairs, 223 of them compute.
- **Replay:** 2026-11-27, offline, on the same host. Root choice from `main` `ab644f0`. `xshader` was built from `12e4b06`, whose translator and `tuxbox-dxbc` sources match `ab644f0`.
- **Compile check:** DXC and DXV 1.10.2605.37 (the nixpkgs Linux build; the game uses the Windows release of 2026-09-29), dxil-spirv `34f0ff8` (the version in GE-Proton11-14's vkd3d-proton).

## Files

| File | Contents |
| --- | --- |
| `replay-main.tsv` | One row per compute pair on `main`: digests, root shape, reflection, arm, root used, variant digest, result, and for translated pairs the profile, DXC + DXV, dxil-spirv and register coverage |
| `replay-main-summary.txt` | Counts by arm and by refusal text |
| `mismatches.tsv` | Every descriptor row whose FSSM class differs from the captured root's range type at that slot, with the user-data register and the PSV row |
| `roots.txt` | Decoded layouts of the roots involved below |
| `root-srv-experiment.patch` | A one-line change to `read_bindings` in `runtime/dxbc/src/compute_table.rs`: accept root SRVs (`0x100`) for compute, not only raster |
| `replay-root-srv-experiment.tsv`, `-summary.txt` | The same replay with the patch applied to `capture-replay`'s `tuxbox-dxbc` |

## Results

- **On `main`, 42 of 223 compute pairs translate:**
  - by arm: reflected 36/177, original 0/7, last 6/39;
  - all 42 pass DXC + DXV and dxil-spirv;
  - 40 have every register covered; `6900aba9` and `f5d8bc6c` lack `s66, space0`.
- **Type mismatches are common:** 192 of the 223 pairs have at least one descriptor slot whose FSSM class differs from the captured root's range type (714 rows).
  - In two-table roots, the verified table rewrite retypes those slots from the bindings.
  - The translator refuses only writes and atomics to slots it sees as non-UAV, so most mismatches show no refusal.
- **16 pairs refuse with `mubuf ... atomic on a non-UAV buffer`.** All 16 have a three-table root (`7e366593`, `c78d33d8`: tables, a root SRV, root constants). For each atomic slot, the FSSM used-resource row (class `0x303`), the user-data load and the PSV row all name the same UAV. Only the root types the slot SRV.
  - `compute_root_bindings` returns nothing for these programs: `read_bindings` maps their root SRV row only for raster stages. So the verified rewrite never runs.
  - **With `root-srv-experiment.patch`, all 16 translate**, pass DXC + DXV and dxil-spirv, and 15 have every register covered (`eb54f26a` lacks `s66`). No other pair changes result (58/223).
- **Layout families:**
  - Table 0 of the captured roots has 126, 197 or 261 slots. In every root that declares `u7`/`u6`, the pair sits at slots 119/120, 186/187 or 235/236 respectively.
  - The atomic programs put `u7`/`u6` at a position of another family than their pipeline root's.
  - The captures cannot tell whether the title pairs them that way or the provider pairs the wrong root.

## Reproducing

```sh
python tools/replay_captures.py <captures> --replay <capture-replay> --xshader <xshader> --work <work> \
    --compile --dxil-spirv <dxil-spirv> --mismatches mismatches.tsv > replay-main.tsv
git apply evidence/projectscarlett-compute-captures/root-srv-experiment.patch   # then rebuild capture-replay
```

Variant digests depend on the exact root bytes. `f5d8bc6c` gives `43ab2740`, the variant that launch used. Pairs whose appended root changed with #254 give new digests.

## Not run

GPU, a game run, vkd3d-proton with real bindings, raster pairs, the patch in the provider. Translating and compiling do not show that a pipeline binds the right descriptors or computes the right values.
