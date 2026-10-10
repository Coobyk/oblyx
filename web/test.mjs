// Node smoke test for the raw-WASM oblyx build:
//   node test.mjs <oblyx_web.wasm> <file.goodnotes> <pdf|svg> <out-path>
// Prints open metadata and progress lines; writes the converted artifact.
import { readFile, writeFile } from "node:fs/promises";

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
console.log(`pages=${meta.pages}`);

const t0 = Date.now();
const rc2 = E.oblyx_convert(fmt);
if (rc2 !== 0) {
  console.error(`convert failed (${lastProgress}):`, new TextDecoder().decode(out()));
  process.exit(1);
}
const bytes = out();
await writeFile(outPath, bytes);
console.log(`ok: ${fmtArg} ${bytes.length} bytes in ${Date.now() - t0} ms (${lastProgress})`);
