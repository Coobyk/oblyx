# oblyx web interface

The browser UI is plain HTML, CSS, and JavaScript. Vite is used only for local
development and to copy the UI into `../www` for Cloudflare Pages.

From this directory:

```sh
npm ci
npm run dev
npm run build
```

To rebuild the full browser app from the repository root, run `./web/build.sh`
and then `npm run build` here. The latter writes the static site to `web/www`.
