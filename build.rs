fn main() {
    // Tell the linker to use our .def file so the DLL exports have the exact
    // undecorated names that Windows looks for when loading dxgi.dll.
    println!("cargo:rustc-cdylib-link-arg=/DEF:exports.def");
}
