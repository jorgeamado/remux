fn main() {
    // rust-embed requires web/dist to exist at compile time; create it so a
    // fresh clone compiles before the web client has been built.
    std::fs::create_dir_all("web/dist").ok();
    println!("cargo:rerun-if-changed=web/dist");
}
