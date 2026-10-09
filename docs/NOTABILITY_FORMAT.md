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

Framings tried and ruled out for the whole file:

- a ZIP, or a single top-level protobuf message (parses fail on every sample);
- a flat stream of `varint length ‖ protobuf`, `u32 length ‖ payload`, or
  `u16 length ‖ payload` records;
- a FlatBuffers buffer read the standard way. The leading
  `u32 ‖ u16, u16, u16…` run *is* shaped exactly like a FlatBuffers vtable, and
  in the `.fb` files the leading `u32` does equal the position of a table-like
  object, with `vtable_len`/`table_len` and field offsets reading sensibly. It
  nevertheless fails to validate: field offsets overlap, and the object that
  appears to own the OCR blob is referenced by nothing. Something in this
  variant is still unidentified.

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

### 4.4 Stroke headers — PARTIALLY VERIFIED

Every run of point units is preceded by a header of **variable length**, roughly
90–140 bytes. Measured across 6 165 stroke runs in 8 sample notebooks, the
header contains:

| offset | type | observation |
| --- | --- | --- |
| +0 | u32 | flags; only `0` and `416` (`0x1A0`) are common |
| +8 | u32 | an id that decreases by 2 between consecutive strokes in one sample |
| +12 | i32 | negative; magnitude varies per stroke |
| +16 | u32 | varies; **not** the record length or point count |
| +20 | u32 | constant `36` within a notebook |
| +24 | f32 | **stroke width** |
| +28 | u32 | usually `0` |
| +32 | 4 B | `00 ff 01 XX`: the `00 ff 01` prefix is constant and `XX` varies per stroke, so this is almost certainly the **colour** |
| +44 | u32 | usually `0`, occasionally `1` |

**Widths are integer multiples of a base pen width.** Across samples the base is
`0.7751` (one notebook uses `0.725`) and the observed multipliers are
`1, 2, 4, 5, 10, 15, 30` — which line up with Notability's pen and highlighter
size presets. Reading these as `base × preset` is therefore safe even though the
exact header field layout is not.

Caveat that limits all of the above: these offsets were read at a fixed window
before each run, which aligns for the dominant case but not universally. The
header length is genuinely variable, so the field table should be treated as a
strong hypothesis to be confirmed once the record framing is solved.

The 90–140 byte gaps between point runs are **not** a pure header: their longest
common suffix across runs is 0–13 bytes. Other record types therefore interleave
with strokes, which is the main obstacle to isolating headers reliably.

## 5. `index/ios/*.fb` — PARTIALLY REVERSE-ENGINEERED

Filenames track content: `HandwritingIndex` is present in every notebook,
`PDFIndex` when the notebook has PDF pages, `ImageIndex` when it has raster
images. Sizes are highly variable (256 B to 119 KB).

### 5.1 These are the OCR / recognized-text indexes — VERIFIED

`index/ios/ImageIndex.fb` in one sample contains **UTF-8 recognized text** as a
`u32`-length-prefixed blob: a length of `1184` at offset `9500`, followed by
1184 bytes of German prose running to within 4 bytes of EOF. The text is real
OCR output of the page content, not metadata.

Consequence: these indexes are a *derived, lossy* side-car (handwriting
recognition plus page geometry), not the document itself. A converter must not
treat them as the source of truth for page content — but they are a cheap,
readable oracle for what text the page contains, and their geometry floats are a
useful cross-check on a stroke decoder.

### 5.2 Buffer layout — FLATBUFFERS-LIKE, NOT CONFIRMED

Every `.fb` buffer, and `noteBundle` as well, begins with the same
`u32 ‖ u16…` tag run, e.g. `10 00 00 00 0c 00 14 00 13 00 12 00 0c 00 04 00`.
That shape is exactly a FlatBuffers vtable (`u16 vtable_len`, `u16 table_len`,
then `u16` field offsets), and the first `u32` appears to be the position of the
root table rather than the trailing root offset that stock FlatBuffers uses.

However, walking the buffer as FlatBuffers does **not** validate: field offsets
read from these pseudo-vtables overlap and point at positions with no coherent
owner, and the table that appears to reference the OCR blob is not itself
referenced by anything. So the buffers are *similar to* FlatBuffers but are not
stock FlatBuffers, or use a variant layout that has not been identified.

This remains the most promising lead for the page model, and it is cheap to keep
pushing, but it is not solved.

`tools/fb_explore.py` is a schema-less walker for these buffers and is where
that work continues.

## 6. Conversion implications

For rendering (`→` SVG/PDF/PNG) the format is tractable: assets are
self-describing and content-addressed, and ink is decoded.

For **editable** export (`→` `.ntb` or `→` `.goodnotes`) the format must be
*understood*, not merely read, and additionally must be validated by round-tripping
through the real applications. No writer should be considered correct until it
has been opened and re-saved by Notability / GoodNotes on a device.