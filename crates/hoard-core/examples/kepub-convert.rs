//! Converts one EPUB to KEPUB, optionally with a library cover.
//!
//!     cargo run -p hoard-core --example kepub-convert -- <src.epub> <dest.kepub.epub> [cover image]

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cover = args.get(3).map(std::path::PathBuf::from);
    let stats = hoard_core::kobo::kepub::convert_with_cover(
        std::path::Path::new(&args[1]),
        std::path::Path::new(&args[2]),
        cover.as_deref(),
    )
    .expect("conversion failed");
    println!("{} documents, {} spans", stats.documents, stats.spans);
}
