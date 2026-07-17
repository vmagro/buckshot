import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

// Every `vite_bundle` target gets this file automatically (see
// node/vite_bundle.bzl) -- targets can't bring their own.
export default defineConfig({
  plugins: [react()],
  // `buck2 run :name[live]` runs against a symlinked_dir work tree so
  // editing the real, checked-in srcs is what the dev server sees.
  // Without this, Vite realpaths every symlinked file it touches,
  // walking straight back out to the checked-in repo path and losing
  // the assembled node_modules/ sibling.
  resolve: {
    preserveSymlinks: true,
  },
});
