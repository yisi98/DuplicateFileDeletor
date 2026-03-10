# DuplicateFileDeletor

`DuplicateFileDeletor` is a Rust CLI for scanning a directory tree, identifying duplicate files by content, and producing reviewable deletion plans. It now defaults to a safe dry-run workflow and only removes files when you explicitly pass `--apply`.

The repo also keeps two older Python prototypes in [legacy/main.py](/C:/Users/liuyisi/PycharmProjects/DuplicateFileDeletor/legacy/main.py) and [legacy/parallel_main.py](/C:/Users/liuyisi/PycharmProjects/DuplicateFileDeletor/legacy/parallel_main.py), but the Rust binary is the maintained path.

## Highlights

- Safe by default: dry-run unless `--apply` is provided
- Resume-friendly checkpoints stored under an output directory
- Byte-for-byte verification before any deletion, even after hash matching
- Configurable keep rules: `oldest-created`, `newest-modified`, `prefer-path`
- Repeatable include/exclude path filters
- CSV reports for scanned files, kept files, and planned or completed deletions
- Unit tests, CI, `.gitignore`, license, and sample fixture data

## How It Works

1. Walk the target directory recursively.
2. Skip anything filtered out by `--include` or `--exclude`.
3. Hash file contents with `xxh3`.
4. Save progress in checkpoint CSV batches.
5. Group candidates by file size and hash.
6. Verify candidate duplicates byte-for-byte.
7. Keep one file per verified duplicate set according to the selected keep rule.
8. Write reports and optionally delete the remaining duplicates.

## Safety Model

The CLI now uses a two-step workflow:

- Dry-run: creates reports only
- Apply: deletes files listed in the planned deletion set

That means the recommended flow is:

```bash
cargo run -- --path C:\Users\you\Pictures
cargo run -- --path C:\Users\you\Pictures --output-dir output\20260310_120000 --resume --apply
```

The first command generates a deletion plan. The second resumes the same run and applies it after review.

## Installation

You need a Rust toolchain with Cargo installed.

```bash
cargo build
```

## Usage

```bash
cargo run -- --path <directory> [options]
```

You can also pass the directory positionally:

```bash
cargo run -- C:\Users\you\Pictures
```

### Common Options

- `--apply`: actually delete duplicate files
- `--dry-run`: plan only; this is the default
- `--output-dir <dir>`: where reports and checkpoints are written
- `--resume`: continue a previous run from checkpoint data in `--output-dir`
- `--keep <rule>`: `oldest-created`, `newest-modified`, or `prefer-path`
- `--prefer-path <dir>`: preferred directory when `--keep prefer-path` is used
- `--include <text>`: only scan paths containing this text; repeatable
- `--exclude <text>`: skip paths containing this text; repeatable
- `--help`: print CLI help

### Examples

Dry-run a folder:

```bash
cargo run -- --path C:\Users\you\Pictures
```

Delete duplicates and keep the newest modified copy:

```bash
cargo run -- --path C:\Users\you\Pictures --apply --keep newest-modified
```

Prefer copies inside a specific folder and exclude cache paths:

```bash
cargo run -- --path C:\Users\you\Photos --keep prefer-path --prefer-path C:\Users\you\Photos\Archive --exclude cache --exclude tmp
```

Resume a prior run from an explicit output directory:

```bash
cargo run -- --path C:\Users\you\Photos --output-dir output\photos-run --resume
```

## Output Files

Each run writes its artifacts into the chosen output directory. By default, that is a timestamped folder under `output/`.

Generated files include:

- `all_files.csv`: every successfully scanned file
- `kept_files.csv`: the files selected to keep
- `planned_deletions.csv`: duplicate files that would be deleted
- `deleted_files.csv`: files actually deleted during `--apply`
- `failed_deletions.csv`: deletions that failed during `--apply`
- `summary.txt`: human-readable run summary
- `checkpoints/checkpoint_*.csv`: resumable scan batches

## Keep Rules

- `oldest-created`: keep the file with the oldest creation timestamp
- `newest-modified`: keep the file with the newest modification timestamp
- `prefer-path`: keep files under a preferred directory first, then break ties by oldest creation time

## Development

Format the code:

```bash
cargo fmt --all
```

Lint the code:

```bash
cargo clippy --all-targets --all-features -- -D warnings
```

Run tests:

```bash
cargo test --all-targets --all-features
```

## Project Layout

```text
.
|-- .github/workflows/ci.yml
|-- Cargo.lock
|-- Cargo.toml
|-- LICENSE
|-- README.md
|-- analysis/
|   `-- analysis_first_run.ipynb
|-- legacy/
|   |-- README.md
|   |-- main.py
|   `-- parallel_main.py
|-- src/
|   |-- lib.rs
|   `-- main.rs
`-- tests/fixtures/
    |-- README.md
    `-- sample-set/
```

## Notes

- The Rust binary is the primary implementation.
- The Python scripts are legacy prototypes and are not wired into CI.
- If your output directory already exists and is not empty, the CLI will stop unless you pass `--resume`.
- The current include/exclude filters use simple case-insensitive substring matching.

## License

This project is licensed under the MIT License. See [LICENSE](/C:/Users/liuyisi/PycharmProjects/DuplicateFileDeletor/LICENSE).
