# Word Hoard — Spec

Exported Nov 26, 2026 from the living doc: https://claude.ai/artifact/ErKeqGr3GQFRBiP8pQtvEJ

## Vision and principles

Word Hoard is a GNOME-native ebook library that treats what you *read and marked* as first-class, not just the files. Calibre is a universal converter with a library attached; Word Hoard is a reading-life manager with a library attached.

**Principles**

- **Linux first, no apologies.** GTK4 + libadwaita, Flatpak, portals, udisks2, D-Bus, XDG dirs. No Qt, no Windows/macOS abstractions in the core, and no Python or other bundled interpreters: one Rust binary on the GNOME runtime.
- **Annotations are data, not exhaust.** Every highlight and note is pulled off the device, anchored to its book, deduplicated and kept forever.
- **Curation lives beside the books.** Collections, notes and syntheses are stored in the same library, linked to the passages they cite.
- **Agents propose, humans dispose.** Every AI change is a reviewable diff; nothing writes to metadata, files or devices without approval.
- **Local first.** Works fully offline, including the assistant on a local model. Cloud providers are opt-in with your own API key, and any MCP client can connect too.
- **Plain, durable storage.** Books stay as normal files in a readable folder tree; the database is SQLite; exports are Markdown.
- **Do fewer things well.** EPUB/KEPUB first. Conversion and reading are delegated (Foliate, KOReader, pandoc) rather than reimplemented.

**Non-goals**

- Not a general format converter (no MOBI/AZW3/PDF→EPUB pipeline in v1).
- Not a reader. Open-in-Foliate is the reading story; a preview pane is the most it gets.
- Not a DRM removal tool. Kobo Store books are detected, listed and annotated, never decrypted.
- Not a content server for v1 (no web UI / OPDS server).

## Scope

v1 is done when a Kobo owner can import their Calibre library, sync books to the Kobo, pull every highlight back, and curate them with an assistant, without touching Calibre again.

| Area | v1 | Later |
| --- | --- | --- |
| Formats | EPUB, KEPUB (generate + read) | PDF, CBZ, MOBI import via external converter |
| Devices | Kobo over USB (all eInk models, firmware 4.x+) | Kobo wireless sync, KOReader, PocketBook, Kindle (via clippings) |
| Library | Add, edit metadata, covers, tags, series, ratings, search | Duplicate merge, smart shelves, multiple libraries |
| Annotations | Extract highlights, notes, dogears from Kobo; Markdown export | Stylus markups (Elipsa/Sage/Libra Colour), Readwise/Obsidian sync |
| Curation | Collections, notes, commonplace book, passage links | Shareable reading journals, publish to static site |
| Agents | Metadata fix, tagging, highlight synthesis, library Q&A, semantic search, MCP server | Long-running background curator |
| Migration | One-way Calibre import | Two-way coexistence |
| Packaging | Flatpak on Flathub | AUR, Nix, distro packages |

## Architecture

A Cargo workspace where the GTK app is a thin client over a headless service layer, so the CLI, tests and the MCP server get every feature for free.

```
 ┌──────────────────────────────┐  ┌────────────────┐  ┌──────────────────────┐
 │ word-hoard-gtk               │  │ word-hoard CLI │  │ MCP server           │
 │ GTK4 + libadwaita, Blueprint │  │ add/sync/export│  │ stdio, same tools    │
 └──────────────┬───────────────┘  └───────┬────────┘  └──────────┬───────────┘
                ▼                          ▼                      ▼
 ┌──────────────────────────────────────────────────────────────────────────────┐
 │ hoard-app services                                                           │
 │ sync planner, Calibre import, export, review queue, job runner on tokio      │
 └──────┬──────────────┬──────────────┬───────────────┬───────────────┬─────────┘
        ▼              ▼              ▼               ▼               ▼
 ┌────────────┐ ┌────────────┐ ┌─────────────┐ ┌─────────────┐ ┌──────────────┐
 │ hoard-core │ │ hoard-epub │ │ hoard-kobo  │ │ hoard-index │ │ hoard-agent  │
 │ model,     │ │ OPF,covers,│ │ DB snapshot,│ │ FTS5 search,│ │ providers,   │
 │ SQLite,    │ │ KEPUB, CFI │ │ diff, writes│ │ embeddings  │ │ tools,       │
 │ migrations │ │            │ │             │ │             │ │ proposals    │
 └─────┬──────┘ └─────┬──────┘ └──────┬──────┘ └──────┬──────┘ └──────┬───────┘
       ▼              ▼               ▼               ▼               ▼
 ┌ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┐ ┌ ─ ─ ─ ─ ─ ─ ┐ ┌ ─ ─ ─ ─ ─ ─ ┐ ┌ ─ ─ ─ ─ ─ ─ ─ ┐
   ~/Books + library.db          Kobo via USB    embeddings.db   LLM providers
   plain files, SQLite (WAL)     udisks2/D-Bus   sqlite-vec      local or API key
 └ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ─ ┘ └ ─ ─ ─ ─ ─ ─ ┘ └ ─ ─ ─ ─ ─ ─ ┘ └ ─ ─ ─ ─ ─ ─ ─ ┘
```

Dashed boxes are external. Every crate below the service layer builds and tests without GTK; `hoard-kobo` is testable against captured `KoboReader.sqlite` fixtures.

**Threading**: the GTK main thread never touches SQLite, disk or network. Services run on a tokio runtime; results return through `async-channel` into `glib::MainContext::spawn_local`. Library state reaches widgets as `gio::ListStore` models of GObject wrappers, so grid and list views stay virtualised.

**Key dependencies**

| Concern | Choice |
| --- | --- |
| UI | gtk4-rs, libadwaita-rs, Blueprint templates (compiled at build time, no Python at runtime), GtkSourceView 5 |
| Async | tokio, async-channel |
| Database | rusqlite (bundled, FTS5), sqlite-vec |
| EPUB | zip, quick-xml, html5ever for KEPUB span insertion |
| D-Bus | zbus (udisks2, search provider) |
| HTTP | reqwest with rustls |
| Secrets | oo7 (Secret portal / libsecret) |
| Templates | minijinja |
| Embeddings | fastembed (ONNX Runtime) |
| MCP | rmcp (official Rust MCP SDK), stdio transport |
| Images | image (cover resize, thumbnailer) |

## Library model and storage

One SQLite database plus a plain folder of books; the database is the index, the files stay the source of truth for content.

**On disk**

```
~/Books/                         # user-chosen library root (default $XDG_DOCUMENTS_DIR/Books)
  Ursula K. Le Guin/
    The Dispossessed (1974)/
      book.epub
      cover.jpg
      book.kepub.epub            # generated, cached, safe to delete
$XDG_DATA_HOME/word-hoard/
  library.db                     # SQLite, WAL mode, FTS5 tables
  embeddings.db                  # sqlite-vec, rebuildable
  backups/kobo/<serial>/<ts>/KoboReader.sqlite
```

**Core entities**

| Entity | Key fields | Notes |
| --- | --- | --- |
| Book | id (UUIDv7), title, sort_title, language, description, published, isbn, identifiers (json), rating, read_status, progress | Logical work; may have several files |
| BookFile | book_id, path, format, sha256, size, mtime, epub_uid | Content hash drives dedupe and device matching |
| Contributor / BookContributor | name, sort_name, role | Author, editor, translator |
| Series | name, index (decimal) | |
| Tag | name, parent_id | Hierarchical tags (Fiction/SF) |
| Annotation | book_id, kind, text, note, color, anchor (json), source, source_id, created, modified, hidden | See Annotations |
| Collection | name, kind (manual, smart, device_shelf), query, sync_to_device | Smart collections store a search query |
| Note | id, title, body (Markdown), created, modified | Curation layer; links to books, annotations, other notes |
| Link | from_ref, to_ref, rel | Generic edge: note→annotation, annotation→annotation, note→book |
| Device / DeviceBook | serial, model, firmware, last_seen / device_id, book_id, content_id, on_device_path | Remembers what lives where |
| AgentRun / Proposal | run_id, provider, model, prompt, cost / run_id, target_ref, diff (json), status | Audit trail for every agent change |

- Full-text search: SQLite FTS5 over metadata, annotation text, notes and (optionally) extracted book text.
- Metadata is written back into the EPUB's OPF on request, never silently.
- Migrations: `refinery` or `sqlx migrate`, forward-only, with an auto backup before each.

**Calibre import** reads `metadata.db` read-only: books, authors, series, tags, ratings, identifiers, custom columns (mapped to tags or json), covers, and `annotations` table if present. Files are copied or hard-linked into the new tree (user choice). Calibre's Kobo plugin state is not imported; Word Hoard re-matches device books by hash and path.

## Kobo support

Kobo is the launch device: plugging one in should show a device pane within two seconds, offer to send books, and pull new annotations without asking twice.

**Detection and identity**

- Watch udisks2 over D-Bus (`zbus`) for a mounted filesystem with label `KOBOeReader` and a `.kobo/version` file.
- Parse `.kobo/version` for serial, firmware version and model id; map model id to a friendly name and capabilities (colour, stylus, screen size).
- Eject = udisks2 `Unmount` + `PowerOff`, surfaced as a header-bar button, so the Kobo starts its import immediately.

**Reading the device (always safe)**

- Copy `.kobo/KoboReader.sqlite` (+ `-wal`) to a temp dir and open read-only; never read the live file in place.
- Check `DbVersion` against a tested range; outside it, read-only mode with a banner.
- `content` rows with `ContentType = 6` are books. Sideloaded ContentIDs are `file:///mnt/onboard/...`; Kobo Store books have UUID ContentIDs and live encrypted in `.kobo/kepub/`.
- Pull per book: `ReadStatus`, `___PercentRead`, `DateLastRead`, `TimeSpentReading`, chapter position.
- `Bookmark` rows give highlights, notes and dogears for both sideloaded *and* store books, so store-book annotations come out without touching DRM.
- `Shelf` / `ShelfContent` give device collections.

**Matching device books to library books**: by path for books Word Hoard sent (recorded in `DeviceBook`), then by EPUB unique identifier, then by content hash, then by fuzzy title+author with a confirm step. Store books with no library match become "device-only" entries that can hold annotations.

**Writing to the device (gated)**

- Send: convert EPUB→KEPUB in Rust (koboSpan sentence wrapping, `book-columns`/`book-inner` wrappers, Kobo-safe CSS fixes; port of kepubify's approach) and copy to a configurable folder, default `Books/<Author>/<Title>.kepub.epub`.
- Metadata is embedded in the KEPUB's OPF before copying; the firmware indexes it on eject. Covers too; firmware generates `.kobo-images` thumbnails itself.
- DB writes limited to shelves, read status and series fields, in one transaction, after a timestamped backup to `backups/kobo/<serial>/`.
- Delete: remove file + mark content/shelf rows deleted the same way the firmware does; annotations stay in the library.

**Sync model**: a sync is a diff shown before anything is written: books to send, books to remove, shelf changes, status changes, annotations to import. One button applies it. Annotations always import automatically; writes need confirmation unless the user ticks "auto-sync this device".

**Later**: Kobo wireless sync by emulating the Kobo store sync API (the approach calibre-web and Komga use), stylus markups from `.kobo/markups/`, and KOReader `.sdr` metadata for people running KOReader on their Kobo.

## Annotations

Annotations are imported, never owned by the device: once pulled, they survive the book being deleted from the Kobo, a factory reset, or a re-sideload.

**Pipeline**

1. **Extract** from the device snapshot: every `Bookmark` row with its text, note, colour, type, timestamps and container paths.
2. **Normalise** into one `Annotation` shape: `kind` (highlight, note, bookmark), `text`, `note`, `color`, `created`, `modified`, `source` (kobo:<serial>), `source_id` (BookmarkID).
3. **Anchor** to the library's EPUB: resolve the Kobo position (`koboSpan` id for KEPUBs, container path + offset for plain EPUBs) to an EPUB CFI plus chapter title and a text-quote selector (exact + 32 chars prefix/suffix, W3C Web Annotation style). The quote selector is the fallback when the file changes.
4. **Merge**: upsert on (`source`, `source_id`); edits on device update the record; deletions on device are recorded as `hidden`, not removed, with a setting to follow device deletions.
5. **Index** into FTS5 and the embedding store.

**Viewing**: an Annotations view per book (chapter-grouped, in reading order, with context shown on hover) and a global stream across the library (newest first, filter by book, tag, colour, date).

**Export**

- Markdown per book with YAML front matter, Obsidian-friendly (`[[Author]]`, block ids per highlight so notes can link back).
- Configurable template (`minijinja`), so people can match their existing vault layout.
- JSON (W3C Web Annotation) for interchange.
- Later: Readwise API push, and writing annotations back into the EPUB for Foliate.

## Curation

Curation is the layer Calibre never had: your own writing and arrangements, stored next to the books and linked to exact passages.

- **Collections**: manual or smart (saved query). Any collection can be marked "sync to device" and becomes a Kobo shelf.
- **Notes**: Markdown documents in the library (stored in SQLite, mirrored to `~/Books/.notes/*.md` for grep and git). A note embeds passages by reference (`![[ann:01J…]]`), so the quote and its source stay live.
- **Commonplace book**: a built-in note type that gathers selected annotations under headings you choose; drag highlights in from any book.
- **Threads**: link two annotations across books with a relation (agrees, contradicts, extends, same idea). Threads show up in each book's annotation view.
- **Reading log**: derived from device stats (started, finished, time spent) plus a one-line verdict per book.

**Where agents help (the curation opportunity)**

| Task | What the agent produces | Human step |
| --- | --- | --- |
| Theme clustering | Proposed collections or threads grouping highlights by idea across books | Accept, rename, discard per group |
| Book digest | A note summarising your highlights for one book, quoting by reference | Edit like any note |
| Connection finding | "This highlight in A echoes B" suggestions as draft threads | Accept or reject each |
| Commonplace drafting | A first-pass commonplace entry on a topic you name | Edit, reorder |
| Gap spotting | Books you highlighted heavily but never wrote a note on; collections with stragglers | A to-do list |

Agent output is always a draft note or a set of proposals, tagged with the run that made it, so nothing generated passes as your own writing.

## Agentic features

The assistant is a tool-using agent over your library whose only write path is a Proposal you approve; it runs on a local model, on an API key you supply, or from any MCP client through Word Hoard's MCP server.

**Providers** (one `Provider` trait: chat with tools and streaming; embeddings optional)

| Provider | Transport | Notes |
| --- | --- | --- |
| Ollama | Native API | Local; auto-discovered on localhost:11434 |
| OpenAI-compatible | `/v1/chat/completions` with tools | Local servers (llama.cpp server, vLLM, LM Studio) and hosted ones (OpenAI, OpenRouter) |
| Anthropic | Messages API, native tool use | Cloud; key in libsecret (Secret portal), never in config files |
| MCP clients | Word Hoard's MCP server | No model configured in Word Hoard; the client brings its own |
| Embeddings | `fastembed` in-process (ONNX Runtime, C++, no Python), or Ollama / provider API | Stored in `sqlite-vec`; fully rebuildable |

Per-task model routing: a small local model for tagging and metadata, a stronger local or cloud model for synthesis. Tasks declare whether they need tool calling, and the app warns when the chosen local model lacks it. Each run shows tokens and estimated cost (zero for local); a per-run budget stops it.

**Tools exposed to the agent**

| Tier | Tools | Default |
| --- | --- | --- |
| Read | `search_library`, `get_book`, `list_annotations`, `search_annotations` (full-text + semantic), `get_note`, `read_excerpt` (bounded chapter text) | On |
| Propose | `propose_metadata`, `propose_tags`, `propose_collection`, `propose_thread`, `draft_note` | On; output lands in the review queue |
| Network | `lookup_metadata` (Open Library, Google Books), `fetch_cover` | Ask once per provider |
| Device | none | Agents never write to devices |

**Privacy controls** per provider: what may leave the machine (metadata only; + annotations; + book text). Local providers default to everything; cloud providers default to metadata + annotations, with book text an explicit opt-in. The setting is shown on every run.

**Built-in tasks** (one-click, each a prompt + tool subset)

- Fix metadata for selected books (title case, series, author sort, ISBN lookup).
- Suggest tags from your existing tag vocabulary.
- Digest my highlights for this book.
- Find connections across my highlights on a topic.
- Ask the library: free-form chat in a sidebar, answers cite annotations and books as clickable chips.

**Review queue**: proposals grouped by run, shown as field-level diffs (old → new), accept/reject per item or per run, fully undoable. `AgentRun` keeps the prompt, model and tool calls for audit.

**MCP server (v1)**: `word-hoard mcp` runs a stdio MCP server built on `rmcp`, the official Rust MCP SDK. It exposes the same read and propose tools as the in-app assistant, plus resources for books, annotations and notes, so Claude Desktop, Claude Code or any MCP client can work with the library.

- Same rules as in-app: propose tools write to the review queue, nothing changes directly, and each MCP session is recorded as an `AgentRun`.
- Works with the app closed: the server opens `library.db` itself (WAL allows both at once); the app picks up new proposals through a D-Bus signal or on next launch.
- One tool registry in `hoard-agent` backs both the assistant and MCP, so a tool is written once.
- Flatpak: clients launch `flatpak run --command=word-hoard <app-id> mcp`; Settings shows a copyable config snippet for Claude Desktop and Claude Code.

## UI and desktop integration

The app should feel like a GNOME Core app: one window, adaptive from phone width to ultrawide, no toolbars full of icons.

**Layout**: `AdwNavigationSplitView` with a sidebar (Library, Reading, Annotations, Notes, Collections, devices as they appear, Review queue with a badge) and a content pane. An `AdwOverlaySplitView` on the right holds the assistant chat, toggled with Ctrl+Shift+A.

**Views**

- **Library**: `GtkGridView` of covers or `GtkColumnView` list, both virtualised for 50k+ books; search bar with field syntax (`author:le guin tag:sf rating:>3`).
- **Book detail**: cover, metadata (edit in place with `AdwEntryRow`s), files, devices it's on, annotations, linked notes, "Open in Foliate" via the OpenURI portal.
- **Annotations**: per-book reading-order list and a library-wide stream; select several to add to a note or collection.
- **Notes**: a Markdown editor (`GtkSourceView 5`) with passage embeds rendered inline as quote cards.
- **Device**: storage gauge, sync diff, "Eject" button; toasts for imports ("42 new highlights from Kobo Libra Colour").
- **Review queue**: diff cards with accept/reject.

**Linux integration**

| Integration | Mechanism |
| --- | --- |
| Packaging | Flatpak on Flathub, GNOME runtime; filesystem access to library root only, udisks via D-Bus permission |
| Device events | udisks2 D-Bus; background portal to keep a small monitor alive for auto-import |
| Secrets | libsecret via Secret portal |
| Shell search | GNOME Shell search provider (books, annotations) over D-Bus |
| Thumbnails | Ship a freedesktop thumbnailer for `.epub`/`.kepub.epub` covers in Nautilus |
| Files | Drag-and-drop from Files, FileChooser portal, "Open with Word Hoard" for EPUBs |
| Notifications | GNotification for finished syncs and agent runs |
| CLI | `word-hoard` binary with `add`, `export`, `sync`, `search` for scripting |

**Accessibility and polish**: keyboard-first (every action in the shortcuts window), screen-reader labels on covers, respects system accent colour, dark style and font scaling, translations through Damned Lies/gettext.

## Roadmap

Build the annotation path end to end first; it is the feature nobody else does well and it proves the Kobo layer.

1. **M0 — Kobo annotation CLI.** `hoard-kobo` + `hoard-core` + a CLI that snapshots a plugged-in Kobo and exports every highlight to Markdown. Gate: correct output on 3 real devices / firmware versions, store and sideloaded books.
2. **M1 — Library core.** SQLite schema, EPUB metadata read/write, Calibre import, FTS search. Gate: imports a 2k-book Calibre library in under 60 s with no lost metadata.
3. **M2 — GTK shell.** Library grid/list, book detail, annotations views, device pane with read-only sync. Gate: usable daily as a Kobo highlight browser.
4. **M3 — Sending books.** KEPUB conversion, send/remove, shelves sync, DB backup and writes. Gate: round-trips 500 books with zero firmware re-import loops.
5. **M4 — Curation.** Notes, commonplace book, threads, collections→shelves, Markdown mirror.
6. **M5 — Agents.** Providers (Ollama, OpenAI-compatible, Anthropic), embeddings and semantic search, read/propose tools, review queue, built-in tasks, MCP server.
7. **M6 — 1.0.** Flathub, search provider, thumbnailer, translations, docs.

## Decisions

The open questions are settled with these defaults; each can be revisited without changing the architecture. The assistant runs on local models, API keys or MCP clients, and search is full-text plus semantic; all of it is v1.

| Question | Decision | Why |
| --- | --- | --- |
| UI framework | Plain gtk4-rs + libadwaita-rs with composite templates in Blueprint | Matches GNOME docs and Workbench. blueprint-compiler is a Python tool but runs only at build time; the Flatpak ships compiled `.ui` XML and no Python. Falling back to hand-written `.ui` XML is a mechanical swap if even a build-time Python is unwanted. |
| Notes storage | SQLite is the source of truth; a one-way Markdown mirror in `~/Books/.notes/` | Passage embeds and links need stable ids and transactions. Two-way vault sync (file watching via `notify`) is a post-1.0 feature. |
| KEPUB conversion | Native Rust port in `hoard-epub` from M3; no kepubify binary bundled | Keeps one language and no extra binaries. kepubify (MIT) is used only in CI as a reference to compare output. |
| Kobo write support | Writes allowed only for `DbVersion` values verified on the M0 test devices; anything else is read-only with a banner | Reading is always safe; writes are where firmware breaks things. Testers can run `word-hoard kobo report` to share a schema-only dump (table definitions + `DbVersion`, no rows) to widen the range. |
| Agent access to book text | Off by default; `read_excerpt` returns at most one chapter or about 8k tokens per call, found through full-text and semantic search | Bounds cost and context, especially for local models; book text stays on the machine unless the per-provider privacy setting allows it. |
| Python anywhere | None at runtime | Calibre import reads `metadata.db` with rusqlite; embeddings use fastembed (ONNX Runtime, a C++ library); secrets use oo7; MCP uses rmcp. If ONNX Runtime proves awkward in Flatpak, swap to candle (pure Rust). |

- [ ] Before M6: check that "Word Hoard" is free on Flathub and crates.io, and pick the app ID.
