// Node smoke test for the raw-WASM oblyx zip-archive flow:
//   node test-zip.mjs <oblyx_web.wasm> <batch.zip> <pdf|svg> <out-path.zip>
// Opens the archive, converts every notebook into a single packed zip
// (the web app's "One .zip" mode) and writes it. For PDF notebooks it
// rasterizes pending jobs with pdftoppm, exactly like test.mjs, so the
// result can be byte-compared against a native CLI conversion.
import { readFile, writeFile, mkdtemp, rm } from "node:fs/promises";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { tmpdir } from "node:os";
import { join } from "node:path";

const run = promisify(execFile);
const [wasmPath, zipPath, fmtArg, outPath] = process.argv.slice(2);
if (!wasmPath || !zipPath || !fmtArg || !outPath) {
  console.error("usage: node test-zip.mjs <wasm> <batch.zip> <pdf|svg> <out.zip>");
  process.exit(2);
}
const fmt = { pdf: 0, svg: 1 }[fmtArg];

const wasmBytes = await readFile(wasmPath);
let lastProgress = "";
const { instance } = await WebAssembly.instantiate(wasmBytes, {
  oblyx: {
    progress(phase, done, total) {
      lastProgress = `${phase === 0 ? "decode" : "render"} ${done}/${total}`;
    },
  },
});
const E = instance.exports;
console.log("exports:", Object.keys(E).sort().join(", "));

const out = () => {
  const p = E.oblyx_out_ptr();
  const n = E.oblyx_out_len();
  const bytes = Uint8Array.from(new Uint8Array(E.memory.buffer, p, n));
  E.oblyx_out_clear();
  return bytes;
};
const withInput = (bytes, fn) => {
  const p = E.oblyx_alloc(bytes.length);
  new Uint8Array(E.memory.buffer, p, bytes.length).set(bytes);
  return fn(p, bytes.length);
};
const dec = new TextDecoder();

const input = new Uint8Array(await readFile(zipPath));
const rc = withInput(input, (p, n) => E.oblyx_open_archive(p, n));
if (rc !== 0) {
  console.error("open_archive failed:", dec.decode(out()));
  process.exit(1);
}
const { entries } = JSON.parse(dec.decode(out()));
console.log(`entries=${entries.length}`);
for (const e of entries) console.log(`  ${e.path} -> ${e.sub || "."}/${e.stem}`);

if (E.oblyx_begin_archive() !== 0) {
  console.error("begin_archive failed:", dec.decode(out()));
  process.exit(1);
}

for (const [idx, entry] of entries.entries()) {
  const rcOpen = E.oblyx_open_entry(idx);
  if (rcOpen !== 0) {
    console.error(`open_entry ${idx} failed:`, dec.decode(out()));
    process.exit(1);
  }
  const meta = JSON.parse(dec.decode(out()));
  console.log(`[${idx + 1}/${entries.length}] ${entry.path}: pages=${meta.pages} raster_jobs=${meta.jobs.length}`);

  // Inject rasters exactly as the native pipeline would (pdftoppm at 144 dpi).
  if (meta.jobs.length > 0) {
    const dir = await mkdtemp(join(tmpdir(), "oblyx-zip-test-"));
    try {
      for (const [i, [att, page]] of meta.jobs.entries()) {
        const pdf = withInput(new TextEncoder().encode(att), (p, n) => {
          const rc = E.oblyx_attachment(p, n);
          if (rc !== 0) throw new Error(dec.decode(out()));
          return out();
        });
        const pdfPath = join(dir, `att-${i}.pdf`);
        await writeFile(pdfPath, pdf);
        const base = join(dir, `att-${i}-p${page}`);
        await run("pdftoppm", [
          "-f", String(page), "-l", String(page), "-r", "144",
          "-singlefile", "-jpeg", "-jpegopt", "quality=85", pdfPath, base,
        ]);
        const jpg = new Uint8Array(await readFile(`${base}.jpg`));
        const rc = withInput(jpg, (p, n) => E.oblyx_set_raster(i, p, n));
        if (rc !== 0) throw new Error(dec.decode(out()));
      }
      console.log(`  injected ${meta.jobs.length} raster(s) via pdftoppm`);
    } finally {
      await rm(dir, { recursive: true, force: true });
    }
  }

  const rcConv = E.oblyx_convert(fmt);
  if (rcConv !== 0) {
    console.error(`convert failed (${lastProgress}):`, dec.decode(out()));
    process.exit(1);
  }
  const leftover = E.oblyx_out_len();
  if (leftover !== 0) {
    console.error("convert returned bytes instead of packing:", dec.decode(out()));
    process.exit(1);
  }
}

const rcPack = E.oblyx_pack_zip();
if (rcPack !== 0) {
  console.error("pack failed:", dec.decode(out()));
  process.exit(1);
}
const packed = out();
await writeFile(outPath, packed);
console.log(`ok: ${fmtArg} archive ${packed.length} bytes (${entries.length} notebook(s))`);
