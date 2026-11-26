//! Writes a fake Kobo (the test fixture) to a directory, for trying the CLI
//! without a device:
//!
//!     cargo run -p hoard-kobo --example fake_kobo -- /tmp/fake-kobo
//!     cargo run -p word-hoard -- kobo export --device /tmp/fake-kobo

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = std::env::args().nth(1).ok_or("usage: fake_kobo <directory>")?;
    let root = std::path::Path::new(&root);
    if root.join(".kobo").exists() {
        return Err(format!("{} already contains a .kobo directory", root.display()).into());
    }
    hoard_kobo::fixture::create_device(root)?;
    println!("Fake Kobo written to {}", root.display());
    Ok(())
}
