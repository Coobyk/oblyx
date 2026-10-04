// Node smoke test for the raw-WASM oblyx build:
//   node test.mjs <oblyx_web.wasm> <file.goodnotes> <pdf|svg> <out-path>
// Prints open metadata and progress lines; writes the converted artifact.
// For PDF notebooks it rasterizes pending jobs with pdftoppm (identical to
// the native pipeline) so outputs can be byte-compared against the CLI.
import { readFile, writeFile, mkdtemp, rm } from "node:fs/promises";
import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { tmpdir } from "node:os";
import { join } from "node:path";

const run = promisify(execFile);
const [wasmPath, inputPath, fmtArg, outPath] = process.argv.slice(2);
if (!wasmPath || !inputPath || !fmtArg || !outPath) {
  console.error("usage: node test.mjs <wasm> <input.goodnotes> <pdf|svg> <out>");
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

const input = new Uint8Array(await readFile(inputPath));
const rc = withInput(input, (p, n) => E.oblyx_open(p, n));
if (rc !== 0) {
  console.error("open failed:", new TextDecoder().decode(out()));
  process.exit(1);
}
const meta = JSON.parse(new TextDecoder().decode(out()));
console.log(`pages=${meta.pages} raster_jobs=${meta.jobs.length}`);

// Inject rasters exactly as the native pipeline would (pdftoppm at 144 dpi).
if (meta.jobs.length > 0) {
  const dir = await mkdtemp(join(tmpdir(), "oblyx-test-"));
  try {
    for (const [i, [att, page]] of meta.jobs.entries()) {
      const pdf = withInput(new TextEncoder().encode(att), (p, n) => {
        const rc = E.oblyx_attachment(p, n);
        if (rc !== 0) throw new Error(new TextDecoder().decode(out()));
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
      if (rc !== 0) throw new Error(new TextDecoder().decode(out()));
    }
    console.log(`injected ${meta.jobs.length} raster(s) via pdftoppm`);
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
}

const t0 = Date.now();
const rc2 = E.oblyx_convert(fmt);
if (rc2 !== 0) {
  console.error(`convert failed (${lastProgress}):`, new TextDecoder().decode(out()));
  process.exit(1);
}
const bytes = out();
await writeFile(outPath, bytes);
console.log(`ok: ${fmtArg} ${bytes.length} bytes in ${Date.now() - t0} ms (${lastProgress})`);
