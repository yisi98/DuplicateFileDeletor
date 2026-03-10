# DuplicateFileDeletor

`DuplicateFileDeletor` is now a native Rust desktop app for finding duplicate files, reviewing the deletion plan visually, and applying changes without using command prompts. The app defaults to a safe dry-run workflow and only deletes files when you explicitly choose an apply action.

## What Changed

The primary experience is now a GUI launched with:

```bash
cargo run
```

The desktop app provides:

- Native folder pickers for the scan root, output folder, and preferred keep path
- A review-first workflow with a dedicated dry-run action
- Progress updates while scanning, planning, and deleting
- A modern dashboard with summary cards and an in-app review list
- One-click access to the scanned folder and generated reports
- Reusable checkpoint/output folders so you can dry-run first and apply the same plan later

A CLI is still available as a secondary entry point:

```bash
cargo run --bin cli -- --path C:\Users\you\Pictures
```

## Desktop Workflow

1. Launch the app with `cargo run`.
2. Pick the folder you want to scan.
3. Choose where reports and checkpoints should be stored.
4. Select how duplicates should be kept:
   - oldest created
   - newest modified
   - prefer a specific folder
5. Optionally add include or exclude filters.
6. Click `Plan Safe Dry Run`.
7. Review the planned deletions inside the app.
8. Click `Apply Current Plan` when you are satisfied.

## Features

- Safe by default: dry-run first
- Byte-for-byte verification before deletion
- Resumable checkpoint scanning
- Keep-rule selection for conflict resolution
- CSV outputs for auditability
- Optional CLI fallback for automation

## Output Files

Each run writes into the selected output folder.

Generated files include:

- `all_files.csv`
- `kept_files.csv`
- `planned_deletions.csv`
- `deleted_files.csv`
- `failed_deletions.csv`
- `summary.txt`
- `checkpoints/checkpoint_*.csv`

## Development

Main desktop app:

```bash
cargo run
```

Optional CLI:

```bash
cargo run --bin cli -- --path <directory> [options]
```

Format the code:

```bash
cargo fmt --all
```

Compile-check locally once the Rust desktop linker/toolchain is installed:

```bash
cargo check --all-targets
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
|   |-- bin/
|   |   `-- cli.rs
|   |-- gui.rs
|   |-- lib.rs
|   `-- main.rs
`-- tests/fixtures/
    |-- README.md
    `-- sample-set/
```

## Notes

- The GUI is the primary product surface now.
- The Python scripts in `legacy/` are reference-only.
- The app writes reports to the chosen output folder and can reuse them with resume/apply flows.
- The local environment used for development may still need the MSVC linker (`link.exe`) installed before full Cargo builds succeed on Windows.

## License

This project is licensed under the MIT License. See [LICENSE](/C:/Users/liuyisi/PycharmProjects/DuplicateFileDeletor/LICENSE).
