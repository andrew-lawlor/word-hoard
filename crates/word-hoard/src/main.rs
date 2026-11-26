use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use hoard_core::export::{json, markdown};
use hoard_core::{AnnotationKind, BookAnnotations};
use hoard_kobo::{KoboDevice, Snapshot, VersionInfo};

#[derive(Parser)]
#[command(name = "word-hoard", version, about = "Ebook library and annotation manager")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Work with a Kobo e-reader connected over USB (read-only)
    #[command(subcommand)]
    Kobo(KoboCommand),
}

#[derive(Subcommand)]
enum KoboCommand {
    /// List connected Kobos
    Devices,
    /// Show what is on a Kobo: books, highlights, notes
    Info(Source),
    /// Export highlights and notes, one Markdown file per book
    Export(ExportArgs),
    /// Print the database schema (no rows, no serial number) for bug reports
    Report(ReportArgs),
}

#[derive(Args)]
struct Source {
    /// Mount point of the Kobo (default: detect the connected one)
    #[arg(long, value_name = "PATH", conflicts_with = "db")]
    device: Option<PathBuf>,
    /// Read a copied KoboReader.sqlite instead of a device
    #[arg(long, value_name = "FILE")]
    db: Option<PathBuf>,
}

#[derive(Args)]
struct ExportArgs {
    #[command(flatten)]
    source: Source,
    /// Directory to write into (created if missing; existing files with the same names are replaced)
    #[arg(short, long, default_value = "kobo-highlights")]
    out: PathBuf,
    #[arg(long, value_enum, default_value_t = Format::Markdown)]
    format: Format,
    /// Include annotations deleted on the device but still in its database
    #[arg(long)]
    include_hidden: bool,
}

#[derive(Args)]
struct ReportArgs {
    #[command(flatten)]
    source: Source,
    /// Write to a file instead of standard output
    #[arg(short, long, value_name = "FILE")]
    output: Option<PathBuf>,
}

#[derive(Clone, Copy, ValueEnum)]
enum Format {
    /// One Markdown file per book
    Markdown,
    /// A single kobo-annotations.json
    Json,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Kobo(KoboCommand::Devices) => devices(),
        Command::Kobo(KoboCommand::Info(source)) => info(&source),
        Command::Kobo(KoboCommand::Export(args)) => export(&args),
        Command::Kobo(KoboCommand::Report(args)) => report(&args),
    }
}

/// Where to read from, after resolving `--device`, `--db` or auto-detection.
struct Resolved {
    db: PathBuf,
    label: String,
    source_tag: String,
    version: Option<VersionInfo>,
}

fn resolve(source: &Source) -> Result<Resolved> {
    if let Some(db) = &source.db {
        if !db.is_file() {
            bail!("{} is not a file", db.display());
        }
        return Ok(Resolved {
            db: db.clone(),
            label: db.display().to_string(),
            source_tag: "kobo:unknown".into(),
            version: None,
        });
    }
    let device = match &source.device {
        Some(root) => KoboDevice::at(root)?,
        None => {
            let mut found = hoard_kobo::detect();
            match found.len() {
                0 => bail!(
                    "No Kobo found. Connect it over USB and tap Connect on the device, \
                     or pass --device <mount point>."
                ),
                1 => found.remove(0),
                n => bail!(
                    "Found {n} Kobos ({}). Choose one with --device.",
                    found.iter().map(|d| d.root.display().to_string()).collect::<Vec<_>>().join(", ")
                ),
            }
        }
    };
    Ok(Resolved {
        db: device.db_path(),
        label: device.root.display().to_string(),
        source_tag: device.source_tag(),
        version: device.version,
    })
}

fn open(resolved: &Resolved) -> Result<(Snapshot, hoard_kobo::KoboDb)> {
    let snapshot = Snapshot::take(&resolved.db)
        .with_context(|| format!("could not copy {}", resolved.db.display()))?;
    let db = snapshot.open()?;
    Ok((snapshot, db))
}

fn devices() -> Result<()> {
    let found = hoard_kobo::detect();
    if found.is_empty() {
        println!("No Kobo connected.");
    }
    for device in found {
        let v = device.version.as_ref();
        println!(
            "{}  serial {}  firmware {}",
            device.root.display(),
            v.map_or("unknown", |v| v.serial.as_str()),
            v.and_then(|v| v.firmware.as_deref()).unwrap_or("unknown"),
        );
    }
    Ok(())
}

fn info(source: &Source) -> Result<()> {
    let resolved = resolve(source)?;
    let (_snapshot, db) = open(&resolved)?;
    let stats = db.stats()?;
    let v = resolved.version.as_ref();
    println!("Kobo at {}", resolved.label);
    if let Some(v) = v {
        println!("  serial       {}", v.serial);
        println!("  firmware     {}", v.firmware.as_deref().unwrap_or("unknown"));
        println!("  model id     {}", v.model_id.as_deref().unwrap_or("unknown"));
    }
    println!(
        "  database     DbVersion {}",
        db.db_version()?.map_or("unknown".to_string(), |n| n.to_string())
    );
    println!(
        "  books        {} ({} sideloaded, {} store)",
        stats.books, stats.sideloaded, stats.store
    );
    println!(
        "  annotations  {} across {} books",
        counts(stats.highlights, stats.notes, stats.bookmarks),
        stats.annotated_books
    );
    if stats.markups > 0 {
        println!("  markups      {} (handwritten, exported as placeholders)", stats.markups);
    }
    if stats.hidden > 0 {
        println!("  hidden       {} deleted on device (export with --include-hidden)", stats.hidden);
    }
    Ok(())
}

fn export(args: &ExportArgs) -> Result<()> {
    let resolved = resolve(&args.source)?;
    let (_snapshot, db) = open(&resolved)?;
    let books = db.annotations(&resolved.source_tag, args.include_hidden)?;
    fs::create_dir_all(&args.out)
        .with_context(|| format!("could not create {}", args.out.display()))?;

    match args.format {
        Format::Markdown => {
            let opts = markdown::MarkdownOptions {
                exported_on: chrono::Local::now().format("%Y-%m-%d").to_string(),
            };
            let mut used = HashSet::new();
            for entry in &books {
                let name = unique_name(markdown::file_name(&entry.book), &mut used);
                write(&args.out.join(name), &markdown::render_book(entry, &opts))?;
            }
        }
        Format::Json => write(&args.out.join("kobo-annotations.json"), &json::render(&books)?)?,
    }

    let total = |kind| books.iter().map(|b: &BookAnnotations| b.count(kind)).sum::<usize>();
    println!(
        "Exported {} from {} {} to {}",
        counts(
            total(AnnotationKind::Highlight),
            total(AnnotationKind::Note),
            total(AnnotationKind::Bookmark)
        ),
        books.len(),
        plural(books.len(), "book", "books"),
        args.out.display()
    );
    Ok(())
}

fn report(args: &ReportArgs) -> Result<()> {
    let resolved = resolve(&args.source)?;
    let (_snapshot, db) = open(&resolved)?;
    let text = db.schema_report(resolved.version.as_ref())?;
    match &args.output {
        Some(path) => write(path, &text)?,
        None => print!("{text}"),
    }
    Ok(())
}

fn write(path: &Path, contents: &str) -> Result<()> {
    fs::write(path, contents).with_context(|| format!("could not write {}", path.display()))
}

/// Avoids two books overwriting each other's file (case-insensitively, since
/// exports often land on FAT or exFAT drives).
fn unique_name(name: String, used: &mut HashSet<String>) -> String {
    let stem = name.strip_suffix(".md").unwrap_or(&name).to_string();
    let mut candidate = name;
    let mut n = 2;
    while !used.insert(candidate.to_lowercase()) {
        candidate = format!("{stem} ({n}).md");
        n += 1;
    }
    candidate
}

fn counts(highlights: usize, notes: usize, bookmarks: usize) -> String {
    format!(
        "{highlights} {}, {notes} {} and {bookmarks} {}",
        plural(highlights, "highlight", "highlights"),
        plural(notes, "note", "notes"),
        plural(bookmarks, "bookmark", "bookmarks"),
    )
}

fn plural<'a>(n: usize, one: &'a str, many: &'a str) -> &'a str {
    if n == 1 { one } else { many }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_names_ignore_case() {
        let mut used = HashSet::new();
        assert_eq!(unique_name("A - B.md".into(), &mut used), "A - B.md");
        assert_eq!(unique_name("a - b.md".into(), &mut used), "a - b (2).md");
        assert_eq!(unique_name("A - B.md".into(), &mut used), "A - B (3).md");
    }
}
