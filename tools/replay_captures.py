"""Replay captured compute pipelines through the provider's root choice and xshader.

Captures are the files TUXBOX_CAPTURE_SHADER_INPUTS (<digest>.dxbc) and
TUXBOX_CAPTURE_RASTER_ROOTS (<shader digest>-<root digest>.rootsig) write, in
CAPTURES/inputs and CAPTURES/roots. For each compute pair, `capture-replay pair`
rebuilds the roots the provider tries in its arm; each is translated in order
until one translates, as the provider does. See tools/capture-replay/README.md.

Prints one tab-separated row per pair and a summary on stderr. With --compile,
the translation is also compiled with the runtime's DXC policy and DXV, and
optionally converted with dxil-spirv; every HLSL register is checked against
the root it was translated for. Output names only digests, binding decisions
and refusal text; the work directory holds title code and stays local.
"""
import argparse
import collections
import hashlib
import os
import re
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def short(text, limit=200):
    text = re.sub(r"0x[0-9a-fA-F]+", "0x_", " ".join(text.split()))
    return text[:limit]


def refusal_class(result):
    """Refusal text without addresses, numbers and instruction spans."""
    text = re.sub(r"\[[^\]]*\]", "", result)
    text = re.sub(r"\b(at|@) 0x_", "", text)
    text = re.sub(r"\b\d+\b", "N", text)
    return re.sub(r"\s+", " ", text).strip(" :")


def translate(xshader, request, base):
    run = subprocess.run([str(xshader), str(request), "--hlsl-out", str(base.with_suffix(".hlsl")),
                          "--profile-out", str(base.with_suffix(".profile"))],
                         capture_output=True, text=True, timeout=600)
    if run.returncode == 0:
        return "ok"
    return short((run.stderr.strip().splitlines() or [f"exit {run.returncode}"])[-1])


def compile_variant(args, base, root):
    from project_tuxbox.runtime_shaders import STAGE_IDS, compile_dxil
    profile = base.with_suffix(".profile").read_text().strip()
    hlsl, cso, spv = base.with_suffix(".hlsl"), base.with_suffix(".cso"), base.with_suffix(".spv")
    cso.unlink(missing_ok=True)
    try:
        compile_dxil(args.dxc, args.dxv, profile, hlsl, cso, None, base.parent.name,
                     STAGE_IDS[profile.split("_")[0]])
        compiled = "ok"
    except Exception as error:  # keep going and report every compiler failure
        compiled = f"fail: {short(str(error), 160)}"
    converted = "-"
    if compiled == "ok" and args.dxil_spirv:
        env = dict(os.environ, LD_LIBRARY_PATH=str(Path(args.dxil_spirv).parent))
        run = subprocess.run([args.dxil_spirv, str(cso), "--output", str(spv), "--validate"],
                             capture_output=True, text=True, env=env, timeout=600)
        ok = run.returncode == 0 and spv.exists() and spv.read_bytes()[:4] == b"\x03\x02\x23\x07"
        converted = "ok" if ok else f"fail({run.returncode}): {short(run.stderr or run.stdout, 160)}"
    covered = subprocess.run([str(args.replay), "uncovered", str(hlsl), str(root)],
                             capture_output=True, text=True).stdout.strip()
    return [profile, compiled, converted, covered]


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("captures", type=Path, help="directory with inputs/ and roots/")
    parser.add_argument("--replay", type=Path, required=True, help="capture-replay binary")
    parser.add_argument("--xshader", type=Path, required=True, help="xshader binary")
    parser.add_argument("--work", type=Path, required=True, help="work directory (holds title code)")
    parser.add_argument("--only", nargs="*", default=[], help="shader digest prefixes")
    parser.add_argument("--compile", action="store_true", help="also compile like the runtime")
    parser.add_argument("--dxc", default=shutil.which("dxc"))
    parser.add_argument("--dxv", default=shutil.which("dxv"))
    parser.add_argument("--dxil-spirv", help="dxil-spirv binary for --compile")
    parser.add_argument("--mismatches", type=Path,
                        help="also write every descriptor row whose FSSM class differs from the "
                             "captured root's range type at that slot")
    args = parser.parse_args()
    if args.compile and not (args.dxc and args.dxv):
        parser.error("--compile needs dxc and dxv")
    sys.path.insert(0, str(ROOT))

    columns = ["shader", "root", "tables", "flags", "reflect", "arm", "used", "variant", "result"]
    if args.compile:
        columns += ["profile", "dxil", "spirv", "coverage"]
    print("\t".join(columns), flush=True)
    arms, classes = collections.Counter(), collections.Counter()
    mismatches = None
    if args.mismatches:
        mismatches = args.mismatches.open("w")
        mismatches.write("shader\troot\tarm\tresult\tclass\ttable\tslot\treg\troot_range\tuser_data\tpsv\n")
    for rootsig in sorted((args.captures / "roots").glob("*.rootsig")):
        shader_digest, root_digest = rootsig.stem.split("-", 1)
        if args.only and not shader_digest.startswith(tuple(args.only)):
            continue
        shader = args.captures / "inputs" / f"{shader_digest}.dxbc"
        if not shader.exists():
            continue
        out = args.work / rootsig.stem
        run = subprocess.run([str(args.replay), "pair", str(shader), str(rootsig), str(out)],
                             capture_output=True, text=True)
        if run.returncode != 0:
            print(f"{shader_digest[:8]}-{root_digest[:8]}: {short(run.stderr)}", file=sys.stderr)
            continue
        fields = dict(re.findall(r"(\w+)=(\S*)", run.stdout))
        if fields.get("stage") != "5":
            continue
        reflect = fields["reflect"]
        if reflect.startswith("append:"):
            reflect = f"append:{reflect.count(',') + 1}"
        used, variant, result = "-", "-", "not rebuilt"
        for name in filter(None, fields["try"].split(",")):
            request = out / f"{name}.dxbc"
            used, variant = name, hashlib.sha256(request.read_bytes()).hexdigest()[:8]
            result = translate(args.xshader, request, out / name)
            if result == "ok":
                break
        cls = "ok" if result == "ok" else refusal_class(result)
        arms[(fields["arm"], cls == "ok")] += 1
        classes[cls] += 1
        row = [shader_digest[:8], root_digest[:8], fields["tables"], fields["flags"], reflect,
               fields["arm"], used, variant, cls]
        if args.compile:
            row += compile_variant(args, out / used, out / f"{used}.rts0") if cls == "ok" else ["-"] * 4
        print("\t".join(row), flush=True)
        if mismatches:
            report = subprocess.run([str(args.replay), "records", str(shader), str(rootsig)],
                                    capture_output=True, text=True).stdout
            for line in report.splitlines():
                if not line.startswith("fssm ") or " mismatch" not in line:
                    continue
                f = dict(re.findall(r"(\w+)=(\S+)", line))
                mismatches.write("\t".join([shader_digest[:8], root_digest[:8], fields["arm"], cls, f["class"],
                                            f["table"], f["slot"], f["reg"], f["root"], f["user"], f["psv"]]) + "\n")

    if mismatches:
        mismatches.close()
    for (arm, ok), count in sorted(arms.items()):
        print(f"arm {arm} {'ok' if ok else 'refused'}: {count}", file=sys.stderr)
    for cls, count in classes.most_common():
        print(f"{count:4} {cls}", file=sys.stderr)


if __name__ == "__main__":
    main()
