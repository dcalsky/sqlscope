fn main() {
    // Deep SQL recurses deeply in the parser and there is no stack growth on
    // wasm; reserve a generous stack.
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("wasm32") {
        println!("cargo:rustc-link-arg=-zstack-size=16777216");
    }
}
