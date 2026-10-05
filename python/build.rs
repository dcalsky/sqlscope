fn main() {
    // Lets plain `cargo build` link the extension module on macOS.
    pyo3_build_config::add_extension_module_link_args();
}
