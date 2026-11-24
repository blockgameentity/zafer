fn main() {
    println!("cargo:rustc-link-arg-cdylib=/ENTRY:DllMain");
    println!("cargo:rustc-link-arg-cdylib=/NODEFAULTLIB");
    // It has nothing to relocate, so it needs a base of its own.
    println!("cargo:rustc-link-arg-cdylib=/BASE:0x190000000");
}
