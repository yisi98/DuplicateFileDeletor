use duplicate_file_deletor::{
    suggested_output_dir, FileInfo, KeepRule, OperationMode, ProgressUpdate, RunArtifacts,
    RunConfig, RunStage,
};
use eframe::egui::{
    self, Align, Color32, CornerRadius, Frame, Layout, Margin, RichText, Stroke, Vec2,
};
use std::{
    path::Path,
    process::Command,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
pub struct DedupeApp {
    form: RunForm,
    active_run: Option<ActiveRun>,
    last_result: Option<RunArtifacts>,
    last_config: Option<RunConfig>,
    latest_progress: Option<ProgressUpdate>,
    status_message: String,
    table_filter: String,
}

struct ActiveRun {
    rx: mpsc::Receiver<UiMessage>,
    started_at: Instant,
}

enum UiMessage {
    Progress(ProgressUpdate),
    Finished(Box<Result<RunArtifacts, String>>, RunConfig),
}

const GUI_PROGRESS_INTERVAL: usize = 100;

#[derive(Clone)]
struct RunForm {
    root_dir: String,
    output_dir: String,
    include_filters: String,
    exclude_filters: String,
    keep_rule_index: usize,
    prefer_path: String,
    fast_prefilter: bool,
    resume: bool,
}

impl Default for DedupeApp {
    fn default() -> Self {
        let output_dir = suggested_output_dir().unwrap_or_default();
        Self {
            form: RunForm {
                root_dir: String::new(),
                output_dir: output_dir.display().to_string(),
                include_filters: String::new(),
                exclude_filters: String::new(),
                keep_rule_index: 0,
                prefer_path: String::new(),
                fast_prefilter: true,
                resume: false,
            },
            active_run: None,
            last_result: None,
            last_config: None,
            latest_progress: None,
            status_message: "Choose a folder and start with a safe dry run.".to_string(),
            table_filter: String::new(),
        }
    }
}

impl DedupeApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        configure_theme(&cc.egui_ctx);
        Self::default()
    }
}

impl eframe::App for DedupeApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_background_messages();
        ctx.request_repaint_after(std::time::Duration::from_millis(150));

        egui::TopBottomPanel::top("hero")
            .frame(Frame::new().fill(Color32::from_rgb(7, 10, 16)))
            .show(ctx, |ui| {
                Frame::new()
                    .fill(Color32::from_rgb(18, 24, 38))
                    .inner_margin(Margin::same(18))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.label(
                                    RichText::new("DuplicateFileDeletor")
                                        .size(28.0)
                                        .strong()
                                        .color(Color32::from_rgb(240, 244, 255)),
                                );
                                ui.label(
                                    RichText::new(
                                        "Desktop deduplication with review-first safety.",
                                    )
                                    .size(13.5)
                                    .color(Color32::from_rgb(150, 170, 192)),
                                );
                            });
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                status_chip(
                                    ui,
                                    if self.active_run.is_some() {
                                        "Running"
                                    } else {
                                        "Ready"
                                    },
                                );
                            });
                        });
                    });
            });

        egui::SidePanel::left("controls")
            .resizable(false)
            .default_width(360.0)
            .frame(
                Frame::new()
                    .fill(Color32::from_rgb(7, 10, 16))
                    .inner_margin(Margin::same(8)),
            )
            .show(ctx, |ui| self.render_controls(ui));

        egui::CentralPanel::default()
            .frame(
                Frame::new()
                    .fill(Color32::from_rgb(7, 10, 16))
                    .inner_margin(Margin::same(8)),
            )
            .show(ctx, |ui| self.render_results(ui));
    }
}

impl DedupeApp {
    fn render_controls(&mut self, ui: &mut egui::Ui) {
        card(ui, "Run Setup", |ui| {
            labeled_field(
                ui,
                "Folder to scan",
                &mut self.form.root_dir,
                "Choose the root folder",
            );
            if ui.button("Browse folder").clicked() {
                if let Some(path) = rfd::FileDialog::new().pick_folder() {
                    self.form.root_dir = path.display().to_string();
                }
            }

            ui.add_space(10.0);
            labeled_field(
                ui,
                "Report folder",
                &mut self.form.output_dir,
                "Output and checkpoints",
            );
            ui.horizontal(|ui| {
                if ui.button("Browse output").clicked() {
                    if let Some(path) = rfd::FileDialog::new().pick_folder() {
                        self.form.output_dir = path.display().to_string();
                    }
                }
                if ui.button("New timestamped folder").clicked() {
                    if let Ok(path) = suggested_output_dir() {
                        self.form.output_dir = path.display().to_string();
                    }
                }
            });

            ui.add_space(10.0);
            ui.label(RichText::new("Keep rule").strong());
            egui::ComboBox::from_id_salt("keep-rule")
                .selected_text(self.keep_rule_label())
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.form.keep_rule_index, 0, "Oldest created");
                    ui.selectable_value(&mut self.form.keep_rule_index, 1, "Newest modified");
                    ui.selectable_value(&mut self.form.keep_rule_index, 2, "Prefer a folder");
                });
            if self.form.keep_rule_index == 2 {
                ui.add_space(8.0);
                labeled_field(
                    ui,
                    "Preferred folder",
                    &mut self.form.prefer_path,
                    "Prefer duplicates kept here",
                );
                if ui.button("Browse preferred folder").clicked() {
                    if let Some(path) = rfd::FileDialog::new().pick_folder() {
                        self.form.prefer_path = path.display().to_string();
                    }
                }
            }

            ui.add_space(10.0);
            labeled_field(
                ui,
                "Include filters",
                &mut self.form.include_filters,
                "Comma-separated substrings",
            );
            labeled_field(
                ui,
                "Exclude filters",
                &mut self.form.exclude_filters,
                "Comma-separated substrings",
            );
            ui.checkbox(
                &mut self.form.fast_prefilter,
                "Fast prefilter: only hash files that share the same size",
            );
            ui.label(
                RichText::new("This is faster and still verifies matches before deletion.")
                    .small()
                    .color(Color32::from_rgb(134, 148, 170)),
            );
            ui.checkbox(
                &mut self.form.resume,
                "Resume from checkpoints in the output folder",
            );
        });

        card(ui, "Actions", |ui| {
            let running = self.active_run.is_some();
            if primary_button(ui, "Plan Safe Dry Run").clicked() && !running {
                self.start_run(OperationMode::DryRun, false);
            }

            let can_apply_plan = self
                .last_result
                .as_ref()
                .is_some_and(|result| result.summary.planned_deletions > 0)
                && !running;
            let apply_button = ui.add_enabled(
                can_apply_plan,
                egui::Button::new(RichText::new("Apply Current Plan").strong())
                    .min_size(Vec2::new(ui.available_width(), 42.0)),
            );
            if apply_button.clicked() {
                self.apply_existing_plan();
            }

            if ui
                .add_enabled(
                    !running,
                    egui::Button::new("Start Fresh Apply")
                        .min_size(Vec2::new(ui.available_width(), 38.0)),
                )
                .clicked()
            {
                self.start_run(OperationMode::Apply, false);
            }

            ui.add_space(8.0);
            ui.label(RichText::new(&self.status_message).color(Color32::from_rgb(180, 190, 208)));

            let elapsed = self.active_run.as_ref().map(|run| run.started_at.elapsed());
            if running || self.latest_progress.is_some() {
                let progress = self.latest_progress.as_ref();
                ui.add(
                    egui::ProgressBar::new(progress_fraction(progress, running))
                        .desired_width(ui.available_width())
                        .text(progress_bar_text(progress, running)),
                );
                ui.add_space(8.0);
                ui.horizontal_wrapped(|ui| {
                    if let Some(progress) = progress {
                        info_pill(ui, format!("Stage: {}", run_stage_label(progress.stage)));
                        info_pill(ui, progress_counts_text(progress));
                    } else {
                        info_pill(ui, "Stage: starting".to_string());
                    }

                    if let Some(elapsed) = elapsed {
                        info_pill(ui, format!("Elapsed: {}", format_duration(elapsed)));
                        let eta = progress
                            .and_then(|progress| progress_eta(progress, elapsed))
                            .map(format_duration)
                            .unwrap_or_else(|| "Estimating...".to_string());
                        info_pill(ui, format!("ETA: {eta}"));
                    }
                });
            }
        });

        card(ui, "Quick Notes", |ui| {
            ui.label("Dry run is the recommended first step.");
            ui.label("Apply Current Plan reuses the same output folder and checkpoints.");
            ui.label("All reports are written as CSV so you can audit what happened.");
        });
    }

    fn render_results(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            metric_card(
                ui,
                "Scanned",
                self.last_result
                    .as_ref()
                    .map(|result| result.summary.scanned_files)
                    .unwrap_or(0),
                Color32::from_rgb(75, 180, 240),
            );
            metric_card(
                ui,
                "Kept",
                self.last_result
                    .as_ref()
                    .map(|result| result.summary.kept_files)
                    .unwrap_or(0),
                Color32::from_rgb(92, 201, 144),
            );
            metric_card(
                ui,
                "Planned",
                self.last_result
                    .as_ref()
                    .map(|result| result.summary.planned_deletions)
                    .unwrap_or(0),
                Color32::from_rgb(255, 181, 72),
            );
            metric_card(
                ui,
                "Deleted",
                self.last_result
                    .as_ref()
                    .map(|result| result.summary.deleted_files)
                    .unwrap_or(0),
                Color32::from_rgb(255, 107, 107),
            );
        });

        ui.add_space(12.0);
        card(ui, "Latest Run", |ui| {
            if let Some(result) = &self.last_result {
                ui.horizontal_wrapped(|ui| {
                    info_pill(ui, format!("Mode: {}", result.summary.mode.as_str()));
                    info_pill(ui, format!("Keep rule: {}", result.summary.keep_rule));
                    info_pill(
                        ui,
                        format!("Duplicate sets: {}", result.summary.duplicate_sets),
                    );
                });
                ui.add_space(8.0);
                ui.label(format!("Root: {}", result.summary.root_dir.display()));
                ui.label(format!("Output: {}", result.summary.output_dir.display()));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Open report folder").clicked() {
                        let _ = open_in_file_manager(&result.summary.output_dir);
                    }
                    if ui.button("Open scanned folder").clicked() {
                        let _ = open_in_file_manager(&result.summary.root_dir);
                    }
                });
            } else {
                ui.label("No runs yet. Configure a folder on the left and start with a dry run.");
            }
        });

        ui.add_space(12.0);
        card(ui, "Review Queue", |ui| {
            ui.horizontal(|ui| {
                ui.label("Filter paths");
                ui.text_edit_singleline(&mut self.table_filter);
            });
            ui.add_space(8.0);
            egui::ScrollArea::vertical()
                .max_height(420.0)
                .show(ui, |ui| {
                    if let Some(result) = &self.last_result {
                        render_file_list(
                            ui,
                            "Planned deletions",
                            &result.planned_deletions,
                            &self.table_filter,
                            true,
                        );
                        ui.add_space(12.0);
                        render_file_list(
                            ui,
                            "Kept files",
                            &result.kept_files,
                            &self.table_filter,
                            false,
                        );
                    } else {
                        ui.label("The dry-run report will appear here once the scan finishes.");
                    }
                });
        });
    }

    fn start_run(&mut self, mode: OperationMode, force_resume: bool) {
        match self.build_config(mode, force_resume) {
            Ok(config) => {
                let (tx, rx) = mpsc::channel();
                let config_for_thread = config.clone();
                thread::spawn(move || {
                    let result =
                        duplicate_file_deletor::run_with_progress(&config_for_thread, |update| {
                            let _ = tx.send(UiMessage::Progress(update));
                        })
                        .map_err(|error| error.to_string());
                    let _ = tx.send(UiMessage::Finished(Box::new(result), config_for_thread));
                });
                self.status_message = match mode {
                    OperationMode::DryRun => "Planning duplicates...".to_string(),
                    OperationMode::Apply => "Applying deletion workflow...".to_string(),
                };
                self.latest_progress = Some(starting_progress(mode));
                self.active_run = Some(ActiveRun {
                    rx,
                    started_at: Instant::now(),
                });
            }
            Err(error) => self.status_message = error,
        }
    }

    fn apply_existing_plan(&mut self) {
        if let Some(config) = &self.last_config {
            let mut config = config.clone();
            config.mode = OperationMode::Apply;
            config.resume = true;
            self.start_run_from_config(config);
        }
    }

    fn start_run_from_config(&mut self, config: RunConfig) {
        let (tx, rx) = mpsc::channel();
        let config_for_thread = config.clone();
        thread::spawn(move || {
            let result = duplicate_file_deletor::run_with_progress(&config_for_thread, |update| {
                let _ = tx.send(UiMessage::Progress(update));
            })
            .map_err(|error| error.to_string());
            let _ = tx.send(UiMessage::Finished(Box::new(result), config_for_thread));
        });
        self.status_message = "Applying current plan...".to_string();
        self.latest_progress = Some(starting_progress(OperationMode::Apply));
        self.active_run = Some(ActiveRun {
            rx,
            started_at: Instant::now(),
        });
    }

    fn build_config(&self, mode: OperationMode, force_resume: bool) -> Result<RunConfig, String> {
        if self.form.root_dir.trim().is_empty() {
            return Err("Choose a folder to scan first.".to_string());
        }
        if self.form.output_dir.trim().is_empty() {
            return Err("Choose a report folder first.".to_string());
        }
        if self.form.keep_rule_index == 2 && self.form.prefer_path.trim().is_empty() {
            return Err("Choose a preferred folder for the selected keep rule.".to_string());
        }

        let keep_rule = match self.form.keep_rule_index {
            0 => KeepRule::OldestCreated,
            1 => KeepRule::NewestModified,
            _ => KeepRule::PreferPath(std::path::PathBuf::from(self.form.prefer_path.trim())),
        };

        let config = RunConfig {
            root_dir: std::path::PathBuf::from(self.form.root_dir.trim()),
            output_dir: std::path::PathBuf::from(self.form.output_dir.trim()),
            mode,
            keep_rule,
            fast_prefilter: self.form.fast_prefilter,
            include_filters: split_filters(&self.form.include_filters),
            exclude_filters: split_filters(&self.form.exclude_filters),
            resume: force_resume || self.form.resume,
            checkpoint_interval: GUI_PROGRESS_INTERVAL,
        };
        config.validate().map_err(|error| error.to_string())?;
        Ok(config)
    }

    fn keep_rule_label(&self) -> &'static str {
        match self.form.keep_rule_index {
            0 => "Oldest created",
            1 => "Newest modified",
            _ => "Prefer a folder",
        }
    }

    fn poll_background_messages(&mut self) {
        let mut buffered = Vec::new();
        let mut elapsed = None;
        if let Some(active_run) = &self.active_run {
            while let Ok(message) = active_run.rx.try_recv() {
                buffered.push(message);
            }
            elapsed = Some(active_run.started_at.elapsed().as_secs_f32());
        }

        let mut finished = false;
        for message in buffered {
            match message {
                UiMessage::Progress(update) => {
                    self.status_message = update.message.clone();
                    self.latest_progress = Some(update);
                }
                UiMessage::Finished(result, config) => {
                    finished = true;
                    match *result {
                        Ok(artifacts) => {
                            self.status_message = format!(
                                "Finished in {:.1}s. Reports saved to {}",
                                elapsed.unwrap_or_default(),
                                artifacts.summary.output_dir.display()
                            );
                            self.last_config = Some(config);
                            self.last_result = Some(artifacts);
                        }
                        Err(error) => self.status_message = error,
                    }
                }
            }
        }

        if finished {
            self.active_run = None;
        }
    }
}

fn starting_progress(mode: OperationMode) -> ProgressUpdate {
    ProgressUpdate {
        stage: RunStage::Preparing,
        message: match mode {
            OperationMode::DryRun => "Starting dry run...".to_string(),
            OperationMode::Apply => "Starting apply run...".to_string(),
        },
        discovered_files: None,
        processed_files: 0,
        planned_deletions: 0,
        deleted_files: 0,
    }
}

fn progress_fraction(progress: Option<&ProgressUpdate>, is_running: bool) -> f32 {
    let fraction = match progress {
        Some(progress) => match progress.stage {
            RunStage::Preparing => 0.04,
            RunStage::Discovering => 0.08,
            RunStage::Scanning => progress
                .discovered_files
                .filter(|total| *total > 0)
                .map(|total| 0.10 + 0.45 * (progress.processed_files as f32 / total as f32))
                .unwrap_or(0.15),
            RunStage::Hashing => progress
                .discovered_files
                .filter(|total| *total > 0)
                .map(|total| 0.58 + 0.18 * (progress.processed_files as f32 / total as f32))
                .unwrap_or(0.62),
            RunStage::Planning => 0.80,
            RunStage::Deleting => {
                if progress.planned_deletions > 0 {
                    0.84 + 0.11
                        * (progress.deleted_files as f32 / progress.planned_deletions as f32)
                } else {
                    0.88
                }
            }
            RunStage::Saving => 0.96,
            RunStage::Complete => 1.0,
        },
        None if is_running => 0.02,
        None => 0.0,
    };
    fraction.clamp(0.0, 1.0)
}

fn progress_bar_text(progress: Option<&ProgressUpdate>, is_running: bool) -> String {
    progress
        .map(|progress| progress.message.clone())
        .unwrap_or_else(|| {
            if is_running {
                "Starting background worker...".to_string()
            } else {
                "No active run".to_string()
            }
        })
}

fn run_stage_label(stage: RunStage) -> &'static str {
    match stage {
        RunStage::Preparing => "Preparing",
        RunStage::Discovering => "Discovering",
        RunStage::Scanning => "Scanning",
        RunStage::Hashing => "Hashing",
        RunStage::Planning => "Planning",
        RunStage::Deleting => "Deleting",
        RunStage::Saving => "Saving",
        RunStage::Complete => "Complete",
    }
}

fn progress_counts_text(progress: &ProgressUpdate) -> String {
    match progress.stage {
        RunStage::Scanning => match progress.discovered_files {
            Some(total) if total > 0 => format!("Files: {} / {}", progress.processed_files, total),
            _ => format!("Files: {} scanned", progress.processed_files),
        },
        RunStage::Hashing => match progress.discovered_files {
            Some(total) if total > 0 => format!("Hashed: {} / {}", progress.processed_files, total),
            _ => format!("Hashed: {}", progress.processed_files),
        },
        RunStage::Deleting => {
            format!(
                "Deleted: {} / {}",
                progress.deleted_files, progress.planned_deletions
            )
        }
        RunStage::Planning => format!("Scanned: {} files", progress.processed_files),
        RunStage::Saving => format!(
            "Planned: {} | Deleted: {}",
            progress.planned_deletions, progress.deleted_files
        ),
        RunStage::Complete => format!(
            "Scanned: {} | Deleted: {}",
            progress.processed_files, progress.deleted_files
        ),
        _ => "Waiting for file counts".to_string(),
    }
}

fn progress_eta(progress: &ProgressUpdate, elapsed: Duration) -> Option<Duration> {
    let elapsed_secs = elapsed.as_secs_f32();
    if elapsed_secs <= 0.0 {
        return None;
    }

    match progress.stage {
        RunStage::Scanning => {
            let total = progress.discovered_files?;
            if total == 0 || progress.processed_files == 0 || progress.processed_files >= total {
                return None;
            }
            let per_second = progress.processed_files as f32 / elapsed_secs;
            if per_second <= 0.0 {
                return None;
            }
            let remaining = total.saturating_sub(progress.processed_files) as f32 / per_second;
            Some(Duration::from_secs_f32(remaining.max(0.0)))
        }
        RunStage::Hashing => {
            let total = progress.discovered_files?;
            if total == 0 || progress.processed_files == 0 || progress.processed_files >= total {
                return None;
            }
            let per_second = progress.processed_files as f32 / elapsed_secs;
            if per_second <= 0.0 {
                return None;
            }
            let remaining = total.saturating_sub(progress.processed_files) as f32 / per_second;
            Some(Duration::from_secs_f32(remaining.max(0.0)))
        }
        RunStage::Deleting => {
            if progress.planned_deletions == 0
                || progress.deleted_files == 0
                || progress.deleted_files >= progress.planned_deletions
            {
                return None;
            }
            let per_second = progress.deleted_files as f32 / elapsed_secs;
            if per_second <= 0.0 {
                return None;
            }
            let remaining = progress
                .planned_deletions
                .saturating_sub(progress.deleted_files) as f32
                / per_second;
            Some(Duration::from_secs_f32(remaining.max(0.0)))
        }
        _ => None,
    }
}

fn format_duration(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    if total_seconds < 60 {
        format!("{:.1}s", duration.as_secs_f32())
    } else if total_seconds < 3_600 {
        format!("{:02}:{:02}", total_seconds / 60, total_seconds % 60)
    } else {
        format!(
            "{:02}:{:02}:{:02}",
            total_seconds / 3_600,
            (total_seconds % 3_600) / 60,
            total_seconds % 60
        )
    }
}
fn configure_theme(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.spacing.item_spacing = Vec2::new(12.0, 12.0);
    style.spacing.button_padding = Vec2::new(14.0, 10.0);
    style.visuals = egui::Visuals::dark();
    style.visuals.override_text_color = Some(Color32::from_rgb(232, 238, 247));
    style.visuals.panel_fill = Color32::from_rgb(7, 10, 16);
    style.visuals.window_fill = Color32::from_rgb(11, 15, 23);
    style.visuals.faint_bg_color = Color32::from_rgb(13, 18, 29);
    style.visuals.extreme_bg_color = Color32::from_rgb(4, 7, 12);
    style.visuals.window_stroke = Stroke::new(1.0, Color32::from_rgb(32, 41, 58));
    style.visuals.selection.bg_fill = Color32::from_rgb(58, 128, 196);
    style.visuals.selection.stroke = Stroke::new(1.0, Color32::from_rgb(230, 240, 255));
    style.visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(18, 24, 38);
    style.visuals.widgets.noninteractive.weak_bg_fill = Color32::from_rgb(18, 24, 38);
    style.visuals.widgets.noninteractive.fg_stroke =
        Stroke::new(1.0, Color32::from_rgb(166, 182, 204));
    style.visuals.widgets.inactive.bg_fill = Color32::from_rgb(22, 30, 47);
    style.visuals.widgets.inactive.weak_bg_fill = Color32::from_rgb(22, 30, 47);
    style.visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, Color32::from_rgb(232, 238, 247));
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(32, 43, 67);
    style.visuals.widgets.hovered.weak_bg_fill = Color32::from_rgb(32, 43, 67);
    style.visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, Color32::from_rgb(245, 248, 255));
    style.visuals.widgets.active.bg_fill = Color32::from_rgb(42, 58, 87);
    style.visuals.widgets.active.weak_bg_fill = Color32::from_rgb(42, 58, 87);
    style.visuals.widgets.active.fg_stroke = Stroke::new(1.0, Color32::from_rgb(245, 248, 255));
    style.visuals.widgets.open.bg_fill = Color32::from_rgb(28, 38, 58);
    style.visuals.widgets.open.weak_bg_fill = Color32::from_rgb(28, 38, 58);
    style.visuals.widgets.open.fg_stroke = Stroke::new(1.0, Color32::from_rgb(232, 238, 247));
    ctx.set_style(style);
}

fn card(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    Frame::new()
        .fill(Color32::from_rgb(16, 22, 34))
        .corner_radius(CornerRadius::same(18))
        .stroke(Stroke::new(1.0, Color32::from_rgb(38, 48, 68)))
        .inner_margin(Margin::same(16))
        .show(ui, |ui| {
            ui.label(
                RichText::new(title)
                    .size(16.0)
                    .strong()
                    .color(Color32::from_rgb(236, 240, 248)),
            );
            ui.add_space(10.0);
            add(ui);
        });
    ui.add_space(12.0);
}

fn metric_card(ui: &mut egui::Ui, label: &str, value: usize, accent: Color32) {
    Frame::new()
        .fill(Color32::from_rgb(16, 22, 34))
        .corner_radius(CornerRadius::same(18))
        .stroke(Stroke::new(1.0, Color32::from_rgb(35, 45, 64)))
        .inner_margin(Margin::same(14))
        .show(ui, |ui| {
            ui.set_min_width(140.0);
            ui.label(RichText::new(label).color(Color32::from_rgb(166, 182, 204)));
            ui.label(
                RichText::new(value.to_string())
                    .size(28.0)
                    .strong()
                    .color(accent),
            );
        });
}

fn labeled_field(ui: &mut egui::Ui, label: &str, value: &mut String, hint: &str) {
    ui.label(RichText::new(label).strong());
    ui.add(egui::TextEdit::singleline(value).hint_text(hint));
}

fn render_file_list(
    ui: &mut egui::Ui,
    title: &str,
    files: &[FileInfo],
    filter: &str,
    emphasize: bool,
) {
    ui.label(RichText::new(format!("{} ({})", title, files.len())).strong());
    let needle = filter.trim().to_lowercase();
    let mut shown = 0usize;
    for file in files
        .iter()
        .filter(|file| needle.is_empty() || file.file_path.to_lowercase().contains(&needle))
    {
        Frame::new()
            .fill(if emphasize {
                Color32::from_rgb(44, 28, 18)
            } else {
                Color32::from_rgb(20, 26, 38)
            })
            .corner_radius(CornerRadius::same(14))
            .inner_margin(Margin::same(12))
            .show(ui, |ui| {
                ui.label(RichText::new(&file.file_name).strong());
                ui.label(
                    RichText::new(&file.file_path)
                        .small()
                        .color(Color32::from_rgb(170, 180, 196)),
                );
                ui.label(format!("{} bytes", file.file_size));
            });
        ui.add_space(8.0);
        shown += 1;
        if shown >= 120 {
            break;
        }
    }
    if shown == 0 {
        ui.label("No matching files for the current filter.");
    }
    if files.len() > shown {
        ui.label(
            RichText::new(format!("Showing {} of {} items", shown, files.len()))
                .small()
                .color(Color32::from_rgb(134, 148, 170)),
        );
    }
}

fn status_chip(ui: &mut egui::Ui, label: &str) {
    Frame::new()
        .fill(Color32::from_rgb(30, 63, 54))
        .corner_radius(CornerRadius::same(32))
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.label(
                RichText::new(label)
                    .strong()
                    .color(Color32::from_rgb(179, 247, 220)),
            );
        });
}

fn info_pill(ui: &mut egui::Ui, text: String) {
    Frame::new()
        .fill(Color32::from_rgb(29, 38, 56))
        .corner_radius(CornerRadius::same(32))
        .inner_margin(Margin::symmetric(12, 7))
        .show(ui, |ui| {
            ui.label(RichText::new(text).color(Color32::from_rgb(210, 221, 239)));
        });
}

fn primary_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add(
        egui::Button::new(
            RichText::new(label)
                .strong()
                .color(Color32::from_rgb(7, 10, 16)),
        )
        .fill(Color32::from_rgb(110, 227, 178))
        .min_size(Vec2::new(ui.available_width(), 44.0)),
    )
}

fn split_filters(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .map(|part| part.to_string())
        .collect()
}

fn open_in_file_manager(path: &Path) -> std::io::Result<()> {
    Command::new("explorer").arg(path).spawn()?;
    Ok(())
}
