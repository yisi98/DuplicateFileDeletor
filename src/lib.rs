use anyhow::{anyhow, bail, Context, Result};
use chrono::Local;
use csv::{Reader, WriterBuilder};
use log::warn;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use walkdir::WalkDir;
use xxhash_rust::xxh3::Xxh3;

pub const OUTPUT_ROOT_DIR: &str = "output";
pub const CHECKPOINT_DIR: &str = "checkpoints";
pub const ALL_FILES_CSV: &str = "all_files.csv";
pub const KEPT_FILES_CSV: &str = "kept_files.csv";
pub const PLANNED_DELETIONS_CSV: &str = "planned_deletions.csv";
pub const DELETED_FILES_CSV: &str = "deleted_files.csv";
pub const FAILED_DELETIONS_CSV: &str = "failed_deletions.csv";
pub const SUMMARY_TXT: &str = "summary.txt";
pub const DEFAULT_CHECKPOINT_INTERVAL: usize = 500;
pub const SAMPLE_FINGERPRINT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileInfo {
    pub file_path: String,
    pub file_size: u64,
    pub xxhash: String,
    pub created_time: i64,
    pub modified_time: i64,
    pub file_name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FailedDeletionRecord {
    pub file_path: String,
    pub error: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperationMode {
    DryRun,
    Apply,
}

impl OperationMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DryRun => "dry-run",
            Self::Apply => "apply",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeepRule {
    OldestCreated,
    NewestModified,
    PreferPath(PathBuf),
}

impl KeepRule {
    pub fn cli_value(&self) -> &'static str {
        match self {
            Self::OldestCreated => "oldest-created",
            Self::NewestModified => "newest-modified",
            Self::PreferPath(_) => "prefer-path",
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::OldestCreated => "Oldest created".to_string(),
            Self::NewestModified => "Newest modified".to_string(),
            Self::PreferPath(path) => format!("Prefer path ({})", path.display()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RunConfig {
    pub root_dir: PathBuf,
    pub output_dir: PathBuf,
    pub mode: OperationMode,
    pub keep_rule: KeepRule,
    pub fast_prefilter: bool,
    pub include_filters: Vec<String>,
    pub exclude_filters: Vec<String>,
    pub follow_symlinks: bool,
    pub use_trash: bool,
    pub resume: bool,
    pub checkpoint_interval: usize,
}

#[derive(Debug, Clone)]
pub struct RunSummary {
    pub mode: OperationMode,
    pub root_dir: PathBuf,
    pub output_dir: PathBuf,
    pub keep_rule: String,
    pub discovered_files: usize,
    pub scanned_files: usize,
    pub kept_files: usize,
    pub planned_deletions: usize,
    pub deleted_files: usize,
    pub failed_deletions: usize,
    pub duplicate_sets: usize,
}

#[derive(Debug, Clone)]
pub struct RunArtifacts {
    pub summary: RunSummary,
    pub all_files: Vec<FileInfo>,
    pub kept_files: Vec<FileInfo>,
    pub planned_deletions: Vec<FileInfo>,
    pub deleted_files: Vec<FileInfo>,
    pub failed_deletions: Vec<FailedDeletionRecord>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStage {
    Preparing,
    Discovering,
    Scanning,
    Hashing,
    Planning,
    Deleting,
    Saving,
    Complete,
}

#[derive(Debug, Clone)]
pub struct ProgressUpdate {
    pub stage: RunStage,
    pub message: String,
    pub discovered_files: Option<usize>,
    pub processed_files: usize,
    pub planned_deletions: usize,
    pub deleted_files: usize,
}

#[derive(Debug, Default)]
struct LoadedCheckpoints {
    files: Vec<FileInfo>,
    next_batch_index: usize,
}

#[derive(Debug)]
struct DedupPlan {
    kept: Vec<FileInfo>,
    planned_deletions: Vec<FileInfo>,
    duplicate_sets: usize,
}

#[derive(Debug)]
struct DeletionOutcome {
    deleted: Vec<FileInfo>,
    failed: Vec<FailedDeletionRecord>,
}

impl RunConfig {
    pub fn from_args<I>(args: I) -> Result<Self>
    where
        I: IntoIterator<Item = String>,
    {
        let mut iter = args.into_iter();
        let binary = iter
            .next()
            .unwrap_or_else(|| "duplicate-file-deletor-cli".to_string());
        let mut root_dir: Option<PathBuf> = None;
        let mut output_dir: Option<PathBuf> = None;
        let mut output_dir_explicit = false;
        let mut mode = OperationMode::DryRun;
        let mut keep_name = "oldest-created".to_string();
        let mut prefer_path: Option<PathBuf> = None;
        let mut fast_prefilter = true;
        let mut include_filters = Vec::new();
        let mut exclude_filters = Vec::new();
        let mut follow_symlinks = false;
        let mut use_trash = false;
        let mut resume = false;

        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--path" => {
                    root_dir = Some(PathBuf::from(next_value(&mut iter, "--path", &binary)?))
                }
                "--output-dir" => {
                    output_dir = Some(PathBuf::from(next_value(
                        &mut iter,
                        "--output-dir",
                        &binary,
                    )?));
                    output_dir_explicit = true;
                }
                "--keep" => keep_name = next_value(&mut iter, "--keep", &binary)?,
                "--prefer-path" => {
                    prefer_path = Some(PathBuf::from(next_value(
                        &mut iter,
                        "--prefer-path",
                        &binary,
                    )?))
                }
                "--hash-all-files" => fast_prefilter = false,
                "--include" => include_filters.push(next_value(&mut iter, "--include", &binary)?),
                "--exclude" => exclude_filters.push(next_value(&mut iter, "--exclude", &binary)?),
                "--apply" => mode = OperationMode::Apply,
                "--dry-run" => mode = OperationMode::DryRun,
                "--resume" => resume = true,
                "--follow-symlinks" => follow_symlinks = true,
                "--use-trash" => use_trash = true,
                value if value.starts_with("--path=") => {
                    root_dir = Some(PathBuf::from(split_flag(value)))
                }
                value if value.starts_with("--output-dir=") => {
                    output_dir = Some(PathBuf::from(split_flag(value)));
                    output_dir_explicit = true;
                }
                value if value.starts_with("--keep=") => keep_name = split_flag(value),
                value if value.starts_with("--prefer-path=") => {
                    prefer_path = Some(PathBuf::from(split_flag(value)))
                }
                "--fast-prefilter" => fast_prefilter = true,
                value if value.starts_with("--include=") => include_filters.push(split_flag(value)),
                value if value.starts_with("--exclude=") => exclude_filters.push(split_flag(value)),
                value if value.starts_with('-') => {
                    bail!("Unknown flag: {value}\n\n{}", usage(&binary))
                }
                value => {
                    if root_dir.is_some() {
                        bail!(
                            "Unexpected positional argument: {value}\n\n{}",
                            usage(&binary)
                        );
                    }
                    root_dir = Some(PathBuf::from(value));
                }
            }
        }

        let root_dir = absolutize_path(
            root_dir.ok_or_else(|| anyhow!("Missing root directory\n\n{}", usage(&binary)))?,
        )?;
        if resume && !output_dir_explicit {
            bail!("--resume requires an explicit --output-dir");
        }
        let keep_rule = parse_keep_rule(&keep_name, prefer_path)?;
        let output_dir = match output_dir {
            Some(path) => absolutize_path(path)?,
            None => suggested_output_dir()?,
        };

        let config = Self {
            root_dir,
            output_dir,
            mode,
            keep_rule,
            fast_prefilter,
            include_filters,
            exclude_filters,
            follow_symlinks,
            use_trash,
            resume,
            checkpoint_interval: DEFAULT_CHECKPOINT_INTERVAL,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.root_dir.as_os_str().is_empty() {
            bail!("Root directory is required");
        }
        if self.output_dir.as_os_str().is_empty() {
            bail!("Output directory is required");
        }
        if !self.root_dir.exists() {
            bail!("Root directory does not exist: {}", self.root_dir.display());
        }
        if !self.root_dir.is_dir() {
            bail!("Root path is not a directory: {}", self.root_dir.display());
        }
        if self.checkpoint_interval == 0 {
            bail!("checkpoint_interval must be greater than 0");
        }
        if matches!(self.keep_rule, KeepRule::PreferPath(_)) {
            let KeepRule::PreferPath(ref path) = self.keep_rule else {
                unreachable!()
            };
            if path.as_os_str().is_empty() {
                bail!("Preferred path is required for the selected keep rule");
            }
            if !path.exists() {
                bail!("Preferred path does not exist: {}", path.display());
            }
        }
        Ok(())
    }
}

pub fn suggested_output_dir() -> Result<PathBuf> {
    Ok(std::env::current_dir()?
        .join(OUTPUT_ROOT_DIR)
        .join(Local::now().format("%Y%m%d_%H%M%S").to_string()))
}

pub fn usage(binary: &str) -> String {
    format!("Usage:\n  {binary} --path <directory> [options]\n  {binary} <directory> [options]\n\nOptions:\n  --apply                    Delete duplicate files after review\n  --dry-run                  Plan only (default)\n  --output-dir <dir>         Folder for reports and checkpoints\n  --resume                   Resume from an existing output directory\n  --keep <rule>              oldest-created | newest-modified | prefer-path\n  --prefer-path <dir>        Preferred directory for --keep prefer-path\n  --hash-all-files           Disable fast prefilter and hash every file\n  --include <text>           Only scan paths containing this text\n  --exclude <text>           Skip paths containing this text\n  --follow-symlinks          Follow symbolic links during traversal\n  --use-trash                Move deleted files to the system trash instead of permanent deletion\n  -h, --help                 Show help")
}

pub fn run(config: &RunConfig) -> Result<RunArtifacts> {
    run_with_progress(config, |_| {})
}

pub fn run_with_progress<F>(config: &RunConfig, mut emit: F) -> Result<RunArtifacts>
where
    F: FnMut(ProgressUpdate),
{
    let mut config = config.clone();
    config.root_dir = absolutize_path(config.root_dir)?;
    config.output_dir = absolutize_path(config.output_dir)?;
    if let KeepRule::PreferPath(path) = &config.keep_rule {
        config.keep_rule = KeepRule::PreferPath(absolutize_path(path.clone())?);
    }
    config.validate()?;
    emit(progress(
        RunStage::Preparing,
        "Preparing output directory",
        None,
        0,
        0,
        0,
    ));
    prepare_output_dir(&config)?;

    emit(progress(
        RunStage::Discovering,
        "Discovering files",
        None,
        0,
        0,
        0,
    ));
    let candidates = discover_candidate_files(&config)?;
    emit(progress(
        RunStage::Scanning,
        format!("Found {} candidate files", candidates.len()),
        Some(candidates.len()),
        0,
        0,
        0,
    ));

    let all_files = scan_files(&config, candidates, &mut emit)?;
    emit(progress(
        RunStage::Planning,
        "Verifying duplicates and building plan",
        Some(all_files.len()),
        all_files.len(),
        0,
        0,
    ));
    let plan = plan_deletions(&all_files, &config.keep_rule)?;

    emit(progress(
        RunStage::Saving,
        "Writing scan reports",
        Some(all_files.len()),
        all_files.len(),
        plan.planned_deletions.len(),
        0,
    ));
    save_file_infos(&all_files, &config.output_dir.join(ALL_FILES_CSV))?;
    save_file_infos(&plan.kept, &config.output_dir.join(KEPT_FILES_CSV))?;
    save_file_infos(
        &plan.planned_deletions,
        &config.output_dir.join(PLANNED_DELETIONS_CSV),
    )?;

    let deletion = match config.mode {
        OperationMode::DryRun => DeletionOutcome {
            deleted: Vec::new(),
            failed: Vec::new(),
        },
        OperationMode::Apply => {
            execute_deletions(&plan.planned_deletions, config.use_trash, &mut emit)?
        }
    };

    if config.mode == OperationMode::Apply {
        emit(progress(
            RunStage::Saving,
            "Writing deletion results",
            Some(all_files.len()),
            all_files.len(),
            plan.planned_deletions.len(),
            deletion.deleted.len(),
        ));
        save_file_infos(
            &deletion.deleted,
            &config.output_dir.join(DELETED_FILES_CSV),
        )?;
        save_failed_deletions(
            &deletion.failed,
            &config.output_dir.join(FAILED_DELETIONS_CSV),
        )?;
    }

    let summary = RunSummary {
        mode: config.mode,
        root_dir: config.root_dir.clone(),
        output_dir: config.output_dir.clone(),
        keep_rule: config.keep_rule.label(),
        discovered_files: all_files.len(),
        scanned_files: all_files.len(),
        kept_files: plan.kept.len(),
        planned_deletions: plan.planned_deletions.len(),
        deleted_files: deletion.deleted.len(),
        failed_deletions: deletion.failed.len(),
        duplicate_sets: plan.duplicate_sets,
    };
    save_summary(&summary, &config.output_dir.join(SUMMARY_TXT))?;
    emit(progress(
        RunStage::Complete,
        "Run complete",
        Some(summary.discovered_files),
        summary.scanned_files,
        summary.planned_deletions,
        summary.deleted_files,
    ));

    Ok(RunArtifacts {
        summary,
        all_files,
        kept_files: plan.kept,
        planned_deletions: plan.planned_deletions,
        deleted_files: deletion.deleted,
        failed_deletions: deletion.failed,
    })
}

fn prepare_output_dir(config: &RunConfig) -> Result<()> {
    if config.output_dir.exists() {
        if !config.resume {
            let mut entries = fs::read_dir(&config.output_dir)?;
            if entries.next().transpose()?.is_some() {
                bail!(
                    "Output directory already exists and is not empty: {}. Choose a new folder or enable resume.",
                    config.output_dir.display()
                );
            }
        }
    } else {
        fs::create_dir_all(&config.output_dir)?;
    }
    fs::create_dir_all(config.output_dir.join(CHECKPOINT_DIR))?;
    Ok(())
}

fn discover_candidate_files(config: &RunConfig) -> Result<Vec<PathBuf>> {
    let output_dir = canonicalize_or_original(&config.output_dir)?;
    let mut files = Vec::new();
    for entry in WalkDir::new(&config.root_dir)
        .sort_by_file_name()
        .follow_links(config.follow_symlinks)
        .into_iter()
        .filter_entry(|entry| !is_output_path(entry.path(), &output_dir))
        .filter_map(|entry| entry.ok())
    {
        let path = entry.path();
        if !entry.file_type().is_file() || is_output_path(path, &output_dir) {
            continue;
        }
        if is_system_protected_path(path) {
            continue;
        }
        if !path_matches_filters(path, &config.include_filters, &config.exclude_filters) {
            continue;
        }
        files.push(path.to_path_buf());
    }
    Ok(files)
}

fn scan_files<F>(
    config: &RunConfig,
    candidates: Vec<PathBuf>,
    emit: &mut F,
) -> Result<Vec<FileInfo>>
where
    F: FnMut(ProgressUpdate),
{
    let LoadedCheckpoints {
        mut files,
        mut next_batch_index,
    } = if config.resume {
        load_saved_files(&config.output_dir)?
    } else {
        LoadedCheckpoints::default()
    };

    let already_scanned: HashSet<String> =
        files.iter().map(|file| file.file_path.clone()).collect();
    let remaining: Vec<PathBuf> = candidates
        .into_iter()
        .filter(|path| !already_scanned.contains(&path_to_string(path)))
        .collect();

    if !already_scanned.is_empty() {
        emit(progress(
            RunStage::Scanning,
            format!(
                "Loaded {} files from saved scan data",
                already_scanned.len()
            ),
            Some(already_scanned.len() + remaining.len()),
            already_scanned.len(),
            0,
            0,
        ));
    }

    for chunk in remaining.chunks(config.checkpoint_interval) {
        let mut chunk_results: Vec<FileInfo> = chunk
            .par_iter()
            .filter_map(|path| match process_file_metadata(path) {
                Ok(file) => Some(file),
                Err(error) => {
                    warn!("Failed to process {}: {}", path.display(), error);
                    None
                }
            })
            .collect();
        chunk_results.sort_by(|left, right| left.file_path.cmp(&right.file_path));
        if chunk_results.is_empty() {
            continue;
        }
        save_checkpoint_batch(&chunk_results, next_batch_index, &config.output_dir)?;
        next_batch_index += 1;
        files.extend(chunk_results);
        emit(progress(
            RunStage::Scanning,
            format!(
                "Indexed {} of {} files",
                files.len(),
                files.len() + remaining.len().saturating_sub(files.len())
            ),
            Some(already_scanned.len() + remaining.len()),
            files.len(),
            0,
            0,
        ));
    }

    populate_hashes(config, &mut files, emit)?;
    files.sort_by(|left, right| left.file_path.cmp(&right.file_path));
    Ok(files)
}

fn load_saved_files(output_dir: &Path) -> Result<LoadedCheckpoints> {
    let all_files_path = output_dir.join(ALL_FILES_CSV);
    if all_files_path.exists() {
        let files = load_file_infos(&all_files_path)?;
        return Ok(LoadedCheckpoints {
            files,
            next_batch_index: next_checkpoint_index(output_dir)?,
        });
    }

    load_checkpoints(output_dir)
}

fn load_checkpoints(output_dir: &Path) -> Result<LoadedCheckpoints> {
    let checkpoint_dir = output_dir.join(CHECKPOINT_DIR);
    if !checkpoint_dir.exists() {
        return Ok(LoadedCheckpoints::default());
    }

    let mut files = Vec::new();
    let mut seen = HashSet::new();
    let mut checkpoints = Vec::new();
    for entry in fs::read_dir(&checkpoint_dir)? {
        let entry = entry?;
        let path = entry.path();
        if let Some(index) = checkpoint_index(&path) {
            checkpoints.push((index, path));
        }
    }
    checkpoints.sort_by_key(|(index, _)| *index);

    for (index, path) in checkpoints {
        let _ = index;
        let mut reader = Reader::from_path(path)?;
        for record in reader.deserialize() {
            let file: FileInfo = record?;
            if seen.insert(file.file_path.clone()) {
                files.push(file);
            }
        }
    }

    Ok(LoadedCheckpoints {
        files,
        next_batch_index: next_checkpoint_index(output_dir)?,
    })
}

fn next_checkpoint_index(output_dir: &Path) -> Result<usize> {
    let checkpoint_dir = output_dir.join(CHECKPOINT_DIR);
    if !checkpoint_dir.exists() {
        return Ok(0);
    }

    let mut next_batch_index = 0;
    for entry in fs::read_dir(&checkpoint_dir)? {
        let entry = entry?;
        if let Some(index) = checkpoint_index(&entry.path()) {
            next_batch_index = next_batch_index.max(index + 1);
        }
    }
    Ok(next_batch_index)
}

fn load_file_infos(path: &Path) -> Result<Vec<FileInfo>> {
    let mut files = Vec::new();
    let mut reader = Reader::from_path(path)?;
    for record in reader.deserialize() {
        files.push(record?);
    }
    Ok(files)
}

fn process_file_metadata(path: &Path) -> Result<FileInfo> {
    let metadata = fs::metadata(path)?;
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow!("Invalid file name"))?
        .to_string();
    let created_time = system_time_to_unix(metadata.created().or_else(|_| metadata.modified())?)?;
    let modified_time = system_time_to_unix(metadata.modified().or_else(|_| metadata.created())?)?;

    Ok(FileInfo {
        file_path: path_to_string(path),
        file_size: metadata.len(),
        xxhash: String::new(),
        created_time,
        modified_time,
        file_name,
    })
}

fn populate_hashes<F>(config: &RunConfig, files: &mut [FileInfo], emit: &mut F) -> Result<()>
where
    F: FnMut(ProgressUpdate),
{
    if !config.fast_prefilter {
        let hash_targets = files
            .iter()
            .enumerate()
            .filter_map(|(index, file)| file.xxhash.is_empty().then_some(index))
            .collect::<Vec<_>>();
        return apply_full_hashes(
            files,
            &hash_targets,
            "Hashing all indexed files",
            config,
            emit,
        );
    }

    let size_groups = collect_same_size_groups(files);
    let sample_targets = size_groups
        .values()
        .flat_map(|indexes| indexes.iter().copied())
        .filter(|index| files[*index].xxhash.is_empty())
        .collect::<Vec<_>>();
    if sample_targets.is_empty() {
        return Ok(());
    }

    emit(progress(
        RunStage::Hashing,
        "Fingerprinting same-size candidates",
        Some(sample_targets.len()),
        0,
        0,
        0,
    ));

    let mut fingerprints = Vec::with_capacity(sample_targets.len());
    let mut fingerprinted = 0usize;
    for chunk in sample_targets.chunks(config.checkpoint_interval) {
        let chunk_results: Vec<(usize, Option<String>)> = chunk
            .par_iter()
            .map(|index| {
                let file = &files[*index];
                match get_sample_fingerprint(Path::new(&file.file_path), file.file_size) {
                    Ok(fingerprint) => (*index, Some(fingerprint)),
                    Err(error) => {
                        warn!("Failed to fingerprint {}: {}", file.file_path, error);
                        (*index, None)
                    }
                }
            })
            .collect();

        fingerprints.extend(chunk_results);
        fingerprinted += chunk.len();
        emit(progress(
            RunStage::Hashing,
            format!(
                "Fingerprinted {} of {} same-size candidates",
                fingerprinted,
                sample_targets.len()
            ),
            Some(sample_targets.len()),
            fingerprinted,
            0,
            0,
        ));
    }

    let full_hash_targets = collect_full_hash_targets(files, fingerprints);
    apply_full_hashes(
        files,
        &full_hash_targets,
        "Hashing fingerprint matches",
        config,
        emit,
    )
}

fn collect_same_size_groups(files: &[FileInfo]) -> HashMap<u64, Vec<usize>> {
    let mut groups: HashMap<u64, Vec<usize>> = HashMap::new();
    for (index, file) in files.iter().enumerate() {
        if file.xxhash.is_empty() {
            groups.entry(file.file_size).or_default().push(index);
        }
    }
    groups.retain(|_, indexes| indexes.len() > 1);
    groups
}

fn collect_full_hash_targets(
    files: &[FileInfo],
    fingerprints: Vec<(usize, Option<String>)>,
) -> Vec<usize> {
    let mut fingerprint_groups: HashMap<(u64, String), Vec<usize>> = HashMap::new();
    for (index, fingerprint) in fingerprints {
        let Some(fingerprint) = fingerprint else {
            continue;
        };
        fingerprint_groups
            .entry((files[index].file_size, fingerprint))
            .or_default()
            .push(index);
    }

    let mut targets = Vec::new();
    for indexes in fingerprint_groups.into_values() {
        if indexes.len() < 2 {
            continue;
        }
        targets.extend(indexes);
    }
    targets.sort_unstable();
    targets
}

fn apply_full_hashes<F>(
    files: &mut [FileInfo],
    hash_targets: &[usize],
    initial_message: &str,
    config: &RunConfig,
    emit: &mut F,
) -> Result<()>
where
    F: FnMut(ProgressUpdate),
{
    if hash_targets.is_empty() {
        return Ok(());
    }

    emit(progress(
        RunStage::Hashing,
        initial_message,
        Some(hash_targets.len()),
        0,
        0,
        0,
    ));

    let mut hashed = 0usize;
    for chunk in hash_targets.chunks(config.checkpoint_interval) {
        let chunk_results: Vec<(usize, Option<String>)> = chunk
            .par_iter()
            .map(|index| {
                let file = &files[*index];
                match get_xxhash(Path::new(&file.file_path)) {
                    Ok(hash) => (*index, Some(hash)),
                    Err(error) => {
                        warn!("Failed to hash {}: {}", file.file_path, error);
                        (*index, None)
                    }
                }
            })
            .collect();

        for (index, hash) in chunk_results {
            if let Some(hash) = hash {
                files[index].xxhash = hash;
            }
        }

        hashed += chunk.len();
        emit(progress(
            RunStage::Hashing,
            format!(
                "Hashed {} of {} candidate files",
                hashed,
                hash_targets.len()
            ),
            Some(hash_targets.len()),
            hashed,
            0,
            0,
        ));
    }

    Ok(())
}

fn get_sample_fingerprint(file_path: &Path, file_size: u64) -> Result<String> {
    let mut hasher = Xxh3::new();
    let mut file = File::open(file_path).context("Failed to open file for fingerprinting")?;

    if file_size <= (SAMPLE_FINGERPRINT_BYTES as u64) * 2 {
        let mut buffer = vec![0_u8; SAMPLE_FINGERPRINT_BYTES.min(file_size as usize)];
        file.read_exact(&mut buffer)?;
        hasher.update(&buffer);
        return Ok(format!("{:x}", hasher.digest()));
    }

    let mut first = vec![0_u8; SAMPLE_FINGERPRINT_BYTES];
    file.read_exact(&mut first)?;
    hasher.update(&first);

    file.seek(SeekFrom::End(-(SAMPLE_FINGERPRINT_BYTES as i64)))?;
    let mut last = vec![0_u8; SAMPLE_FINGERPRINT_BYTES];
    file.read_exact(&mut last)?;
    hasher.update(&last);

    Ok(format!("{:x}", hasher.digest()))
}

fn get_xxhash(file_path: &Path) -> Result<String> {
    let mut hasher = Xxh3::new();
    let mut file = File::open(file_path).context("Failed to open file for hashing")?;
    let mut buffer = [0_u8; 8192];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.digest()))
}

fn save_checkpoint_batch(batch: &[FileInfo], index: usize, output_dir: &Path) -> Result<()> {
    let path = output_dir
        .join(CHECKPOINT_DIR)
        .join(format!("checkpoint_{index}.csv"));
    save_file_infos(batch, &path)
}

fn plan_deletions(files: &[FileInfo], keep_rule: &KeepRule) -> Result<DedupPlan> {
    let mut groups: HashMap<(u64, String), Vec<FileInfo>> = HashMap::new();
    let mut kept = Vec::new();

    for file in files.iter().cloned() {
        if file.xxhash.is_empty() {
            kept.push(file);
            continue;
        }
        groups
            .entry((file.file_size, file.xxhash.clone()))
            .or_default()
            .push(file);
    }

    let mut planned_deletions = Vec::new();
    let mut duplicate_sets = 0;

    for (_, mut group) in groups {
        if group.len() == 1 {
            kept.push(group.remove(0));
            continue;
        }
        sort_group_for_keep_rule(&mut group, keep_rule);
        for cluster in split_verified_clusters(group) {
            if cluster.len() == 1 {
                kept.push(cluster.into_iter().next().expect("cluster item"));
                continue;
            }
            duplicate_sets += 1;
            let mut iter = cluster.into_iter();
            kept.push(iter.next().expect("keep item"));
            planned_deletions.extend(iter);
        }
    }

    kept.sort_by(|left, right| left.file_path.cmp(&right.file_path));
    planned_deletions.sort_by(|left, right| left.file_path.cmp(&right.file_path));
    Ok(DedupPlan {
        kept,
        planned_deletions,
        duplicate_sets,
    })
}

fn sort_group_for_keep_rule(group: &mut [FileInfo], keep_rule: &KeepRule) {
    match keep_rule {
        KeepRule::OldestCreated => group.sort_by(|left, right| {
            left.created_time
                .cmp(&right.created_time)
                .then_with(|| left.file_path.cmp(&right.file_path))
        }),
        KeepRule::NewestModified => group.sort_by(|left, right| {
            right
                .modified_time
                .cmp(&left.modified_time)
                .then_with(|| left.file_path.cmp(&right.file_path))
        }),
        KeepRule::PreferPath(preferred_root) => {
            let preferred_root = normalize_dir_prefix(preferred_root);
            group.sort_by(|left, right| {
                let left_pref = normalize_path_string(&left.file_path).starts_with(&preferred_root);
                let right_pref =
                    normalize_path_string(&right.file_path).starts_with(&preferred_root);
                right_pref
                    .cmp(&left_pref)
                    .then_with(|| left.created_time.cmp(&right.created_time))
                    .then_with(|| left.file_path.cmp(&right.file_path))
            });
        }
    }
}

fn split_verified_clusters(group: Vec<FileInfo>) -> Vec<Vec<FileInfo>> {
    let mut clusters: Vec<Vec<FileInfo>> = Vec::new();
    'outer: for file in group {
        for cluster in &mut clusters {
            match files_are_identical(Path::new(&file.file_path), Path::new(&cluster[0].file_path))
            {
                Ok(true) => {
                    cluster.push(file);
                    continue 'outer;
                }
                Ok(false) => {}
                Err(error) => warn!(
                    "Could not verify {} against {}: {}",
                    file.file_path, cluster[0].file_path, error
                ),
            }
        }
        clusters.push(vec![file]);
    }
    clusters
}

fn files_are_identical(left: &Path, right: &Path) -> Result<bool> {
    let left_meta = fs::metadata(left)?;
    let right_meta = fs::metadata(right)?;
    if left_meta.len() != right_meta.len() {
        return Ok(false);
    }
    let mut left_reader = BufReader::new(File::open(left)?);
    let mut right_reader = BufReader::new(File::open(right)?);
    let mut left_buffer = [0_u8; 8192];
    let mut right_buffer = [0_u8; 8192];
    loop {
        let left_read = left_reader.read(&mut left_buffer)?;
        let right_read = right_reader.read(&mut right_buffer)?;
        if left_read != right_read {
            return Ok(false);
        }
        if left_read == 0 {
            return Ok(true);
        }
        if left_buffer[..left_read] != right_buffer[..right_read] {
            return Ok(false);
        }
    }
}

fn execute_deletions<F>(
    files: &[FileInfo],
    use_trash: bool,
    emit: &mut F,
) -> Result<DeletionOutcome>
where
    F: FnMut(ProgressUpdate),
{
    let deletion_errors: Vec<Option<String>> = files
        .par_iter()
        .map(|file| {
            let result = if use_trash {
                trash::delete(&file.file_path).map_err(|e| e.to_string())
            } else {
                fs::remove_file(&file.file_path).map_err(|e| e.to_string())
            };
            result.err()
        })
        .collect();

    let mut deleted = Vec::new();
    let mut failed = Vec::new();
    for (file, error) in files.iter().zip(deletion_errors) {
        match error {
            None => deleted.push(file.clone()),
            Some(message) => failed.push(FailedDeletionRecord {
                file_path: file.file_path.clone(),
                error: message,
            }),
        }
        emit(progress(
            RunStage::Deleting,
            format!(
                "Applying deletion plan: {} of {}",
                deleted.len() + failed.len(),
                files.len()
            ),
            None,
            0,
            files.len(),
            deleted.len(),
        ));
    }
    Ok(DeletionOutcome { deleted, failed })
}

fn save_file_infos(files: &[FileInfo], path: &Path) -> Result<()> {
    let mut writer = WriterBuilder::new().has_headers(false).from_path(path)?;
    writer.write_record([
        "file_path",
        "file_size",
        "xxhash",
        "created_time",
        "modified_time",
        "file_name",
    ])?;
    for file in files {
        writer.serialize(file)?;
    }
    writer.flush()?;
    Ok(())
}

fn save_failed_deletions(records: &[FailedDeletionRecord], path: &Path) -> Result<()> {
    let mut writer = WriterBuilder::new().has_headers(false).from_path(path)?;
    writer.write_record(["file_path", "error"])?;
    for record in records {
        writer.serialize(record)?;
    }
    writer.flush()?;
    Ok(())
}

fn save_summary(summary: &RunSummary, path: &Path) -> Result<()> {
    let text = format!(
        "mode: {}\nroot_dir: {}\noutput_dir: {}\nkeep_rule: {}\ndiscovered_files: {}\nscanned_files: {}\nkept_files: {}\nplanned_deletions: {}\ndeleted_files: {}\nfailed_deletions: {}\nduplicate_sets: {}\n",
        summary.mode.as_str(),
        summary.root_dir.display(),
        summary.output_dir.display(),
        summary.keep_rule,
        summary.discovered_files,
        summary.scanned_files,
        summary.kept_files,
        summary.planned_deletions,
        summary.deleted_files,
        summary.failed_deletions,
        summary.duplicate_sets,
    );
    fs::write(path, text)?;
    Ok(())
}

fn progress(
    stage: RunStage,
    message: impl Into<String>,
    discovered_files: Option<usize>,
    processed_files: usize,
    planned_deletions: usize,
    deleted_files: usize,
) -> ProgressUpdate {
    ProgressUpdate {
        stage,
        message: message.into(),
        discovered_files,
        processed_files,
        planned_deletions,
        deleted_files,
    }
}

fn next_value<I>(iter: &mut I, flag: &str, binary: &str) -> Result<String>
where
    I: Iterator<Item = String>,
{
    iter.next()
        .ok_or_else(|| anyhow!("Missing value for {flag}\n\n{}", usage(binary)))
}

fn split_flag(flag: &str) -> String {
    flag.split_once('=')
        .map(|(_, value)| value)
        .unwrap_or_default()
        .to_string()
}

fn parse_keep_rule(name: &str, prefer_path: Option<PathBuf>) -> Result<KeepRule> {
    match name {
        "oldest-created" => Ok(KeepRule::OldestCreated),
        "newest-modified" => Ok(KeepRule::NewestModified),
        "prefer-path" => Ok(KeepRule::PreferPath(absolutize_path(
            prefer_path.ok_or_else(|| anyhow!("--keep prefer-path requires --prefer-path"))?,
        )?)),
        other => bail!("Unsupported keep rule: {other}"),
    }
}

fn checkpoint_index(path: &Path) -> Option<usize> {
    let stem = path.file_stem()?.to_str()?;
    stem.strip_prefix("checkpoint_")?.parse::<usize>().ok()
}

fn path_matches_filters(path: &Path, includes: &[String], excludes: &[String]) -> bool {
    let value = normalize_path(path);
    let include_match = includes.is_empty()
        || includes
            .iter()
            .any(|pattern| value.contains(&pattern.to_lowercase()));
    let exclude_match = excludes
        .iter()
        .any(|pattern| value.contains(&pattern.to_lowercase()));
    include_match && !exclude_match
}

fn is_output_path(candidate: &Path, output_dir: &Path) -> bool {
    let candidate = normalize_path(candidate);
    let output_dir = normalize_dir_prefix(output_dir);
    let sep = std::path::MAIN_SEPARATOR;
    candidate == output_dir.trim_end_matches(sep) || candidate.starts_with(&output_dir)
}

fn normalize_dir_prefix(path: &Path) -> String {
    let sep = std::path::MAIN_SEPARATOR;
    let mut value = normalize_path(path);
    if !value.ends_with(sep) {
        value.push(sep);
    }
    value
}

fn normalize_path(path: &Path) -> String {
    normalize_path_string(&path.to_string_lossy())
}

fn normalize_path_string(value: &str) -> String {
    if cfg!(windows) {
        value.replace('/', "\\").to_lowercase()
    } else {
        value.replace('\\', "/").to_lowercase()
    }
}

fn is_system_protected_path(path: &Path) -> bool {
    if !cfg!(windows) {
        return false;
    }
    let normalized = normalize_path(path);
    const SYSTEM_PATTERNS: &[&str] = &[
        r"\windows\system32",
        r"\windows\syswow64",
        r"\windows\winsxs",
        "$recycle.bin",
        "system volume information",
    ];
    SYSTEM_PATTERNS.iter().any(|p| normalized.contains(*p))
}

fn path_to_string(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

fn absolutize_path(path: PathBuf) -> Result<PathBuf> {
    if path.is_absolute() {
        canonicalize_or_original(&path)
    } else {
        canonicalize_or_original(&std::env::current_dir()?.join(path))
    }
}

fn canonicalize_or_original(path: &Path) -> Result<PathBuf> {
    match path.canonicalize() {
        Ok(path) => Ok(path),
        Err(_) => Ok(path.to_path_buf()),
    }
}

fn system_time_to_unix(value: SystemTime) -> Result<i64> {
    Ok(value.duration_since(UNIX_EPOCH)?.as_secs() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn fast_prefilter_only_hashes_same_size_groups() -> Result<()> {
        let root_dir = create_temp_dir("fast_prefilter_root")?;
        let output_dir = create_temp_dir("fast_prefilter_output")?;

        fs::write(root_dir.join("a.txt"), b"same")?;
        fs::write(root_dir.join("b.txt"), b"same")?;
        fs::write(root_dir.join("c.txt"), b"diff")?;
        fs::write(root_dir.join("d.txt"), b"unique-size")?;

        let config = RunConfig {
            root_dir: root_dir.clone(),
            output_dir: output_dir.clone(),
            mode: OperationMode::DryRun,
            keep_rule: KeepRule::OldestCreated,
            fast_prefilter: true,
            include_filters: Vec::new(),
            exclude_filters: Vec::new(),
            follow_symlinks: false,
            use_trash: false,
            resume: false,
            checkpoint_interval: 2,
        };

        let artifacts = run(&config)?;
        let unique = artifacts
            .all_files
            .iter()
            .find(|file| file.file_name == "d.txt")
            .expect("unique file should be scanned");
        assert!(unique.xxhash.is_empty());
        assert_eq!(artifacts.summary.planned_deletions, 1);

        cleanup_temp_dir(&root_dir);
        cleanup_temp_dir(&output_dir);
        Ok(())
    }

    #[test]
    fn hash_all_files_mode_hashes_unique_sizes_too() -> Result<()> {
        let root_dir = create_temp_dir("hash_all_root")?;
        let output_dir = create_temp_dir("hash_all_output")?;

        fs::write(root_dir.join("a.txt"), b"same")?;
        fs::write(root_dir.join("b.txt"), b"same")?;
        fs::write(root_dir.join("d.txt"), b"unique-size")?;

        let config = RunConfig {
            root_dir: root_dir.clone(),
            output_dir: output_dir.clone(),
            mode: OperationMode::DryRun,
            keep_rule: KeepRule::OldestCreated,
            fast_prefilter: false,
            include_filters: Vec::new(),
            exclude_filters: Vec::new(),
            follow_symlinks: false,
            use_trash: false,
            resume: false,
            checkpoint_interval: 2,
        };

        let artifacts = run(&config)?;
        let unique = artifacts
            .all_files
            .iter()
            .find(|file| file.file_name == "d.txt")
            .expect("unique file should be scanned");
        assert!(!unique.xxhash.is_empty());

        cleanup_temp_dir(&root_dir);
        cleanup_temp_dir(&output_dir);
        Ok(())
    }

    #[test]
    fn fast_prefilter_skips_full_hash_when_same_size_files_fingerprint_differ() -> Result<()> {
        let root_dir = create_temp_dir("fingerprint_skip_root")?;
        let output_dir = create_temp_dir("fingerprint_skip_output")?;

        fs::write(root_dir.join("a.txt"), b"abcd")?;
        fs::write(root_dir.join("b.txt"), b"wxyz")?;

        let config = RunConfig {
            root_dir: root_dir.clone(),
            output_dir: output_dir.clone(),
            mode: OperationMode::DryRun,
            keep_rule: KeepRule::OldestCreated,
            fast_prefilter: true,
            include_filters: Vec::new(),
            exclude_filters: Vec::new(),
            follow_symlinks: false,
            use_trash: false,
            resume: false,
            checkpoint_interval: 2,
        };

        let artifacts = run(&config)?;
        assert_eq!(artifacts.summary.planned_deletions, 0);
        assert!(artifacts
            .all_files
            .iter()
            .all(|file| file.xxhash.is_empty()));

        cleanup_temp_dir(&root_dir);
        cleanup_temp_dir(&output_dir);
        Ok(())
    }

    #[test]
    fn fast_prefilter_falls_back_to_full_hash_when_samples_match() -> Result<()> {
        let root_dir = create_temp_dir("fingerprint_match_root")?;
        let output_dir = create_temp_dir("fingerprint_match_output")?;

        let first = vec![b'a'; SAMPLE_FINGERPRINT_BYTES];
        let middle_left = vec![b'x'; 1024];
        let middle_right = vec![b'y'; 1024];
        let last = vec![b'z'; SAMPLE_FINGERPRINT_BYTES];

        let mut left = Vec::new();
        left.extend_from_slice(&first);
        left.extend_from_slice(&middle_left);
        left.extend_from_slice(&last);

        let mut right = Vec::new();
        right.extend_from_slice(&first);
        right.extend_from_slice(&middle_right);
        right.extend_from_slice(&last);

        fs::write(root_dir.join("left.bin"), &left)?;
        fs::write(root_dir.join("right.bin"), &right)?;

        let config = RunConfig {
            root_dir: root_dir.clone(),
            output_dir: output_dir.clone(),
            mode: OperationMode::DryRun,
            keep_rule: KeepRule::OldestCreated,
            fast_prefilter: true,
            include_filters: Vec::new(),
            exclude_filters: Vec::new(),
            follow_symlinks: false,
            use_trash: false,
            resume: false,
            checkpoint_interval: 2,
        };

        let artifacts = run(&config)?;
        assert_eq!(artifacts.summary.planned_deletions, 0);
        assert!(artifacts
            .all_files
            .iter()
            .all(|file| !file.xxhash.is_empty()));

        cleanup_temp_dir(&root_dir);
        cleanup_temp_dir(&output_dir);
        Ok(())
    }

    // --- deletion ---

    #[test]
    fn apply_mode_deletes_duplicate_files() -> Result<()> {
        let root_dir = create_temp_dir("apply_root")?;
        let output_dir = create_temp_dir("apply_output")?;

        fs::write(root_dir.join("a.txt"), b"duplicate content")?;
        fs::write(root_dir.join("b.txt"), b"duplicate content")?;
        fs::write(root_dir.join("c.txt"), b"unique")?;

        let config = RunConfig {
            root_dir: root_dir.clone(),
            output_dir: output_dir.clone(),
            mode: OperationMode::Apply,
            keep_rule: KeepRule::OldestCreated,
            fast_prefilter: true,
            include_filters: Vec::new(),
            exclude_filters: Vec::new(),
            follow_symlinks: false,
            use_trash: false,
            resume: false,
            checkpoint_interval: 100,
        };

        let artifacts = run(&config)?;
        assert_eq!(artifacts.summary.deleted_files, 1);
        assert_eq!(artifacts.summary.failed_deletions, 0);
        let a_exists = root_dir.join("a.txt").exists();
        let b_exists = root_dir.join("b.txt").exists();
        assert!(
            a_exists ^ b_exists,
            "exactly one of a.txt/b.txt should be deleted"
        );
        assert!(
            root_dir.join("c.txt").exists(),
            "unique file should not be deleted"
        );

        cleanup_temp_dir(&root_dir);
        cleanup_temp_dir(&output_dir);
        Ok(())
    }

    // --- filters ---

    #[test]
    fn include_filter_restricts_scanned_files() -> Result<()> {
        let root_dir = create_temp_dir("include_root")?;
        let output_dir = create_temp_dir("include_output")?;

        fs::write(root_dir.join("a.txt"), b"same")?;
        fs::write(root_dir.join("b.txt"), b"same")?;
        fs::write(root_dir.join("a.log"), b"same")?;

        let config = RunConfig {
            root_dir: root_dir.clone(),
            output_dir: output_dir.clone(),
            mode: OperationMode::DryRun,
            keep_rule: KeepRule::OldestCreated,
            fast_prefilter: true,
            include_filters: vec![".txt".to_string()],
            exclude_filters: Vec::new(),
            follow_symlinks: false,
            use_trash: false,
            resume: false,
            checkpoint_interval: 100,
        };

        let artifacts = run(&config)?;
        assert!(
            artifacts
                .all_files
                .iter()
                .all(|f| f.file_name.ends_with(".txt")),
            "only .txt files should be scanned"
        );
        assert_eq!(artifacts.summary.planned_deletions, 1);

        cleanup_temp_dir(&root_dir);
        cleanup_temp_dir(&output_dir);
        Ok(())
    }

    #[test]
    fn exclude_filter_skips_matching_files() -> Result<()> {
        let root_dir = create_temp_dir("exclude_root")?;
        let output_dir = create_temp_dir("exclude_output")?;

        fs::write(root_dir.join("a.txt"), b"same")?;
        fs::write(root_dir.join("b.txt"), b"same")?;
        fs::write(root_dir.join("skip_this.txt"), b"same")?;

        let config = RunConfig {
            root_dir: root_dir.clone(),
            output_dir: output_dir.clone(),
            mode: OperationMode::DryRun,
            keep_rule: KeepRule::OldestCreated,
            fast_prefilter: false,
            include_filters: Vec::new(),
            exclude_filters: vec!["skip_".to_string()],
            follow_symlinks: false,
            use_trash: false,
            resume: false,
            checkpoint_interval: 100,
        };

        let artifacts = run(&config)?;
        assert!(
            !artifacts
                .all_files
                .iter()
                .any(|f| f.file_name.starts_with("skip_")),
            "files matching exclude filter must not be scanned"
        );

        cleanup_temp_dir(&root_dir);
        cleanup_temp_dir(&output_dir);
        Ok(())
    }

    // --- keep rules ---

    #[test]
    fn keep_rule_newest_modified_keeps_most_recently_changed() -> Result<()> {
        let root_dir = create_temp_dir("newest_mod_root")?;
        let output_dir = create_temp_dir("newest_mod_output")?;

        let content = b"identical bytes";
        fs::write(root_dir.join("old.txt"), content)?;
        std::thread::sleep(std::time::Duration::from_millis(50));
        fs::write(root_dir.join("new.txt"), content)?;

        let config = RunConfig {
            root_dir: root_dir.clone(),
            output_dir: output_dir.clone(),
            mode: OperationMode::DryRun,
            keep_rule: KeepRule::NewestModified,
            fast_prefilter: false,
            include_filters: Vec::new(),
            exclude_filters: Vec::new(),
            follow_symlinks: false,
            use_trash: false,
            resume: false,
            checkpoint_interval: 100,
        };

        let artifacts = run(&config)?;
        assert_eq!(artifacts.summary.planned_deletions, 1);
        let deleted = &artifacts.planned_deletions[0];
        assert_eq!(
            deleted.file_name, "old.txt",
            "oldest-modified file should be deleted, got {}",
            deleted.file_name
        );

        cleanup_temp_dir(&root_dir);
        cleanup_temp_dir(&output_dir);
        Ok(())
    }

    #[test]
    fn keep_rule_prefer_path_keeps_file_in_preferred_dir() -> Result<()> {
        let root_dir = create_temp_dir("prefer_path_root")?;
        let output_dir = create_temp_dir("prefer_path_output")?;

        let preferred = root_dir.join("preferred");
        let other = root_dir.join("other");
        fs::create_dir_all(&preferred)?;
        fs::create_dir_all(&other)?;

        let content = b"same content here";
        fs::write(preferred.join("file.txt"), content)?;
        fs::write(other.join("file.txt"), content)?;

        let config = RunConfig {
            root_dir: root_dir.clone(),
            output_dir: output_dir.clone(),
            mode: OperationMode::DryRun,
            keep_rule: KeepRule::PreferPath(preferred.clone()),
            fast_prefilter: false,
            include_filters: Vec::new(),
            exclude_filters: Vec::new(),
            follow_symlinks: false,
            use_trash: false,
            resume: false,
            checkpoint_interval: 100,
        };

        let artifacts = run(&config)?;
        assert_eq!(artifacts.summary.planned_deletions, 1);
        let kept = artifacts
            .kept_files
            .iter()
            .find(|f| f.file_name == "file.txt")
            .expect("kept file should exist");
        assert!(
            kept.file_path.to_lowercase().contains("preferred"),
            "preferred path file should be kept, got {}",
            kept.file_path
        );

        cleanup_temp_dir(&root_dir);
        cleanup_temp_dir(&output_dir);
        Ok(())
    }

    // --- resume ---

    #[test]
    fn resume_reuses_previously_scanned_files() -> Result<()> {
        let root_dir = create_temp_dir("resume_root")?;
        let output_dir = create_temp_dir("resume_output")?;

        fs::write(root_dir.join("a.txt"), b"same")?;
        fs::write(root_dir.join("b.txt"), b"same")?;
        fs::write(root_dir.join("c.txt"), b"unique content xyz")?;

        let config = RunConfig {
            root_dir: root_dir.clone(),
            output_dir: output_dir.clone(),
            mode: OperationMode::DryRun,
            keep_rule: KeepRule::OldestCreated,
            fast_prefilter: false,
            include_filters: Vec::new(),
            exclude_filters: Vec::new(),
            follow_symlinks: false,
            use_trash: false,
            resume: false,
            checkpoint_interval: 1,
        };
        let first = run(&config)?;

        let mut resume_config = config.clone();
        resume_config.resume = true;
        let second = run(&resume_config)?;

        assert_eq!(
            first.summary.planned_deletions, second.summary.planned_deletions,
            "resumed run should produce the same plan as the original"
        );

        cleanup_temp_dir(&root_dir);
        cleanup_temp_dir(&output_dir);
        Ok(())
    }

    // --- CSV I/O ---

    #[test]
    fn csv_round_trip_preserves_all_fields() -> Result<()> {
        let dir = create_temp_dir("csv_roundtrip")?;
        let path = dir.join("files.csv");

        let original = vec![
            FileInfo {
                file_path: "/tmp/a.txt".to_string(),
                file_size: 42,
                xxhash: "deadbeef".to_string(),
                created_time: 1_000_000,
                modified_time: 2_000_000,
                file_name: "a.txt".to_string(),
            },
            FileInfo {
                file_path: "/tmp/b.txt".to_string(),
                file_size: 0,
                xxhash: String::new(),
                created_time: 3_000_000,
                modified_time: 4_000_000,
                file_name: "b.txt".to_string(),
            },
        ];

        save_file_infos(&original, &path)?;
        let loaded = load_file_infos(&path)?;

        assert_eq!(original.len(), loaded.len());
        for (expected, got) in original.iter().zip(loaded.iter()) {
            assert_eq!(expected, got);
        }

        cleanup_temp_dir(&dir);
        Ok(())
    }

    fn create_temp_dir(prefix: &str) -> Result<PathBuf> {
        let path = std::env::temp_dir().join(format!(
            "duplicate_file_deletor_{prefix}_{}",
            TEST_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        if path.exists() {
            fs::remove_dir_all(&path)?;
        }
        fs::create_dir_all(&path)?;
        Ok(path)
    }

    fn cleanup_temp_dir(path: &Path) {
        let _ = fs::remove_dir_all(path);
    }
}
