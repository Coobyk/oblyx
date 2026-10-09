# `.ntb` (Notability cloud) File Format — Reverse-Engineering Notes

Status: **in progress.** This document records only what has been *verified
against sample files*. Unverified guesses are marked as hypotheses and must not
be relied on.

Samples used for verification were personal notebooks; they are **not** part of
this repository and no content derived from them is committed here. The probe
tooling (`tools/ntb_probe.py`) reads a path given on the command line.

Not affiliated with or endorsed by Notability or Notefile.

---

## 1. Container — VERIFIED

A `.ntb` file is a plain **ZIP archive**, members stored uncompressed
(`compression method = store`).

Observed member layout:

```
version                            1 B      ASCII "1"
manifest.json                      ~109 B   JSON, see below
noteBundle                         main payload
thumbnail.png                      cover thumbnail (PNG)
index/ios/HandwritingIndex.fb      FlatBuffer, per-page stroke OCR index
index/ios/ImageIndex.fb            FlatBuffer, raster image index
index/ios/PDFIndex.fb              FlatBuffer, PDF-page index
assets/<128-hex>.<ext>             content-addressed attachments
```

Notes:

- Only the index type actually present varies by notebook; a notebook with an
  imported PDF carries `PDFIndex.fb`, one with images carries `ImageIndex.fb`,
  and `HandwritingIndex.fb` is essentially always present.
- `assets/` names are the **first 128 hex characters of the SHA-512 of the file
  contents**, plus the extension. This makes assets trivially verifiable: hash
  the member and confirm it matches its own name. (It is *not* SHA-256; that was
  an early wrong guess.) Extensions observed: `.webp`, `.pdf`.
- All 24 assets across 9 sample notebooks verified against SHA-512[:128].
- No directory entries are required; the paths above are the logical layout.

## 2. `manifest.json` — VERIFIED

```json
{
  "subjectId": "<UUID>",
  "subjectName": "<display name>",
  "appVersion": "16.12.5"
}
```

Key order is not stable across files, so consumers must parse the JSON rather
than assume an ordering. `appVersion` is the Notability version that wrote the
archive.

## 3. `version` — VERIFIED

A single byte containing the ASCII character `1`.

## 4. `noteBundle` — PARTIALLY REVERSE-ENGINEERED

This is the bulk of the notebook. Its framing is **not yet solved**.

### 4.1 What is known

`noteBundle` is **not**:

- a ZIP,
- a single top-level protobuf message (parses fail on every sample),
- a flat stream of `varint length ‖ protobuf` records,
- a flat stream of `u32 length ‖ payload` records,
- a sequence of `u16`-length-prefixed records.

It is a raw binary container. Protobuf-style *sub-records* appear to be present
inside it, but the outer framing rule has not been identified.

### 4.2 Rejected hypothesis: record-offset table

A `u32 count` followed by `count-1` strictly-descending `u32` values (offsets
measured backwards from EOF) reproduces the layout of the two smallest sample
notebooks (`8456 B` and `4064 B`) exactly. It does **not** generalise: it
matches 3 of 9 sample notebooks, and in one of those the detected table sits in
the middle of the file. Treated as coincidental structure, not the container
rule.

### 4.3 Stroke point encoding — VERIFIED

This is the one part of `noteBundle` that is nailed down, and it is the part
that matters most for rendering.

Ink strokes are stored as a run of fixed-size **19-byte** units:

```
offset  size  meaning
0       4     f32 LE   x
4       4     f32 LE   y
8       4     f32 LE   z
12      7     trailer, always 00 3c 00 3c ff 00 00
```

Evidence (smallest sample notebook, 8456 B): the 7-byte trailer occurs 321
times; of the 333 byte-runs between consecutive trailers, **290 are exactly
19 bytes**, and those decode to plausible page coordinates. Records begin with a
short metadata header before the first unit and end with a partial unit.

Open questions on strokes:

- the meaning of the third `f32` (a third coordinate, pressure, or a time
  value) is not established;
- the per-stroke header (colour, width, style/pen, transform) is not decoded;
- whether strokes are bezier-sampled or polyline is not established.

The older, pre-cloud `.note` format is documented separately (Julia Evans,
2018) and uses an Apple `NSKeyedArchiver` `Session.plist` with `curvespoints`,
`curvesnumpoints`, `curveswidth`, `curvescolors`, `curvesstyles`,
`curvesforces`, `curvesfractionalwidths` arrays. That is a **different**
container and does not apply to `noteBundle`, but it confirms the domain model
Notability uses for ink: per-curve point counts, widths, RGBA colours and a
per-curve style index, where alpha `< 255` marks highlighter strokes.

## 5. `index/ios/*.fb` — NOT STARTED

FlatBuffers. Filenames indicate content (`HandwritingIndex`, `ImageIndex`,
`PDFIndex`) and one file exists per page in at least some notebooks. No schema
has been recovered; these are almost certainly the page directory and the
OCR/text index, and decoding them is likely the cheapest route to the
`noteBundle` page structure.

## 6. Conversion implications

For rendering (`→` SVG/PDF/PNG) the format is tractable: assets are
self-describing and content-addressed, and ink is decoded.

For **editable** export (`→` `.ntb` or `→` `.goodnotes`) the format must be
*understood*, not merely read, and additionally must be validated by round-tripping
through the real applications. No writer should be considered correct until it
has been opened and re-saved by Notability / GoodNotes on a device.