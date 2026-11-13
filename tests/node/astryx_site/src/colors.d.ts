// Types for the in-tree `colors` `node_module` (see
// `tests/node/node_module/colors`), which ships plain JS with no `.d.ts`
// of its own. Kept in sync with `colors/index.js` by hand.
declare module "colors" {
  export function green(s: string): string;
  export function live_reload_color(): string;
}
