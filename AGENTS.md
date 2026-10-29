# Project development

- Use mise for development toolchains and commands. Rust is pinned in `mise.toml`.
- Install the project toolchain with `mise install rust`; do not install tools globally.
- Use `mise run test`, `mise run check`, and `mise run build`.
- Manage Rust crates with `mise exec -- cargo ... --manifest-path sorter/Cargo.toml` and commit `sorter/Cargo.lock` when dependencies change.
- Never test against or modify real `uploads/`, `music/`, or `audiobooks/` data. Tests must use temporary directories.
- Preserve verified, no-clobber transfers and conservative handling of incomplete uploads and malformed metadata.
