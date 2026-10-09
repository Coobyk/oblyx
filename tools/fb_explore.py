#!/usr/bin/env python3
"""Schema-less FlatBuffers explorer for Notability index/ios/*.fb files.

These buffers are FlatBuffers, except the root table offset is stored as a u32
at byte 0 instead of the usual trailing 4 bytes. Field types are inferred
heuristically; this is a research aid, not a validator.

Usage: python3 tools/fb_explore.py FILE [--max-depth N] [--limit N]
"""
from __future__ import annotations

import argparse
import struct
import sys


class Buf:
    def __init__(self, data: bytes):
        self.d = data
        self.n = len(data)

    def u8(self, p):
        return self.d[p] if p < self.n else 0

    def u16(self, p):
        return struct.unpack_from("<H", self.d, p)[0] if p + 2 <= self.n else 0

    def u32(self, p):
        return struct.unpack_from("<I", self.d, p)[0] if p + 4 <= self.n else 0

    def i32(self, p):
        return struct.unpack_from("<i", self.d, p)[0] if p + 4 <= self.n else 0

    def f32(self, p):
        return struct.unpack_from("<f", self.d, p)[0] if p + 4 <= self.n else 0.0

    def f64(self, p):
        return struct.unpack_from("<d", self.d, p)[0] if p + 8 <= self.n else 0.0

    def blob(self, p, ln):
        return self.d[p:p + ln]


def vtable_at(b: Buf, p: int):
    """If p is a plausible vtable, return (vlen, tlen, [field offsets])."""
    if p < 0 or p + 4 > b.n:
        return None
    vlen = b.u16(p)
    tlen = b.u16(p + 2)
    if vlen < 4 or vlen % 2 or p + vlen > b.n:
        return None
    if tlen < 4 or p + tlen > b.n + vlen:
        return None
    if vlen > 512:
        return None
    fields = [b.u16(p + 4 + 2 * i) for i in range((vlen - 4) // 2)]
    for f in fields:
        if f and f >= tlen:
            return None
    return vlen, tlen, fields


def table_at(b: Buf, pos: int):
    """If pos is a table, return (tlen, fields)."""
    if pos < 0 or pos + 4 > b.n:
        return None
    soff = b.i32(pos)
    vt = vtable_at(b, pos - soff)
    if not vt:
        return None
    return vt[1], vt[2]


def looks_like_string(b: Buf, p: int):
    if p < 0 or p + 4 > b.n:
        return None
    ln = b.u32(p)
    if ln == 0 or ln > 4096 or p + 4 + ln > b.n:
        return None
    raw = b.blob(p + 4, ln)
    if all(32 <= c < 127 or c in (9, 10, 13) for c in raw):
        return raw.decode("utf-8", "replace")
    return None


def looks_like_vector(b: Buf, p: int):
    if p < 0 or p + 4 > b.n:
        return None
    ln = b.u32(p)
    if ln == 0 or ln > 1 << 20 or p + 4 + ln > b.n:
        return None
    return ln


def fmt_scalar(b: Buf, a: int, size_hint: int | None = None):
    out = []
    if size_hint in (None, 4):
        out.append(("u32", b.u32(a)))
        out.append(("i32", b.i32(a)))
        f = b.f32(a)
        if abs(f) > 1e-7 and abs(f) < 1e9:
            out.append(("f32", round(f, 6)))
    if size_hint in (None, 8):
        out.append(("f64", round(b.f64(a), 6)))
    return out


def walk(b: Buf, pos: int, depth: int, max_depth: int, seen: set, path: str, out: list):
    t = table_at(b, pos)
    if not t:
        out.append(f"{'  ' * depth}{path}: NOT A TABLE @{pos}")
        return
    tlen, fields = t
    if pos in seen or depth > max_depth:
        out.append(f"{'  ' * depth}{path}: (cycle/depth @{pos})")
        return
    seen.add(pos)
    out.append(f"{'  ' * depth}{path} @{pos} tlen={tlen} fields={fields}")
    for i, fo in enumerate(fields):
        if not fo:
            continue
        a = pos + fo
        nxt = f"{path}.f{i}"
        # string?
        tgt = a + b.u32(a)
        s = looks_like_string(b, tgt)
        if s is not None:
            out.append(f"{'  ' * (depth+1)}{nxt} str@{tgt}: {s[:90]!r}")
            continue
        if table_at(b, tgt):
            out.append(f"{'  ' * (depth+1)}{nxt} table@{tgt}")
            walk(b, tgt, depth + 2, max_depth, seen, nxt, out)
            continue
        vl = looks_like_vector(b, tgt)
        if vl is not None:
            # guess element width from remaining bytes
            out.append(f"{'  ' * (depth+1)}{nxt} vec@{tgt} len={vl}")
            _vector(b, tgt, vl, depth + 2, max_depth, seen, nxt, out)
            continue
        out.append(f"{'  ' * (depth+1)}{nxt} @{a} raw={fmt_scalar(b, a)}")


def _vector(b: Buf, tgt: int, ln: int, depth, max_depth, seen, path, out):
    base = tgt + 4
    rem = b.n - base
    for width in (4, 8, 2, 1):
        if ln * width <= rem:
            first = b.f32(base) if width == 4 else (b.u32(base) if width else 0)
            out.append(f"{'  ' * depth}{path}[0..] width={width} first={first}")
            if width == 4 and depth + 1 <= max_depth:
                for k in range(min(ln, 8)):
                    out.append(f"{'  ' * (depth+1)}{path}[{k}]={b.f32(base + 4*k):.4f}")
                if ln > 8:
                    out.append(f"{'  ' * (depth+1)}... {ln} elements total")
            return
    out.append(f"{'  ' * depth}{path} (vector len {ln}, width unclear)")


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("path")
    ap.add_argument("--max-depth", type=int, default=6)
    ap.add_argument("--limit", type=int, default=400)
    args = ap.parse_args()

    data = open(args.path, "rb").read()
    b = Buf(data)
    root = b.u32(0)
    print(f"# {args.path}  {len(data)} bytes")
    print(f"# root table offset (u32 @ 0) = {root}")
    out: list = []
    walk(b, root, 0, args.max_depth, set(), "root", out)
    print("\n".join(out[:args.limit]))
    if len(out) > args.limit:
        print(f"... {len(out) - args.limit} more lines")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())