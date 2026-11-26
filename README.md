# Word Hoard

A GNOME-native ebook library that treats your highlights and notes as first-class. See [SPEC.md](SPEC.md).

## Status: M0, Kobo annotation CLI

Reads a Kobo over USB and exports every highlight, note and bookmark. The device is never written to:
the database is copied to a temporary directory and read from there.

```sh
cargo run -p word-hoard -- kobo devices            # list connected Kobos
cargo run -p word-hoard -- kobo info               # books and annotation counts
cargo run -p word-hoard -- kobo export -o ~/Notes/Kobo   # one Markdown file per book
cargo run -p word-hoard -- kobo export --format json
cargo run -p word-hoard -- kobo report > schema.sql      # schema only, for bug reports
```

Every command auto-detects a connected Kobo, or takes `--device <mount point>` or
`--db <copied KoboReader.sqlite>`.

No Kobo handy? Write a fake one and point the CLI at it:

```sh
cargo run -p hoard-kobo --example fake_kobo -- /tmp/fake-kobo
cargo run -p word-hoard -- kobo export --device /tmp/fake-kobo
```

## Layout

| Crate | Purpose |
| --- | --- |
| `hoard-core` | Domain model (`Annotation`, `BookInfo`) and Markdown/JSON exporters |
| `hoard-kobo` | Kobo detection, database snapshots, schema-tolerant reader |
| `word-hoard` | The `word-hoard` command-line tool |

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
