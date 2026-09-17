// `src/server/ui.rs` embeds `ui/dist` with `include_dir!`, which does not
// register the directory as a compiler input. Without this, `cargo build`
// treats the `otto` binary as fresh after `ui/dist` changes and keeps
// serving whatever was embedded at the last incidental recompile.
fn main() {
    println!("cargo:rerun-if-changed=../../ui/dist");
}
