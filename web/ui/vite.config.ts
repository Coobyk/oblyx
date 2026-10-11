import { createReadStream, existsSync, statSync } from "node:fs"
import { execSync } from "node:child_process"
import { extname, resolve, sep } from "node:path"
import { defineConfig, type Plugin } from "vite"

const www = resolve(import.meta.dirname, "../www")

const runtimeTypes: Record<string, string> = {
  ".js": "text/javascript",
  ".mjs": "text/javascript",
  ".wasm": "application/wasm",
  ".svg": "image/svg+xml",
  ".woff2": "font/woff2",
}

// worker.js and the wasm module stay next to the Pages output.
function serveRuntime(): Plugin {
  return {
    name: "oblyx-runtime",
    configureServer(server) {
      server.middlewares.use((req, res, next) => {
        const url = (req.url ?? "").split("?")[0]
        const allowed =
          url === "/worker.js" ||
          url === "/oblyx_web.wasm" ||
          url === "/favicon.svg" ||
          url.startsWith("/assets/")
        if (!allowed) {
          next()
          return
        }
        const file = resolve(www, url.slice(1))
        if (!file.startsWith(www + sep) || !existsSync(file) || !statSync(file).isFile()) {
          next()
          return
        }
        res.setHeader("Content-Type", runtimeTypes[extname(file)] ?? "application/octet-stream")
        createReadStream(file).pipe(res)
      })
    },
  }
}

function buildStamp(): Plugin {
  let stamp = ""
  return {
    name: "oblyx-build-stamp",
    buildStart() {
      const date = new Date().toISOString().replace("T", " ").slice(0, 19)
      let commit = "dev"
      try {
        commit = execSync("git rev-parse --short HEAD", { cwd: import.meta.dirname })
          .toString()
          .trim()
      } catch {}
      stamp = `${date} UTC · commit ${commit}`
    },
    transformIndexHtml(html) {
      return html.replace(
        "</body>",
        `    <footer class="build-stamp">${stamp}</footer>\n  </body>`,
      )
    },
  }
}

export default defineConfig({
  plugins: [serveRuntime(), buildStamp()],
  build: {
    outDir: "../www",
    emptyOutDir: false,
  },
})
