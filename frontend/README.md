# Frontend components

The web and Tauri clients share the same embedded assets. React owns the command palette and image dialog through `src/overlays.tsx`; cmdk provides command filtering and keyboard selection, Base UI provides modal lifecycle and focus handling, and Floating UI DOM positions the existing popovers and tooltips. Existing CSS tokens and glass surfaces remain the styling source. The transcript, Native UI protocol, browser overlay coordination and Rust event protocol retain their existing boundaries.

## Development

Use Node 22 or newer. Install locked dependencies with `npm ci`, regenerate the embedded bundle with `npm run build`, then run `npm run check` and `npm test`. The check includes TypeScript and byte-for-byte verification of the generated files. Do not edit `crates/cli/src/serve/assets/ecosystem.js` or `ecosystem.licenses.txt` by hand. These two generated files are checked in so ordinary Rust builds and crate consumers do not require a Node runtime or an npm installation; CI verifies them against their sources and lockfile.

The bundle is a production IIFE with an explicit `window.SunmaoUI` bridge, loaded before the classic application scripts. React owns only `ecosystem-root` and its portals in `palette` / `imv`; legacy scripts must not write into those subtrees. The replay parity harness deliberately skips this DOM renderer and continues testing the event fold. Component tests execute the actual embedded bundle against a DOM implementation; they are not visual or WebView acceptance tests.

All runtime dependency license texts ship in `ecosystem.licenses.txt`, also available through the shared resource route. License fallback provenance is recorded in `licenses/README.md`. The build does not fetch libraries or licenses from a CDN at runtime.
