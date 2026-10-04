// Web Worker: owns the oblyx WASM instance so the UI thread stays responsive.
// Plain classic worker — no imports, wasm fetched from the same directory.

let W; // WebAssembly.Instance.exports
let mem;

const dec = new TextDecoder();
const enc = new TextEncoder();

function outBytes() {
  const p = W.oblyx_out_ptr();
  const n = W.oblyx_out_len();
  const bytes = new Uint8Array(mem.buffer, p, n).slice();
  W.oblyx_out_clear();
  return bytes;
}

function outText() {
  return dec.decode(outBytes());
}

// Input-taking exports consume the buffer (no double free).
function withInput(bytes, fn) {
  const p = W.oblyx_alloc(bytes.length);
  new Uint8Array(mem.buffer, p, bytes.length).set(bytes);
  return fn(p, bytes.length);
}

async function init() {
  // Query string is the wasm hash. worker.js revalidates; the wasm file is
  // cached, so a new worker must not keep calling exports the cached module lacks.
  const resp = await fetch("oblyx_web.wasm?v=8443452301c1");
  if (!resp.ok) throw new Error(`wasm download failed: HTTP ${resp.status}`);
  const imports = {
    oblyx: {
      progress(phase, done, total) {
        postMessage({ type: "progress", phase, done, total });
      },
    },
  };
  // Compile while the bytes arrive when the server sends application/wasm.
  // Fall back to a buffered instantiate if the type is wrong or the
  // streaming path rejects (some local preview servers).
  const type = resp.headers.get("content-type") || "";
  let instance;
  if (WebAssembly.instantiateStreaming && /wasm/.test(type)) {
    ({ instance } = await WebAssembly.instantiateStreaming(resp, imports));
  } else {
    ({ instance } = await WebAssembly.instantiate(await resp.arrayBuffer(), imports));
  }
  W = instance.exports;
  mem = W.memory;
  postMessage({ type: "ready" });
}

self.onmessage = (e) => {
  const msg = e.data;
  try {
    switch (msg.type) {
      case "open": {
        const rc = withInput(new Uint8Array(msg.buffer), (p, n) => W.oblyx_open(p, n));
        if (rc !== 0) throw new Error(outText());
        const meta = JSON.parse(outText());
        postMessage({ type: "opened", pages: meta.pages, jobs: meta.jobs });
        break;
      }
      case "open_archive": {
        const rc = withInput(new Uint8Array(msg.buffer), (p, n) => W.oblyx_open_archive(p, n));
        if (rc !== 0) throw new Error(outText());
        const meta = JSON.parse(outText());
        postMessage({ type: "opened_archive", entries: meta.entries });
        break;
      }
      case "open_entry": {
        const rc = W.oblyx_open_entry(msg.index);
        if (rc !== 0) throw new Error(outText());
        const meta = JSON.parse(outText());
        postMessage({ type: "opened", pages: meta.pages, jobs: meta.jobs });
        break;
      }
      case "set_member": {
        const rc = withInput(enc.encode(msg.path), (p, n) => W.oblyx_set_member(p, n));
        if (rc !== 0) throw new Error(outText());
        postMessage({ type: "member_set" });
        break;
      }
      case "open_member": {
        const rc = withInput(new Uint8Array(msg.buffer), (p, n) => W.oblyx_open_member(p, n));
        if (rc !== 0) throw new Error(outText());
        const meta = JSON.parse(outText());
        postMessage({ type: "opened", pages: meta.pages, jobs: meta.jobs });
        break;
      }
      case "begin_archive": {
        const rc = W.oblyx_begin_archive();
        if (rc !== 0) throw new Error(outText());
        postMessage({ type: "begun" });
        break;
      }
      case "pack": {
        const rc = W.oblyx_pack_zip();
        if (rc !== 0) throw new Error(outText());
        const bytes = outBytes();
        postMessage({ type: "done", buffer: bytes.buffer }, [bytes.buffer]);
        break;
      }
      case "attachment": {
        const rc = withInput(enc.encode(msg.uuid), (p, n) => W.oblyx_attachment(p, n));
        if (rc !== 0) throw new Error(outText());
        const bytes = outBytes();
        postMessage({ type: "attachment", uuid: msg.uuid, buffer: bytes.buffer }, [bytes.buffer]);
        break;
      }
      case "raster": {
        const rc = withInput(new Uint8Array(msg.buffer), (p, n) =>
          W.oblyx_set_raster(msg.index, p, n),
        );
        if (rc !== 0) throw new Error(outText());
        postMessage({ type: "rastered" });
        break;
      }
      case "convert": {
        const rc = W.oblyx_convert(msg.fmt);
        if (rc !== 0) throw new Error(outText());
        const bytes = outBytes();
        postMessage({ type: "done", buffer: bytes.buffer }, [bytes.buffer]);
        break;
      }
      default:
        throw new Error(`unknown message ${msg.type}`);
    }
  } catch (err) {
    postMessage({ type: "error", message: err && err.message ? err.message : String(err) });
  }
};

init().catch((err) => postMessage({ type: "error", message: err.message }));
