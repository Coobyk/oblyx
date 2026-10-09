#!/usr/bin/env python3
"""Probe Notability .ntb archives.

Research tooling for docs/NOTABILITY_FORMAT.md. Reads paths from argv; it never
contains or embeds notebook content.

Usage:
    python3 tools/ntb_probe.py notebook.ntb [more.ntb ...]
"""
from __future__ import annotations

import collections
import hashlib
import json
import struct
import sys
import zipfile

# 3 x f32 (x, y, z) + 7-byte constant trailer.
STROKE_TRAILER = b"\x00\x3c\x00\x3c\xff\x00\x00"
STROKE_UNIT = 12 + len(STROKE_TRAILER)  # 19

KNOWN_INDEXES = ("HandwritingIndex.fb", "ImageIndex.fb", "PDFIndex.fb")


def u32(buf: bytes, off: int) -> int:
    return int.from_bytes(buf[off:off + 4], "little")


def inspect_archive(path: str) -> dict:
    out: dict = {"file": path.rsplit("/", 1)[-1]}
    with zipfile.ZipFile(path) as zf:
        names = zf.namelist()
        out["members"] = len(names)
        out["entries"] = []
        for info in zf.infolist():
            out["entries"].append(
                {
                    "name": info.filename,
                    "size": info.file_size,
                    "method": "store" if info.compress_type == 0 else str(info.compress_type),
                }
            )
        if "manifest.json" in names:
            out["manifest"] = json.loads(zf.read("manifest.json"))
        if "version" in names:
            out["version"] = zf.read("version").decode("ascii", "replace")
        out["indexes"] = [n for n in names if n.endswith(".fb")]

        # Assets are content-addressed: name must equal sha512(blob)[:128].
        assets = {}
        for name in names:
            if name.startswith("assets/"):
                blob = zf.read(name)
                digest = hashlib.sha512(blob).hexdigest()
                stem = name.rsplit(".", 1)[0].rsplit("/", 1)[-1]
                assets[name] = {
                    "size": len(blob),
                    "ext": name.rsplit(".", 1)[-1],
                    "sha512_ok": digest[:128] == stem,
                }
        out["assets"] = assets

        if "noteBundle" in names:
            out["bundle"] = inspect_bundle(zf.read("noteBundle"))
    return out


def inspect_bundle(nb: bytes) -> dict:
    res: dict = {"size": len(nb), "head": nb[:16].hex()}
    counts = collections.Counter(i for i in range(len(nb)) if nb.startswith(STROKE_TRAILER, i))
    res["stroke_trailers"] = len(counts)
    res["stroke_points_est"] = len(counts)

    # Runs between consecutive trailers: the dominant length should be 19.
    pos = sorted(counts)
    gaps = collections.Counter(b - a for a, b in zip(pos, pos[1:]))
    res["trailer_gap_histogram"] = gaps.most_common(8)
    res["gap19_fraction"] = round(gaps[STROKE_UNIT] / max(1, sum(gaps.values())), 3)

    # Sanity-check that the 19-byte units really hold plausible coordinates.
    ok = bad = 0
    for p in pos:
        if p < 12:
            continue
        x, y, z = struct.unpack_from("<fff", nb, p - 12)
        if all(abs(v) < 1e6 for v in (x, y, z)):
            ok += 1
        else:
            bad += 1
    res["unit_coords_plausible"] = f"{ok}/{ok + bad}"
    return res


def main(argv: list[str]) -> int:
    if len(argv) < 2:
        print(__doc__)
        return 2
    for path in argv[1:]:
        try:
            info = inspect_archive(path)
        except (OSError, zipfile.BadZipFile, KeyError) as exc:
            print(f"{path}: ERROR {exc}")
            continue
        print(f"== {info['file']} ==")
        print(f"  members      {info['members']}")
        print(f"  version      {info.get('version', '-')!r}")
        print(f"  manifest     {info.get('manifest', '-')}")
        print(f"  indexes      {info.get('indexes') or '-'}")
        for name, meta in (info.get("assets") or {}).items():
            flag = "ok" if meta["sha512_ok"] else "MISMATCH"
            print(f"  asset        {name.rsplit('/', 1)[-1][:16]}… "
                  f"{meta['size']:>9} B  sha512[:128]={flag}")
        bundle = info.get("bundle")
        if bundle:
            print(f"  noteBundle   {bundle['size']} B")
            print(f"  stroke units {bundle['stroke_points_est']} "
                  f"(gap19 fraction {bundle['gap19_fraction']}, "
                  f"coords {bundle['unit_coords_plausible']})")
            print(f"  gap histogram {bundle['trailer_gap_histogram']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))