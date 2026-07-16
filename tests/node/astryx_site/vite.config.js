import react from "@vitejs/plugin-react";
import { defineConfig } from "vite";

export default defineConfig({
  plugins: [react()],
  // `buck2 run :astryx_site[live]` runs against a symlinked_dir work
  // tree (see node/vite_bundle.bzl) so editing the real, checked-in
  // srcs is what the dev server sees. Without this, Vite realpaths
  // every symlinked file it touches, walking straight back out to the
  // checked-in repo path and losing the assembled node_modules/ sibling.
  resolve: {
    preserveSymlinks: true,
  },
});
