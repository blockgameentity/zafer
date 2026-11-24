//! Generates `windows::Win32`'s D3D12, DXGI and Direct3D namespaces from
//! the metadata `windows-bindgen` carries, for targets the `windows`
//! crate does not build on. The generated code names `windows_core::`,
//! which becomes our `crate::core` (`windows_core` plus what it only has
//! on Windows).

use std::path::PathBuf;

const FILTER: &[&str] = &[
    "Windows.Win32.Graphics.Direct3D",
    "Windows.Win32.Graphics.Direct3D12",
    "Windows.Win32.Graphics.Dxgi",
    "Windows.Win32.Graphics.Dxgi.Common",
    "!Windows.Win32.Graphics.Direct3D.Dxc",
    "!Windows.Win32.Graphics.Direct3D.Fxc",
    "!Windows.Win32.Graphics.Direct3D10",
    "!Windows.Win32.Graphics.Direct3D11",
    "!Windows.Win32.Graphics.Direct3D11on12",
    "!Windows.Win32.Graphics.Direct3D9",
    "!Windows.Win32.Graphics.Direct3D9on12",
    "Windows.Win32.Foundation.E_FAIL",
    "Windows.Win32.Foundation.E_INVALIDARG",
    "Windows.Win32.Foundation.E_NOINTERFACE",
    "Windows.Win32.Foundation.E_NOTIMPL",
    "Windows.Win32.Foundation.HANDLE",
    "Windows.Win32.Foundation.HWND",
    "Windows.Win32.Foundation.RECT",
];

fn main() {
    println!("cargo::rerun-if-changed=build.rs");
    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("bindings.rs");
    let mut args = vec!["--out", out.to_str().unwrap(), "--no-comment", "--filter"];
    args.extend_from_slice(FILTER);
    // What it skips (members of namespaces we leave out) we do not call.
    let _ = windows_bindgen::bindgen(args);
    let text = std::fs::read_to_string(&out).unwrap();
    // Inner attributes cannot be `include!`d; the crate sets its own.
    let body = text.trim_start();
    let body = match body.strip_prefix("#![") {
        Some(rest) => &rest[rest.find(")]").unwrap() + 2..],
        None => body,
    };
    std::fs::write(&out, body.replace("windows_core::", "crate::core::")).unwrap();
}
