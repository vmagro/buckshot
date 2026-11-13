// Side-effect CSS imports (`import "./index.css"`) are handled by Vite;
// this keeps `tsc --noEmit` (run by `vite_bundle` before the build) from
// rejecting them.
declare module "*.css";
