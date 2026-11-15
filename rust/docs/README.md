# buckshot rust docs

Local runs go through the BXL, CI through the action below — both wrap
the same [`rustdoc.bxl`](./rustdoc.bxl) query.

```sh
# full site, then open it (macOS; on Linux use xdg-open)
buck2 bxl buckshot//rust/docs/rustdoc.bxl:build
open "$(buck2 bxl buckshot//rust/docs/rustdoc.bxl:build 2>/dev/null | head -n 1)/index.html"
# one crate plus its transitive deps
buck2 bxl buckshot//rust/docs/rustdoc.bxl:build -- --targets tests//rust:hello
# one crate only, no deps (fastest iteration)
buck2 bxl buckshot//rust/docs/rustdoc.bxl:build -- --targets tests//rust:hello --include_deps false
# serve over HTTP (rustdoc search needs http, not file://)
buck2 bxl buckshot//rust/docs/rustdoc.bxl:build 2>/dev/null | head -n 1 | xargs -I{} python3 -m http.server -d {}
```

Unlike the clippy and test actions this one is not change-gated: it
takes explicit query patterns and documents whatever they select
(`//...` by default), deps included unless `--include_deps false`
narrows it to just the selected crates. Merging unions each selected
crate's `[doc]` subtarget into one site, regenerates `crates.js`
(unioned `ALL_CRATES`) and a top-level `index.html` crate listing, and
prints the site directory on its first stdout line so callers can
`head -n 1` it. `rust_test`s are excluded (their docs overlap the
library) and `build_script_build` crates are skipped. Full-text search
covers a single deterministic crate (per-crate `search.index`
manifests cannot merge); browsing and intra-doc links work across all
merged crates.

## Action

Builds the site with the BXL above, then always uploads it as a
workflow artifact — plus, on request, as a Pages artifact a downstream
`deploy` job publishes:

```yaml
- uses: vmagro/buckshot/rust/docs@vX.Y.Z
  with:
    targets: |
      buckshot//...
      tests//rust/...
```

Inputs: `targets` (patterns to document, one per line — a YAML block
scalar is the list idiom; default `//...`), `include-deps` (default
`'true'`; `'false'` documents just the selected crates), `buck2`,
`bxl` (merging BXL function; override when the buckshot cell has
another name), `artifact-name` (default `'rustdoc'`), `pages`
(default `'false'`; `'true'` also uploads a Pages artifact).

Outputs: `site` (generated site directory), `artifact` (uploaded
workflow artifact id).

With `pages: 'true'`, add a deploy job and give the workflow
`pages: write` plus `id-token: write`:

```yaml
jobs:
  docs:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: vmagro/buckshot/actions/setup@vX.Y.Z
      - uses: vmagro/buckshot/rust/docs@vX.Y.Z
        with:
          targets: buckshot//...
          pages: 'true'
  deploy:
    needs: docs
    runs-on: ubuntu-latest
    permissions:
      pages: write
      id-token: write
    environment:
      name: github-pages
      url: ${{ steps.deploy.outputs.page_url }}
    steps:
      - id: deploy
        uses: actions/deploy-pages@v4
```

## Layout

Everything docs needs lives here:

- [`action.yml`](./action.yml) — the composite action: runs the BXL
  below, uploads the site as a workflow artifact (always) and as a
  Pages artifact (when `pages: 'true'`).
- [`rustdoc.bxl`](./rustdoc.bxl) — the BXL function (default `bxl:`
  input): resolves the patterns, merges the `[doc]` subtargets, and
  prints the site directory first.
