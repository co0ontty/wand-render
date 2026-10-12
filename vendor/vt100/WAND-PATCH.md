# Local vt100 patch

Source: crates.io `vt100` 0.16.2, MIT (see LICENSE), registry checksum
`054ff75fb8fa83e609e685106df4faeffdf3a735d3c74ebce97ec557d5d36fd9`
(from the original Cargo.lock).
Upstream: https://github.com/doy/vt100-rust

Only `src/row.rs::Row::resize` differs from upstream source: after resizing,
clear a wide leader in the last column whose continuation has been clipped,
preserving its attributes. This matches `Row::truncate` and repairs both the
normal and alternate grids without modifying the actual PTY byte stream.

Without this invariant, printing or erasing the clipped cell panics in
`screen.rs` / `row.rs`, which previously terminated a Render reader thread.
Regression tests live in `crates/wand-render/tests/vt_resize.rs`.

The manifest/source/license/readme/changelog are otherwise copied from the
published crate. No Cargo cache or downloaded artifact is patched in place.
Remove this override when a reviewed upstream release passes these regressions.
