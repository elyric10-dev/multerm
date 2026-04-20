---
name: Project renamed to Multerm
description: The project was renamed from termite to multerm — all crate names, directories, and identifiers updated
type: project
---

Project renamed from **termite** → **multerm** (reason: crates.io / GitHub naming conflict with existing termite terminal emulator).

Crate directories:
- `termite-app` → `multerm-app`
- `termite-core` → `multerm-core`
- `termite-vt` → `multerm-vt`
- `termite-render` → `multerm-render`
- `termite-input` → `multerm-input`
- `termite-ui` → `multerm-ui`

Binary names: `termite` → `multerm`, `termite-ui` → `multerm-ui`

Source file renamed: `termite_ui.rs` → `multerm_ui.rs`

Rust identifiers: `termite_vt::`, `termite_core::`, etc. → `multerm_vt::`, `multerm_core::`, etc.

**Why:** Original name conflicted with an existing terminal project on GitHub/crates.io with the same name and same purpose.

**How to apply:** Always use `multerm` in new code, crate references, and file names. The workspace root directory is still named `termite/` on disk but all crates use the multerm name.
