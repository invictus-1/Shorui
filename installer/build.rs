use std::{env, fs, path::PathBuf};

// Packs the release build of Shorui and PDFium into the installer, compressed.
fn main() {
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).parent().unwrap().to_path_buf();
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    let exe = root.join("target/release/shorui.exe");
    let dll = root.join("vendor/pdfium/pdfium.dll");
    println!("cargo:rerun-if-changed={}", exe.display());
    println!("cargo:rerun-if-changed={}", dll.display());
    println!("cargo:rerun-if-changed={}", root.join("assets/icon.ico").display());
    embed_resource::compile(root.join("assets/app.rc"), embed_resource::NONE).manifest_optional().unwrap();

    for (src, name) in [(&exe, "shorui.exe.z"), (&dll, "pdfium.dll.z")] {
        let bytes = fs::read(src).unwrap_or_else(|_| {
            panic!("{} is missing. Build the app first: cargo build --release -p shorui", src.display())
        });
        fs::write(out.join(name), miniz_oxide::deflate::compress_to_vec(&bytes, 7)).unwrap();
    }
}
