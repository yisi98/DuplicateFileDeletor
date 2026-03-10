use anyhow::{anyhow, bail, Context, Result};
use chrono::Local;
use csv::{Reader, WriterBuilder};
use log::warn;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, File},
    io::{BufReader, Read},
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
    pub include_filters: Vec<String>,
    pub exclude_filters: Vec<String>,
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
        let mut include_filters = Vec::new();
        let mut exclude_filters = Vec::new();
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
                "--include" => include_filters.push(next_value(&mut iter, "--include", &binary)?),
                "--exclude" => exclude_filters.push(next_value(&mut iter, "--exclude", &binary)?),
                "--apply" => mode = OperationMode::Apply,
                "--dry-run" => mode = OperationMode::DryRun,
                "--resume" => resume = true,
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
            include_filters,
            exclude_filters,
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
    format!("Usage:\n  {binary} --path <directory> [options]\n  {binary} <directory> [options]\n\nOptions:\n  --apply                    Delete duplicate files after review\n  --dry-run                  Plan only (default)\n  --output-dir <dir>         Folder for reports and checkpoints\n  --resume                   Resume from an existing output directory\n  --keep <rule>              oldest-created | newest-modified | prefer-path\n  --prefer-path <dir>        Preferred directory for --keep prefer-path\n  --include <text>           Only scan paths containing this text\n  --exclude <text>           Skip paths containing this text\n  -h, --help                 Show help")
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
        OperationMode::Apply => execute_deletions(&plan.planned_deletions, &mut emit)?,
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
        .into_iter()
        .filter_entry(|entry| !is_output_path(entry.path(), &output_dir))
        .filter_map(|entry| entry.ok())
    {
        let path = entry.path();
        if !entry.file_type().is_file() || is_output_path(path, &output_dir) {
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
        load_checkpoints(&config.output_dir)?
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
            format!("Loaded {} files from checkpoints", already_scanned.len()),
            Some(already_scanned.len() + remaining.len()),
            already_scanned.len(),
            0,
            0,
        ));
    }

    for chunk in remaining.chunks(config.checkpoint_interval) {
        let mut chunk_results: Vec<FileInfo> = chunk
            .par_iter()
            .filter_map(|path| match process_file(path) {
                Ok(Some(file)) => Some(file),
                Ok(None) => None,
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
                "Scanned {} of {} files",
                files.len(),
                files.len() + remaining.len().saturating_sub(files.len())
            ),
            Some(already_scanned.len() + remaining.len()),
            files.len(),
            0,
            0,
        ));
    }

    files.sort_by(|left, right| left.file_path.cmp(&right.file_path));
    Ok(files)
}

fn load_checkpoints(output_dir: &Path) -> Result<LoadedCheckpoints> {
    let checkpoint_dir = output_dir.join(CHECKPOINT_DIR);
    if !checkpoint_dir.exists() {
        return Ok(LoadedCheckpoints::default());
    }

    let mut files = Vec::new();
    let mut seen = HashSet::new();
    let mut next_batch_index = 0;
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
        next_batch_index = next_batch_index.max(index + 1);
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
        next_batch_index,
    })
}

fn process_file(path: &Path) -> Result<Option<FileInfo>> {
    let metadata = fs::metadata(path)?;
    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .ok_or_else(|| anyhow!("Invalid file name"))?
        .to_string();
    let created_time = system_time_to_unix(metadata.created().or_else(|_| metadata.modified())?)?;
    let modified_time = system_time_to_unix(metadata.modified().or_else(|_| metadata.created())?)?;
    let xxhash = match get_xxhash(path) {
        Ok(hash) => hash,
        Err(error) => {
            warn!("Failed to hash {}: {}", path.display(), error);
            return Ok(None);
        }
    };

    Ok(Some(FileInfo {
        file_path: path_to_string(path),
        file_size: metadata.len(),
        xxhash,
        created_time,
        modified_time,
        file_name,
    }))
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
    for file in files.iter().cloned() {
        groups
            .entry((file.file_size, file.xxhash.clone()))
            .or_default()
            .push(file);
    }

    let mut kept = Vec::new();
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

fn execute_deletions<F>(files: &[FileInfo], emit: &mut F) -> Result<DeletionOutcome>
where
    F: FnMut(ProgressUpdate),
{
    let mut deleted = Vec::new();
    let mut failed = Vec::new();
    for (index, file) in files.iter().enumerate() {
        match fs::remove_file(&file.file_path) {
            Ok(()) => deleted.push(file.clone()),
            Err(error) => failed.push(FailedDeletionRecord {
                file_path: file.file_path.clone(),
                error: error.to_string(),
            }),
        }
        emit(progress(
            RunStage::Deleting,
            format!("Applying deletion plan: {} of {}", index + 1, files.len()),
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
    candidate == output_dir.trim_end_matches('\\') || candidate.starts_with(&output_dir)
}

fn normalize_dir_prefix(path: &Path) -> String {
    let mut value = normalize_path(path);
    if !value.ends_with('\\') {
        value.push('\\');
    }
    value
}

fn normalize_path(path: &Path) -> String {
    normalize_path_string(&path.to_string_lossy())
}

fn normalize_path_string(value: &str) -> String {
    value.replace('/', "\\").to_lowercase()
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
