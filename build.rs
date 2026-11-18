use std::{env, fs, path::PathBuf};

// Copies the vendored PDFium library next to the built executable.
fn main() {
    println!("cargo:rerun-if-changed=vendor/pdfium/pdfium.dll");
    println!("cargo:rerun-if-changed=assets/icon.ico");
    embed_resource::compile("assets/app.rc", embed_resource::NONE).manifest_optional().unwrap();
    let src = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap()).join("vendor/pdfium/pdfium.dll");
    let out = PathBuf::from(env::var("OUT_DIR").unwrap());
    // OUT_DIR is target/<profile>/build/<pkg>-<hash>/out
    if let Some(profile_dir) = out.ancestors().nth(3) {
        let dst = profile_dir.join("pdfium.dll");
        let stale = match (fs::metadata(&src), fs::metadata(&dst)) {
            (Ok(s), Ok(d)) => s.len() != d.len(),
            _ => true,
        };
        if stale {
            fs::copy(&src, &dst).expect("copy pdfium.dll next to the executable");
        }
    }
}
