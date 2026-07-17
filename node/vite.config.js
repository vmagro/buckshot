import { readFileSync, existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";
import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// Every `vite_bundle` target gets this file automatically (see
// node/vite_bundle.bzl) -- targets can't bring their own.

// `vite_bundle`'s `live` sub_target writes a JSON array of top-level
// `node_modules/` package names that are *not* `immutable` (in-tree
// `node_module` deps -- see `JsPackageInfo`'s own doc) next to this
// file, in the work tree it assembles. Missing (as for `build`/`serve`,
// which don't write one -- irrelevant there since `vite build` never
// runs the dev-only optimizer/watcher this feeds) just means no
// packages need the special-casing below.
const configDir = path.dirname(fileURLToPath(import.meta.url));
const manifestPath = path.join(configDir, ".buckshot-live-mutable-deps.json");
const mutableDeps = existsSync(manifestPath)
  ? JSON.parse(readFileSync(manifestPath, "utf8"))
  : [];

export default defineConfig({
  // `buck2 run :name[live]` runs against a symlinked_dir work tree so
  // editing the real, checked-in srcs is what the dev server sees.
  // Without this, Vite realpaths every symlinked file it touches,
  // walking straight back out to the checked-in repo path and losing
  // the assembled node_modules/ sibling.
  resolve: {
    preserveSymlinks: true,
  },
  // A mutable dep's `node_modules/<name>` entry (in `live`'s symlinked
  // work tree) is a real symlink straight back to its checked-in
  // source -- editing it updates the file on disk immediately (that
  // part "just works", no config needed), but the dev server's watcher
  // needs to actually notice it (below) and, since a stale dependency
  // pre-bundle cache could otherwise mask the change, restart on it too
  // (see the `buckshot-mutable-deps-restart` plugin further down).
  server: {
    watch: {
      ignored: mutableDeps.length === 0 ? [] : [
        // Chokidar's directory walk prunes at `node_modules` itself
        // before ever reaching a package inside it: picomatch's `**`
        // treats `**/node_modules/**` as matching the bare
        // `node_modules` path too (not just paths *under* it), so
        // without this the per-package negations below never even get
        // evaluated (confirmed empirically with a debug patch straight
        // into chokidar's `_isIgnored` -- it returned `true` for the
        // bare `.../live_work/node_modules` path, one level above
        // anything package-specific). This alone doesn't un-ignore any
        // package -- `node_modules/<other-pkg>` still matches the
        // default glob and stays pruned -- it just lets the walk
        // continue one level down to where the per-package globs apply.
        "!**/node_modules",
        ...mutableDeps.flatMap((name) => [
          `!**/node_modules/${name}/**`,
          `!**/node_modules/${name}`,
        ]),
      ],
    },
  },
  plugins: [
    react(),
    // Vite pre-bundles every bare-specifier dependency into a cached
    // `.vite/deps/<name>.js` chunk once at startup -- that's what the
    // browser actually imports, and it's what gives a CommonJS package
    // (like an in-tree `node_module` typically is -- plain
    // `module.exports`, no bundler of its own) its ESM interop
    // (a named import works because pre-bundling's esbuild pass wraps
    // it, not because the raw file itself has real `export` statements).
    // A first attempt at this fixed live-reload by excluding mutable
    // deps from that pre-bundling (`optimizeDeps.exclude`) so their raw,
    // always-current source would be used -- but Vite only wires up
    // CJS/ESM interop for files the optimizer actually manages
    // (`depsOptimizer.isOptimizedDepFile`), so an excluded CJS package's
    // raw `module.exports` gets served completely unmodified and the
    // browser's native ESM loader can't find any named export at all
    // (confirmed in a real browser: `Uncaught SyntaxError: ... doesn't
    // provide an export named: 'live_reload_color'`). So: leave
    // pre-bundling on (interop keeps working, exactly like a real npm
    // package), and instead blow away the *whole* dev server -- and
    // with it the pre-bundle cache -- on any change to a mutable dep,
    // forcing a full re-scan against the now-current file content. The
    // injected `@vite/client` already knows to full-reload the page once
    // the WebSocket reconnects after a restart, so nothing else here
    // needs to push that explicitly.
    {
      name: "buckshot-mutable-deps-restart",
      configureServer(server) {
        if (mutableDeps.length === 0) return;
        const dirs = mutableDeps.map((name) =>
          path.join(server.config.root, "node_modules", name) + path.sep,
        );
        server.watcher.on("change", (file) => {
          // A plain `restart()` reuses the existing `.vite/deps` cache
          // wholesale (nothing about the resolved dependency list or
          // config changed, which is all it normally checks) --
          // `forceOptimize` is needed to actually re-bundle against the
          // now-current file content (confirmed empirically: without
          // it, the server restarts but keeps serving the stale
          // pre-bundled chunk from before the edit).
          if (dirs.some((dir) => file.startsWith(dir))) server.restart(true);
        });
      },
    },
  ],
});
