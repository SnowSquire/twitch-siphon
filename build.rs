// Cargo can miss manifest-only edits (e.g. a version bump) when deciding
// freshness, leaving a stale version baked into the binary. Watching the
// manifest forces a rebuild on those edits; `src` keeps the default file
// watching that a bare `rerun-if-changed` would otherwise replace.
fn main() {
    println!("cargo::rerun-if-changed=Cargo.toml");
    println!("cargo::rerun-if-changed=src");
    slint_build::compile("ui/siphon.slint").expect("slint UI compiles");
}
