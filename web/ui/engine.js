// Browser-side driver for worker.js. The worker owns the WASM module.
const MB = 1024 * 1024;
let pdfjsPromise = null;
function loadPdfJs() {
    if (!pdfjsPromise) {
        const href = "/pdfjs/pdf.min.mjs";
        pdfjsPromise = import(/* @vite-ignore */ href).then((lib) => {
            const pdf = lib;
            pdf.GlobalWorkerOptions.workerSrc = "/pdfjs/pdf.worker.min.mjs";
            return pdf;
        });
    }
    return pdfjsPromise;
}
let worker = null;
let waiter = null;
let onWorkerProgress = null;
let readyPromise = null;
function ensureWorker() {
    if (worker)
        return readyPromise;
    worker = new Worker("/worker.js");
    worker.onmessage = (e) => {
        const m = e.data;
        if (m.type === "progress") {
            onWorkerProgress?.(m);
            return;
        }
        if (!waiter)
            return;
        if (m.type === "error") {
            const w = waiter;
            waiter = null;
            w.reject(new Error(m.message));
        }
        else if (m.type === waiter.type) {
            const w = waiter;
            waiter = null;
            w.resolve(m);
        }
    };
    worker.onerror = (e) => {
        if (waiter) {
            const w = waiter;
            waiter = null;
            w.reject(new Error(e.message || "worker crashed"));
        }
    };
    readyPromise = new Promise((resolve, reject) => {
        const prev = worker.onmessage;
        worker.onmessage = (e) => {
            if (e.data.type === "ready") {
                worker.onmessage = prev;
                resolve();
                return;
            }
            if (e.data.type === "error") {
                reject(new Error(e.data.message));
                return;
            }
            prev?.call(worker, e);
        };
    });
    return readyPromise;
}
const expect = (type) => new Promise((resolve, reject) => {
    waiter = { type, resolve, reject };
});
export function prewarm() {
    ensureWorker();
}
let rasterCanvas = null;
let rasterCtx = null;
const RENDER_TIMEOUT_MS = 180000;
function prepareCanvas(w, h) {
    if (!rasterCanvas) {
        rasterCanvas = document.createElement("canvas");
        rasterCanvas.addEventListener("contextlost", () => {
            rasterCanvas = null;
            rasterCtx = null;
        });
        rasterCtx = rasterCanvas.getContext("2d", { alpha: false });
    }
    if (rasterCanvas.width !== w || rasterCanvas.height !== h) {
        rasterCanvas.width = w;
        rasterCanvas.height = h;
    }
    rasterCtx.fillStyle = "#ffffff";
    rasterCtx.fillRect(0, 0, w, h);
    return rasterCtx;
}
async function renderPdfPage(pdfDoc, pageNum) {
    const page = await pdfDoc.getPage(pageNum);
    const viewport = page.getViewport({ scale: 2 });
    const w = Math.max(1, Math.round(viewport.width));
    const h = Math.max(1, Math.round(viewport.height));
    const ctx = prepareCanvas(w, h);
    let timer = 0;
    try {
        await Promise.race([
            page.render({ canvasContext: ctx, viewport }).promise,
            new Promise((_, reject) => {
                timer = window.setTimeout(() => {
                    rasterCanvas = null;
                    rasterCtx = null;
                    reject(new Error(`rendering page ${pageNum} timed out — try again`));
                }, RENDER_TIMEOUT_MS);
            }),
        ]);
    }
    finally {
        window.clearTimeout(timer);
    }
    const blob = await new Promise((res, rej) => rasterCanvas.toBlob((b) => (b ? res(b) : rej(new Error("JPEG encode failed"))), "image/jpeg", 0.92));
    const bytes = new Uint8Array(await blob.arrayBuffer());
    page.cleanup();
    return bytes;
}
function mimeFor(name) {
    if (/\.pdf$/i.test(name))
        return "application/pdf";
    if (/\.svg$/i.test(name))
        return "image/svg+xml";
    if (/\.zip$/i.test(name))
        return "application/zip";
    return "application/octet-stream";
}
function fileOf(name, buffer) {
    const blob = new Blob([buffer], { type: mimeFor(name) });
    return { name, url: URL.createObjectURL(blob), size: blob.size };
}
function entryOutputName(entry, pages, fmt) {
    const ext = fmt === 0 ? "pdf" : pages <= 1 ? "svg" : "zip";
    const base = entry.sub ? `${entry.sub}/${entry.stem}` : entry.stem;
    return `${base}.${ext}`;
}
export function formatSize(bytes) {
    return `${(bytes / MB).toFixed(1)} MB`;
}
function copyBytes(bytes) {
    return bytes.slice(0);
}
function memberLayout(name) {
    const parts = [];
    for (const part of name.replaceAll("\\", "/").split("/")) {
        if (part === "" || part === ".")
            continue;
        if (part === ".." || part === "__MACOSX" || part.startsWith("."))
            return null;
        parts.push(part);
    }
    const file = parts.pop();
    if (!file || !file.toLowerCase().endsWith(".goodnotes"))
        return null;
    return { sub: parts.join("/"), stem: file.slice(0, -".goodnotes".length) };
}
// `Blob.arrayBuffer()` fails above 2 GB (NotReadableError). Read ranges instead.
async function listZip(blob) {
    const size = blob.size;
    const tailLen = Math.min(size, 22 + 65535);
    const tail = new Uint8Array(await blob.slice(size - tailLen, size).arrayBuffer());
    let eocd = -1;
    for (let i = tail.length - 22; i >= 0; i--) {
        if (tail[i] === 0x50 && tail[i + 1] === 0x4b && tail[i + 2] === 0x05 && tail[i + 3] === 0x06) {
            eocd = i;
            break;
        }
    }
    if (eocd < 0)
        throw new Error("Could not read this zip.");
    const view = new DataView(tail.buffer, tail.byteOffset + eocd, 22);
    const cdSize = view.getUint32(12, true);
    const cdOff = view.getUint32(16, true);
    if (cdOff === 0xffffffff || cdSize === 0xffffffff) {
        throw new Error("This zip uses Zip64, which this page cannot read.");
    }
    const cd = new Uint8Array(await blob.slice(cdOff, cdOff + cdSize).arrayBuffer());
    const entries = [];
    let p = 0;
    const dec = new TextDecoder();
    while (p + 46 <= cd.length) {
        const sig = new DataView(cd.buffer, cd.byteOffset + p, 4).getUint32(0, true);
        if (sig !== 0x02014b50)
            break;
        const dv = new DataView(cd.buffer, cd.byteOffset + p, 46);
        const method = dv.getUint16(10, true);
        const compSize = dv.getUint32(20, true);
        const nameLen = dv.getUint16(28, true);
        const extraLen = dv.getUint16(30, true);
        const commentLen = dv.getUint16(32, true);
        const offset = dv.getUint32(42, true);
        const name = dec.decode(cd.subarray(p + 46, p + 46 + nameLen));
        p += 46 + nameLen + extraLen + commentLen;
        const layout = memberLayout(name);
        if (!layout)
            continue;
        if (method !== 0 && method !== 8)
            throw new Error(`Cannot read ${name}`);
        entries.push({ name, method, compSize, offset, sub: layout.sub, stem: layout.stem });
    }
    entries.sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
    if (!entries.length)
        throw new Error("No .goodnotes files found in this zip.");
    return entries;
}
async function readMember(blob, entry) {
    const head = new DataView(await blob.slice(entry.offset, entry.offset + 30).arrayBuffer());
    if (head.getUint32(0, true) !== 0x04034b50)
        throw new Error(`Bad zip header for ${entry.name}`);
    const nameLen = head.getUint16(26, true);
    const extraLen = head.getUint16(28, true);
    const start = entry.offset + 30 + nameLen + extraLen;
    const data = new Uint8Array(await blob.slice(start, start + entry.compSize).arrayBuffer());
    if (entry.method === 0)
        return data;
    const inflated = new Blob([data]).stream().pipeThrough(new DecompressionStream("deflate-raw"));
    return new Uint8Array(await new Response(inflated).arrayBuffer());
}
function bufferOf(bytes) {
    if (bytes.byteOffset === 0 && bytes.byteLength === bytes.buffer.byteLength) {
        return bytes.buffer;
    }
    return bytes.slice().buffer;
}
export async function convertFile(name, blob, preloaded, fmt, packZip, onProgress) {
    const show = (label) => onProgress({ label, pct: 0 });
    const set = (label, done, total) => {
        const pct = total > 0 ? Math.min(100, Math.round((done / total) * 100)) : 100;
        onProgress({ label, pct });
    };
    await ensureWorker();
    async function renderRasters(meta, prefix) {
        if (!meta.jobs.length)
            return;
        const pdfjsLib = await loadPdfJs();
        const p = prefix ? `${prefix} — ` : "";
        const byAtt = new Map();
        meta.jobs.forEach(([att, page], i) => {
            if (!byAtt.has(att))
                byAtt.set(att, []);
            byAtt.get(att).push({ i, page });
        });
        const total = meta.jobs.length;
        let done = 0;
        set(prefix ? `${p}rendering backgrounds 1/${total}` : `Rendering backgrounds 1/${total}`, 0, total);
        // One pdf.js worker for every background in this notebook. Each document
        // is released without terminating that worker; pdf.js would otherwise
        // start a new worker per file.
        const pdfWorker = new pdfjsLib.PDFWorker();
        try {
            for (const [att, jobs] of byAtt) {
                const attP = expect("attachment");
                worker.postMessage({ type: "attachment", uuid: att });
                const { buffer } = await attP;
                const loading = pdfjsLib.getDocument({
                    data: buffer,
                    worker: pdfWorker,
                    isEvalSupported: false,
                });
                try {
                    const pdfDoc = await loading.promise;
                    for (const { i, page } of jobs) {
                        const jpg = await renderPdfPage(pdfDoc, page);
                        done++;
                        set(prefix ? `${p}rendering backgrounds ${done}/${total}` : `Rendering backgrounds ${done}/${total}`, done, total);
                        const acked = expect("rastered");
                        worker.postMessage({ type: "raster", index: i, buffer: jpg.buffer }, [jpg.buffer]);
                        await acked;
                    }
                }
                finally {
                    loading._worker = null;
                    await loading.destroy?.();
                }
            }
        }
        finally {
            pdfWorker.destroy();
        }
    }
    async function convertCurrent(label) {
        const p = label ? `${label} — ` : "";
        const phaseLabel = {
            0: `${p}decoding pages`,
            1: fmt === 0 ? `${p}writing PDF` : `${p}writing SVGs`,
        };
        show(phaseLabel[0]);
        onWorkerProgress = (m) => set(phaseLabel[m.phase ?? 0] ?? "", m.done ?? 0, m.total ?? 0);
        const doneP = expect("done");
        worker.postMessage({ type: "convert", fmt });
        const { buffer } = await doneP;
        onWorkerProgress = null;
        return buffer;
    }
    try {
        if (/\.zip$/i.test(name)) {
            const entries = await listZip(blob);
            if (packZip) {
                const begun = expect("begun");
                worker.postMessage({ type: "begin_archive" });
                await begun;
            }
            const results = [];
            for (let i = 0; i < entries.length; i++) {
                const entry = entries[i];
                const label = `Notebook ${i + 1}/${entries.length}`;
                show(`${label} — opening…`);
                const raw = bufferOf(await readMember(blob, entry));
                if (packZip) {
                    const named = expect("member_set");
                    worker.postMessage({ type: "set_member", path: entry.name });
                    await named;
                    const openedP = expect("opened");
                    const buf = copyBytes(raw);
                    worker.postMessage({ type: "open_member", buffer: buf }, [buf]);
                    const meta = await openedP;
                    await renderRasters(meta, label);
                    await convertCurrent(label);
                }
                else {
                    const openedP = expect("opened");
                    const buf = copyBytes(raw);
                    worker.postMessage({ type: "open", buffer: buf }, [buf]);
                    const meta = await openedP;
                    await renderRasters(meta, label);
                    const buffer = await convertCurrent(label);
                    results.push(fileOf(entryOutputName(entry, meta.pages ?? 1, fmt), buffer));
                }
            }
            if (packZip) {
                show("Packing archive…");
                const packP = expect("done");
                worker.postMessage({ type: "pack" });
                const { buffer } = await packP;
                set("Packing archive…", 1, 1);
                const outName = `${name.replace(/\.zip$/i, "")}-converted.zip`;
                return [fileOf(outName, buffer)];
            }
            return results;
        }
        show("Opening notebook…");
        const bytes = preloaded ?? (await blob.arrayBuffer());
        const buf = copyBytes(bytes);
        const openedP = expect("opened");
        worker.postMessage({ type: "open", buffer: buf }, [buf]);
        const meta = await openedP;
        await renderRasters(meta, "");
        const buffer = await convertCurrent("");
        const pages = meta.pages ?? 1;
        const single = fmt === 1 && pages <= 1;
        const ext = fmt === 0 ? "pdf" : single ? "svg" : "zip";
        const base = name.replace(/\.goodnotes$/i, "");
        return [fileOf(`${base}.${ext}`, buffer)];
    }
    catch (err) {
        onWorkerProgress = null;
        throw err;
    }
}
