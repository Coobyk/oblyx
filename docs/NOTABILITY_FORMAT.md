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

## 4. `noteBundle` — FlatBuffer, PARTIALLY DECODED

`noteBundle` **is a standard FlatBuffer** (an earlier claim that it was ruled
out was wrong — the walker simply used a broken discriminator order). The root
`u32` uoffset sits at byte 0 and points to the root table.

### 4.1 Root table — VERIFIED (all 9 samples)

| field | type | meaning |
| --- | --- | --- |
| f0 @56 | 16 B inline | note GUID (raw bytes, not a FlatBuffer string) |
| f3 @120 | string | uppercase device UUID, e.g. `"798E2A39-…"` |
| f4 @40 | u64 | note create/modify timestamp, ms since Unix epoch |
| f5 @76 | string | lowercase device UUID |
| f6 | vector | list of element tables (see 4.2) |
| f7 @30 | u32 | version-like; `8650770` in one sample, `8388626` in another |

Element counts in `root.f6` (full walk, no cap):

| sample | elements |
| --- | --- |
| B1 | 802 |
| Cambridge | 19 |
| D1 | 1882 |
| E1 | 2265 |
| Eth1 | 282 |
| F1 | 1556 |
| G1 | 2802 |
| M1 | 24 |
| P1 | 1235 |

### 4.2 Element taxonomy — PARTIALLY DECODED

Each `root.f6[i]` is a **wrapper table** with `nf=6` vtable fields. The wrapper
holds u64 timestamps and a `f5` uoffset pointing at the **inner object** table.

The inner table's field count (`nf`) determines the element type:

| inner nf | count (Eth1) | type | key fields |
| --- | --- | --- | --- |
| 16 | 220 | **handwriting stroke** (§4.3) | f1 anchor, f8 width, f9 points |
| 12 | 19 | **shape / drawing** | f1 f32 Y, f5 vector, f9 colour, f10 width, f11 u32 |
| 4 | 16 | unknown | f1 u32 small, f3 u8 |
| 1 | 13 | lightweight element / annotation | f0 u32 (0, 4, or hash) |
| 15 | 7 | **shape / drawing** | f1 f32 Y, f5 vector, f9 colour, f10 width, f14 f32 |
| 7 | 3 | unknown | f0 vector n=1, f5 u8, f6 u32 |
| 8 | 1 | **text box** | f4 string (font name, e.g. `"Inter"`), f5 f32 (font size) |
| 2 | 1 | **page definition** | f1 vector: [width, height, …] in points |
| 13 | 1 | unknown | f0 vector n=1, f12 vector n=2 |
| 11 | 1 | **shape / drawing** | f1 f32 Y, f9 colour, f10 width |

**nf=2 (page definition):** The f1 vector's first two f32s are the page width
and height in points. Verified across samples:

| sample | pages | dimensions (pt) | orientation |
| --- | --- | --- | --- |
| B1 | 2 | 651 × 407 | custom |
| E1 | 5 | 596 × 842 | A4 portrait |
| Eth1 | 1 | 596 × 842 | A4 portrait |
| F1 | 7 | 455 × 571 | custom |
| G1 | 6 | (varies) | — |
| P1 | 6 | 842 × 595 | A4 landscape |
| Cambridge, D1, M1 | 0 | — | continuous-scroll |

**nf=8 (text box):** f4 is a FlatBuffer string holding the font family
(e.g. `"Inter"`). f5 is an f32 font size (e.g. `14.0`). f3 is a u8 flag
(bold?). The actual text characters are stored elsewhere (not in the element
table); f1 is a vector of packed u32s, possibly glyph or layout data.

**nf=11 / 12 / 15 (shapes):** All three share a common structure:
- f1: f32, page Y coordinate (same range as stroke anchors)
- f9: u32 colour, e.g. `0xFF000000` (RGBA bytes `00 00 00 FF` = black)
- f10: f32 width (e.g. 0.7751, matching stroke widths)
- f11 (nf=12): u32, e.g. `0xFFFFFFFF` (second colour / fill?)
- f14 (nf=15): f32, another dimension (e.g. 1.746)
- f5 (nf=12, nf=15): vector, possibly point coordinates (contents undecoded)

**nf=1 (lightweight element):** Single f0 u32 field with values `0`, `4`, or
a hash-like constant. The wrapper's f5 matches stroke-like table sizes
(32, 36, 44), suggesting a shared envelope. Purpose unknown — possibly an
annotation marker or a degenerate stroke.

Odd vtable field offsets (e.g. 7, 19, 23, 27) are **u8 / bool fields**, not
misaligned u32s. This was confirmed by the field-size gaps between consecutive
offsets.

### 4.3 Stroke table — VERIFIED (two subtypes)

Handwriting strokes are inner tables with `nf=16`. Two subtypes exist,
distinguished by table size and vtable layout:

| | Type A | Type B |
| --- | --- | --- |
| tsize | 44 | 40 |
| vtable fields | `[32,24,0,0,0,23,0,19,12,8,0,0,0,0,0,4]` | `[28,20,0,0,0,0,0,16,12,8,0,0,0,0,0,4]` |
| f9 uoffset | 36 | 32 |
| u16 in points | constant `15360` (`0x3c00`) | varies (~14681–15401) |

Observed distribution (B1): 55 Type A, 506 Type B out of 561 strokes.

Decoded fields (offsets differ per subtype; shown for Type A → Type B):

| field | Type A off | Type B off | type | meaning |
| --- | --- | --- | --- | --- |
| f1 | 24 | 20 | struct 2×f32 | **stroke anchor (Y, X)** — absolute page coords, Y increases downward |
| f8 | 12 | 12 | f32 | **stroke width** in points |
| f9 | 8 | 8 | uoffset | points to the point-data blob at `tp + tsize` |
| f15 | 4 | 4 | u32 | id-like, varies per stroke |

The f1 struct covers 8 bytes (two f32s). For Type A the second f32 sits at
offset 28 (not listed in the vtable — it is part of the struct, not a separate
field). Verified: anchor `(Y=48.97, X=69.18)` for one Cambridge stroke,
`(Y=109.44, X=65.15)` for one Type B stroke. Across Eth1, anchors range
Y 27.8–229.9, X 23.7–153.7 — page-plausible.

**Widths are integer multiples of a base pen width.** Base is `0.7751` (one
notebook uses `0.725`); observed multipliers `1, 2, 4, 5, 10, 15, 30` — matching
Notability's pen/highlighter size presets.

### 4.4 Point-data blob — structure VERIFIED, coordinates UNSOLVED

Each stroke's `f9` points at a blob at `tp + tsize` (verified for 561/568 B1
strokes). The blob layout:

```
offset 0   u32    = 19*n − 5   (also serves as pt0's x, denormal ≈ 0)
offset 4   u8     = 0
offset 5   u8     = n          (point count)
offset 6   u8     = 0
offset 7   u8     = 3          (constant)
offset 8+  n × 19-byte point records
```

Each 19-byte point record:

```
offset  size  meaning
0       4     f32 LE   x
4       4     f32 LE   y
8       4     f32 LE   z
12      2     u16 LE   (Type A: constant 15360 = f16 1.0; Type B: varies)
14      5     trailer, always 00 3c ff 00 00
```

The earlier "7-byte trailer `00 3c 00 3c ff 00 00`" is the u16 (which for Type
A is always `00 3c`) concatenated with the 5-byte trailer. Using the 5-byte
trailer at offset 14 works for **both** subtypes and recovers the full stroke
counts.

Corrected stroke/point totals (all 9 samples, 5-byte-trailer model):

| sample | strokes | total pts | Type A | Type B |
| --- | --- | --- | --- | --- |
| B1 | 561 | 16744 | 55 | 506 |
| Eth1 | 220 | 4035 | 220 | 0 |
| D1 | 1102 | 21229 | 438 | 664 |
| E1 | 1485 | 31430 | 1208 | 277 |
| F1 | 1301 | 20790 | 1301 | 0 |
| G1 | 1790 | 46343 | 1752 | 38 |
| P1 | 631 | 18059 | 631 | 0 |
| Cambridge | 13 | 321 | 13 | 0 |
| M1 | 1 | 54 | 1 | 0 |

#### Coordinate encoding — UNSOLVED

The three f32 values per point are **not** page coordinates and **not** simple
deltas. Observed behaviour:

- Values start near zero, grow **exponentially** (each successive point is
  ~2–10× the previous in magnitude), reaching ~10¹³ in long strokes. The sign
  is negative for most points (sign bit set).
- The u32 reinterpretation of these f32 bit patterns appears monotonically
  increasing — but this is an **artifact**: for negative f32 values, larger
  magnitude ⇒ larger u32 bit pattern. Not fixed-point coordinates.
- `cumsum(x, y)` does **not** reproduce the PDF vector path shape.
- No constant scale factor relates cumsum extents to PDF extents across
  strokes.
- The u16 field is constant 15360 (f16 = 1.0) for Type A and varies ~14681–15401
  for Type B — possibly pressure or a timestamp delta, but unconfirmed.

Hypotheses tried and rejected: fixed-point reinterpretation (u32/2^k for
k=16,20,24,28), log-encoding, sqrt-encoding, velocity × constant-dt integration,
velocity × z-as-dt integration, bfloat16 pairs, running average, trapezoidal
integration, scale-normalized cumsum, z-as-altitude segmentation.

The stroke **anchor** (f1 = Y, X) is confirmed absolute page position; only the
intra-stroke path geometry is unsolved. May require inspecting the Notability
app binary or SDK.

### 4.5 PDF comparison — anchors confirmed, paths not

Matching PDFs were used as an oracle. Findings:

- PDF handwriting is **rasterized** (`/Im1 Do`, 0 vector paths) in Cambridge and
  M1; the other 7 PDFs contain **vector** paths using both `l` (lineto) and `c`
  (cubic Bezier) operators.
- PDF segment counts roughly match .ntb stroke counts (Eth1: 220 strokes vs 230
  PDF paths; B1: 561 vs 3791 after Bezier subdivision).
- PDF paths are drawn with the **identity transform** (page coordinates
  directly); a `cm` scale/flip appears only for embedded images.
- PDF y-axis is bottom-up; .ntb anchor Y is top-down. `pdf_y = pageH − ntb_Y`
  matches (Eth1 pageH = 841.89).
- Matching .ntb anchors to PDF path bbox centers (y-flipped) succeeds for
  ~42% of Eth1 strokes at a 15-point threshold, with residuals of a few points —
  confirming the anchor is the stroke's page position.
- Point-count and bbox-extent comparisons between .ntb and PDF do **not**
  correlate, because the point coordinates themselves are undecoded.

## 5. `index/ios/*.fb` — FlatBuffers, OCR side-car

Filenames track content: `HandwritingIndex` is present in every notebook,
`PDFIndex` when the notebook has PDF pages, `ImageIndex` when it has raster
images. Sizes are highly variable (256 B to 119 KB).

These buffers hold **UTF-8 recognized text** (OCR output) plus page geometry.
They are a derived, lossy side-car — not the source of truth for page content —
but a cheap oracle for what text a page contains.

The buffers are standard FlatBuffers (same walker as noteBundle). Per-field
schemas are not yet decoded.

## 6. Conversion implications

### 6.1 Rendering (`→` SVG/PDF/PNG)

The format is **partially tractable today**: stroke anchors, widths, and counts
are decoded; only the intra-stroke path geometry is missing. The Rust reader
(`src/ntb.rs`) therefore emits each stroke as a 2-point horizontal dash at its
anchor position — a placeholder that preserves page layout but does not
reproduce the ink. Full render fidelity is blocked on the coordinate encoding
(§4.4).

Page geometry: `.ntb` notebooks are continuous-scroll; the matching PDF export
paginates at A4 height (841.89 pt). `ntb::paginate()` splits strokes by Y
coordinate using a caller-supplied page height.

### 6.2 Editable export (`→` `.ntb` or `→` `.goodnotes`)

The format must be *understood*, not merely read, and additionally must be
validated by round-tripping through the real applications. No writer should be
considered correct until it has been opened and re-saved by Notability /
GoodNotes on a device.

## 7. Rust reader — IMPLEMENTED

`src/ntb.rs` provides a schema-less FlatBuffer walker and a `.ntb` container
parser. Verified against all 9 sample notebooks (Notability 16.12.5):

| sample | strokes | point records |
| --- | --- | --- |
| B1 | 561 | 16 744 |
| Cambridge | 13 | 321 |
| D1 | 1 102 | 21 229 |
| E1 | 1 485 | 31 430 |
| Eth1 | 220 | 4 035 |
| F1 | 1 301 | 20 790 |
| G1 | 1 790 | 46 343 |
| M1 | 1 | 54 |
| P1 | 631 | 18 059 |

Public API:

- `ntb::open(path)` → `NtbDocument` (meta + strokes + assets)
- `ntb::parse_bundle_public(bytes)` → same, from an in-memory `noteBundle`
- `ntb::paginate(doc, page_h)` → `Vec<NtbPage>` (strokes split by Y)
- `ntb::to_pages(doc, page_h)` → `Vec<Page>` (portable IR items)

The convert pipeline dispatches `.ntb` files via `is_ntb()`; assets are loaded
into the document attachment map. Example: `cargo run --bin oblyx -- -f svg,pdf
ntb/Eth1.ntb`.

## 9. Open problems

1. **Point coordinate encoding** (4.4) — the blocker for render fidelity. The
   three f32s per point have a velocity-envelope pattern but do not integrate
   to the PDF path. May require inspecting the Notability app binary or SDK.
2. **Text content location** (4.2, nf=8) — font name and size are decoded, but
   the actual text characters are stored elsewhere (not in the element table).
   Possibly in assets/ or a separate string table.
3. **Shape geometry** (4.2, nf=11/12/15) — Y position, colour, and width are
   decoded, but the f5 vectors (point coordinates?) are undecoded.
4. **Colour encoding** — f9 = `0xFF000000` interpreted as RGBA bytes
   `00 00 00 FF` (black). Needs verification with coloured notebooks.
5. **nf=1 purpose** — single u32 field (0, 4, or hash). Wrapper envelope matches
   strokes. Possibly annotations or degenerate strokes.
6. **nf=4, 7, 13** — undecoded.
7. **f15 u32 in strokes** — matches neither point count nor byte length;
   purpose unknown.
8. **u16 in points** — constant f16 1.0 for Type A, varying for Type B; likely
   pressure or timestamp, unconfirmed.
9. **Page-size mismatch** — renderers use GoodNotes page constants (1091×1543);
   `.ntb` coordinates are page-relative (e.g. A4 596×842). Strokes appear in
   the upper portion of the canvas. A proper fix needs a page-size-aware
   renderer or a coordinate scale at load time using nf=2 dimensions.
10. **No device validation** — no Notability/GoodNotes round-trip has been
    performed; any future writer must be validated on a real device.
