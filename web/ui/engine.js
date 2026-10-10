// Browser-side driver for worker.js. The worker owns the WASM module.
const MB = 1024 * 1024;
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
                    await openedP;
                    await convertCurrent(label);
                }
                else {
                    const openedP = expect("opened");
                    const buf = copyBytes(raw);
                    worker.postMessage({ type: "open", buffer: buf }, [buf]);
                    const meta = await openedP;
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
