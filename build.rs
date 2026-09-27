fn main() {
    // Embed the app icon (😴, Noto Emoji, Apache 2.0) into the exe.
    println!("cargo:rerun-if-changed=assets/windoze.rc");
    println!("cargo:rerun-if-changed=assets/windoze.ico");
    embed_resource::compile("assets/windoze.rc", embed_resource::NONE)
        .manifest_optional()
        .expect("failed to embed icon resource");
}
