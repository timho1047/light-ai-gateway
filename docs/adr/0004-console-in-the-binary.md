# The console ships inside the binary, without a Node build toolchain

**English** | [简体中文](0004-console-in-the-binary.zh-CN.md)

The install path is `cargo install --path . --locked` producing self-contained
binaries (the gateway and its small launcher), and the repository has no Node
toolchain and no CI. A
Vite/React console would either require Node to build -- breaking that install
claim -- or commit generated artefacts. The console is therefore server-rendered
HTML from Rust plus a small amount of hand-written JavaScript, with CSS, JS and
icons embedded with `include_str!` / `include_bytes!`. A development override
(`LGW_UI_DIR`) serves the assets from disk. There is no CDN and no third-party
script at runtime, and type comes from system font stacks so no font is shipped.
The copy is bilingual through a server-side string catalogue selected by
`?lang=en|zh`; no cookie carries it.

## Considered Options

- A client-side SPA: more comfortable for a large reactive surface, but it moves
  validation and error rendering into a second language, and this is a
  five-screen tool.
- `tower-http` plus `rust-embed` for static serving: two dependencies for what a
  small asset handler does, in a repo that hand-rolls its tables.

## Consequences

- A frontend change needs a Rust rebuild; the development override exists for
  iterating on CSS and copy.
- Charts are hand-drawn SVG, not a charting library.
- Both language catalogues live in Rust beside the handlers and are covered by
  the same integration tests.
