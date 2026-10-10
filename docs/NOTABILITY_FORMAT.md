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
| 16 | 220 | **handwriting stroke** (§4.3) | f1 anchor, f7 colour, f8 width, f9 points |
| 12 | 19 | **shape / drawing** (§4.2) | f1 anchor, f5 vector, f9 colour, f10 width, f11 fill? |
| 4 | 16 | unknown | f1 u32 small, f3 u8 |
| 1 | 13 | lightweight element / annotation | f0 u32 (0, 4, or hash) |
| 15 | 7 | **shape / drawing** (§4.2) | f1 anchor, f5 vector, f7 colour, f8 width |
| 7 | 3 | unknown | f0 vector n=1, f5 u8, f6 u32 |
| 8 | 1 | **style record** — font "Inter", size 14 | f4 string (font), f5 f32 (size) |
| 2 | 1 | **page definition** | f1 vector: [width, height, …] in points |
| 13 | 1 | unknown | f0 vector n=1, f12 vector n=2 |
| 11 | 1 | **shape / drawing** (§4.2) | f1 anchor, f5 vector, f9/f10 colour |

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

**nf=8 (style record):** f4 is a FlatBuffer string holding a font family
(e.g. `"Inter"`), f5 an f32 size (e.g. `14.0`), f3 a u8 flag (bold?).
Elem 0 is the *document-level* default style; the same table also appears
per pen. **No typed text exists in any of the 9 samples** — the actual
text characters (if any) are stored elsewhere (see open problem 2).

**nf=11 / 12 / 15 (shapes) — DECODED:**

- f1: struct 2×f32 `(X, Y)` — absolute page anchor, same layout as strokes.
- f5: uoffset → vector of f32s holding the geometry (table below).
- f4: u8 subtype (1 and 3 observed; 1 ↔ line-like vectors, 3 ↔ closed).
- colour: layout-driven (§4.6) — nf=15 → f7; nf=11 → f9 (f10 when f9 is
  not a colour); nf=12 → f9 or f7. Never a field scan — f0/f4 hold noise
  u32s with a valid-looking alpha byte.
- width: first plausible f32 (0.01..100) among f8/f9/f10 that is not the
  colour (e.g. 0.7751, 1.163, 1.550, 3.100 — the same base-width presets
  as strokes).
- f11 (nf=12 only): second colour, likely fill — left unused for now
  (fill renders transparent); semantics unconfirmed.

| vector len | dims | classification |
| --- | --- | --- |
| 8 | `(v0, v1)` | near-equal → ellipse; else → rectangle |
| 12 | `(v0, v1)` | line (2-pt stroke from anchor to anchor+dims) |
| 14 | header at v0; dims `(v1, v2)` | line |

Position convention (top-left vs centre of bbox) is **not** confirmed —
see open problem 3. Element counts: B1 4, D1 2, E1 4, Eth1 2, F1 3,
G1 4, M1 1, P1 4, Cambridge 0 (24 shapes total).

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
| f1 | 24 | 20 | struct 2×f32 | **stroke anchor (X, Y)** — absolute page coords, X horizontal, Y vertical (top-down). Thumbnail-overlay verified: plotting `(f32[0], f32[1)` tracks the handwriting exactly; the opposite order scatters over empty margin/grid. |
| f7 | 19 | 16 | u32 | **stroke colour** `0xAABBGGRR` (§4.6); f5 is noise (low byte always `0x01`) |
| f8 | 12 | 12 | f32 | **stroke width** in points |
| f9 | 8 | 8 | uoffset | points to the point-data blob at `tp + tsize` |
| f15 | 4 | 4 | u32 | id-like, varies per stroke |

The f1 struct covers 8 bytes (two f32s). For Type A the second f32 sits at
offset 28 (not listed in the vtable — it is part of the struct, not a separate
field). Verified by thumbnail overlay (dots at `(f32[0], f32[1)`, page-scaled,
land exactly on the ink; the swapped order scatters over empty margin/grid):
`(X=48.97, Y=69.18)` for one Cambridge stroke, `(X=109.44, Y=65.15)` for one
Type B stroke. Across Eth1 strokes, X ranges 27.8–229.9, Y 23.7–153.7 —
page-plausible (handwriting sits in the upper-left of the imported page).
Note: this corrects an earlier reading that had the fields as `(Y, X)`; that
order made anchors exceed the page width on portrait notebooks (e.g. G1 X
up to 839.8 > 595) and put pagination splits on the wrong axis.

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

The stroke **anchor** (f1 = X, Y) is confirmed absolute page position by
thumbnail overlay; only the intra-stroke path geometry is unsolved. May
require inspecting the Notability app binary or SDK.

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
- The decisive anchor check is the **thumbnail overlay** (§4.3): page-scaled
  dots at `(f32[0], f32[1)` land on the ink in Eth1/G1/E1; the swapped order
  scatters over empty margin/grid. An earlier PDF-bbox matching pass that
  reported ~42% for Eth1 was computed under the swapped reading and is
  superseded by the overlay evidence.
- Point-count and bbox-extent comparisons between .ntb and PDF do **not**
  correlate, because the point coordinates themselves are undecoded.
- The 24 decoded shapes could **not** be isolated as distinct PDF segments:
  under every transform tried, 0/4 sampled shapes matched a PDF line segment.
  Shapes are mostly template/annotation marks over imported-PDF thumbnails,
  so they may not export as vector ink at all.

### 4.6 Colour encoding — VERIFIED

A colour is a single u32 whose on-disk byte order is **R, G, B, A**, so it
reads as `0xAABBGGRR` in little-endian:

- Proof: ntb `0xff1515b5` appears where the matching PDF draws `#b51515`
  (E1 ×1, G1 ×3, P1 ×1); the reversed interpretation `#1515b5` never occurs
  in any of the 9 PDFs.
- **Alpha byte**: `0xFF` = opaque ink. `0x6B` (107 ≈ 42 %) = **highlighter**
  — PDF-confirmed: `0x6bff6f00`→`#006fff`, `0x6bff9cb0`→`#b09cff`,
  `0x6b00ffff`→`#ffff00`, `0x6bdec714`→`#14c7de`, `0x6b17ffa9`→`#a9ff17`,
  `0x6b0fc4f1`→`#f1c40f`.
- **Strokes**: colour is always at **f7** (both subtypes). f5 holds
  noise-like u32s whose low byte is always `0x01` (`0x8d41ac01`, …) — do
  not mistake them for colours by scanning fields.
- **Shapes**: colour at f7/f9/f10 depending on nf (§4.2) — a field scan
  false-positives on f0/f4 noise (`0xfffedc01`).
- `rgba_from_u32()` in `src/ntb.rs` performs the conversion; the reader
  accepts alpha `0xFF` and `0x6B` and rejects other patterns.

Example stroke colour histograms (after the f7 fix; `0x6b…` = highlighter):

| sample | top colours |
| --- | --- |
| E1 | black 1361, `#b09cff` 32, `#b51515` 21, `#622d90` 15, `#14c7de` 14, `#006fff` 13, `#a9ff17` 11, `#f1c40f` 9 |
| G1 | black 1704, `#b09cff` 23, `#b51515` 18, `#ed3624` 17, white 17, `#1749b3` 10 |
| P1 | black 619, `#ffff00` 6, `#8c54d0` 3, `#f5bdb6` 2, `#f1c40f` 1 |
| B1 | black 557, `#14c7de` 3, `#b09cff` 1 |

### 4.7 Rejected nf=11/12 elements — NOT DECODED (intentionally skipped)

Many nf=11/12 elements share the shape layout (f1 anchor and f9/f10
colour/width are readable in-table) but their f5 vector's **first slot is a
negative i32**, not a count. For every such element,
`landing + |i32|` points at a **shared table** — most rejected elements in a
notebook converge on one of a handful of such tables (B1: 34 of 40 → a single
table whose inline f32 is `36.0`, matching B1's 36 pt grid paper). The dims
readable around those tables form repeating 18/36-multiple grid patterns.

Counts: B1 40, D1 10, E1 181, G1 121, P1 108. Colours include red and
purple in E1 (`0xff902d62`, `0xff1515b5`) and grays. Thumbnails show the
affected notebooks are grid-paper pages (B1), imported PDF worksheets
(D1), and template pages with ruled tables (E1) — the shared-table +
grid-spacing evidence is consistent with **page-grid/table marks of imports
rather than user shapes**.

The reader **rejects** these elements (vector count ∉ {8,12,14}). Semantics
unverified — do not emit them until decoded. See open problem 5.

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
(§4.4). Shapes fare better: ellipse/rect/line classification from the f5 vector
(§4.2) is decoded and emitted as portable-IR shapes with colour + width.

Page geometry: `.ntb` notebooks are continuous-scroll; the matching PDF export
paginates at the paper height from the first nf=2 page definition (A4 841.89 pt
when absent). `ntb::paginate()` splits strokes/shapes by Y using that height.

Page size is carried on the portable IR (`Page.width/height`). `.ntb` pages
use their nf=2 dimensions (fallback A4 595.28×841.89); GoodNotes pages keep
the historical 1091.35×1543.5 canvas. Renderers honour per-page size: SVG/PNG
use it for the canvas; the PDF renderer scales page width onto a fixed A4
MediaBox (`595.28/page.width` — 0.5455 for GoodNotes, ≈1.0 for `.ntb`), which
fixes the old bug where `.ntb` coordinates were double-shrunk into the upper
quarter of the A4 page.

### 6.2 Editable export (`→` `.ntb` or `→` `.goodnotes`)

The format must be *understood*, not merely read, and additionally must be
validated by round-tripping through the real applications. No writer should be
considered correct until it has been opened and re-saved by Notability /
GoodNotes on a device.

## 7. Rust reader — IMPLEMENTED

`src/ntb.rs` provides a schema-less FlatBuffer walker and a `.ntb` container
parser. Verified against all 9 sample notebooks (Notability 16.12.5):

| sample | strokes | point records | shapes |
| --- | --- | --- | --- |
| B1 | 561 | 16 744 | 4 |
| Cambridge | 13 | 321 | 0 |
| D1 | 1 102 | 21 229 | 2 |
| E1 | 1 485 | 31 430 | 4 |
| Eth1 | 220 | 4 035 | 2 |
| F1 | 1 301 | 20 790 | 3 |
| G1 | 1 790 | 46 343 | 4 |
| M1 | 1 | 54 | 1 |
| P1 | 631 | 18 059 | 4 |

Public API:

- `ntb::open(path)` → `NtbDocument` (meta + strokes + shapes + assets)
- `ntb::parse_bundle_public(bytes)` → same, from an in-memory `noteBundle`
- `ntb::paginate(doc, page_h)` → `Vec<NtbPage>` (strokes + shapes split by Y)
- `ntb::to_pages(doc, page_h)` → `Vec<Page>` (portable IR items with true
  page dimensions; shapes → ellipse/rect/line, strokes coloured via
  `rgba_from_u32` with alpha)

Anchor fields are `(X, Y)` (thumbnail-overlay verified; see §4.3). Renders of
Eth1/G1 place dash anchors on the handwriting lines of the matching
thumbnail.

The convert pipeline dispatches `.ntb` files via `is_ntb()`; assets are loaded
into the document attachment map. Example: `cargo run --bin oblyx -- -f svg,pdf
ntb/Eth1.ntb`.

## 9. Open problems

1. **Point coordinate encoding** (4.4) — the blocker for render fidelity. The
   three f32s per point have a velocity-envelope pattern but do not integrate
   to the PDF path. May require inspecting the Notability app binary or SDK.
2. **Typed text** (4.2, nf=8) — no user-typed text exists in any of the 9
   samples; noteBundles contain only UUIDs, MIME types, and asset-title
   fragments. If the feature exists, its storage is undiscovered.
3. **Shape position convention** (4.2) — anchor (X, Y) and dims are decoded
   and render plausibly on the thumbnail, but top-left vs centre placement
   is unconfirmed (0/4 sampled shapes matched PDF line segments).
4. **nf=12 f11 fill colour** (4.2) — second colour likely means fill; kept
   transparent until semantics are confirmed against a rendered shape.
5. **Rejected nf=11/12 elements** (4.7) — negative-i32 header resolves to a
   shared table (grid spacing 36.0 in B1); consistent with import grid/table
   marks. Decode fully before emitting.
6. **nf=1 purpose** — single u32 field (0, 4, or hash). Wrapper envelope matches
   strokes. Possibly annotations or degenerate strokes.
7. **nf=4, 7, 13** — undecoded.
8. **f15 u32 in strokes** — matches neither point count nor byte length;
   purpose unknown.
9. **u16 in points** — constant f16 1.0 for Type A, varying for Type B; likely
   pressure or timestamp, unconfirmed.
10. **Mixed paper sizes** — B1/F1 carry non-A4 first page defs (651×407,
    455×571); pagination uses the first def's height for all slices. Cumulative
    per-def pagination is unimplemented (samples keep all ink within one or
    two slices either way).
11. **No device validation** — no Notability/GoodNotes round-trip has been
    performed; any future writer must be validated on a real device.
