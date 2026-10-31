use crate::preview::media::{format_duration, VideoPlayer};
use crate::preview::{FileMetadata, PreviewContent, PreviewLoader};
use eframe::egui::{self, Color32, Key, RichText, Stroke, TextureHandle, Vec2};
use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::Arc;
use std::thread;

pub enum IpcCommand {
    Toggle,
    Open(PathBuf),
}

pub struct QuickLookApp {
    ctx: egui::Context,
    current_path: PathBuf,
    siblings: Vec<PathBuf>,
    sibling_index: usize,
    metadata: FileMetadata,
    preview: PreviewContent,
    texture: Option<TextureHandle>,
    /// Animation frames (GPU texture, delay ms) and playback start time
    anim: Vec<(TextureHandle, u32)>,
    anim_start: std::time::Instant,
    /// Real-time streaming video + audio player
    video_player: Option<VideoPlayer>,
    /// Code view layout is computed once and cached rather than rebuilt every frame.
    code_galley: Option<Arc<egui::Galley>>,
    zoom: f32,
    markdown_raw: bool,
    pdf_page: usize,
    ipc_rx: Receiver<IpcCommand>,
    theme_initialized: bool,
    /// Previews load asynchronously in the background so the UI never freezes.
    loading: bool,
    load_gen: u64,
    load_tx: Sender<(u64, PreviewContent)>,
    load_rx: Receiver<(u64, PreviewContent)>,
}

impl QuickLookApp {
    pub fn new(ctx: egui::Context, path: PathBuf, ipc_rx: Receiver<IpcCommand>) -> Self {
        let canonical = std::fs::canonicalize(&path).unwrap_or(path);
        let (siblings, sibling_index) = Self::scan_siblings(&canonical);
        let metadata = FileMetadata::from_path(&canonical);
        let (load_tx, load_rx) = mpsc::channel();

        let mut app = Self {
            ctx,
            current_path: canonical,
            siblings,
            sibling_index,
            metadata,
            preview: PreviewContent::Unsupported {
                reason: String::new(),
                hex_preview: String::new(),
            },
            texture: None,
            anim: Vec::new(),
            anim_start: std::time::Instant::now(),
            video_player: None,
            code_galley: None,
            zoom: 1.0,
            markdown_raw: false,
            pdf_page: 1,
            ipc_rx,
            theme_initialized: false,
            loading: false,
            load_gen: 0,
            load_tx,
            load_rx,
        };
        app.start_load(1);
        app
    }

    /// Prepares the preview on a background thread. Stale results from rapid navigation
    /// are discarded using the `load_gen` generation counter.
    fn start_load(&mut self, pdf_page: usize) {
        self.load_gen += 1;
        self.loading = true;
        let gen = self.load_gen;
        let tx = self.load_tx.clone();
        let ctx = self.ctx.clone();
        let path = self.current_path.clone();
        thread::spawn(move || {
            let content = PreviewLoader::load(&path, pdf_page);
            let _ = tx.send((gen, content));
            ctx.request_repaint();
        });
    }

    fn poll_loader(&mut self) {
        while let Ok((gen, content)) = self.load_rx.try_recv() {
            if gen == self.load_gen {
                self.video_player = None;
                if let PreviewContent::Media {
                    is_video,
                    duration_secs,
                    video_dims,
                    ..
                } = &content
                {
                    self.video_player = Some(VideoPlayer::new(
                        self.current_path.clone(),
                        *is_video,
                        *duration_secs,
                        *video_dims,
                        self.ctx.clone(),
                    ));
                }
                self.preview = content;
                self.texture = None;
                self.anim.clear();
                self.code_galley = None;
                self.loading = false;
            }
        }
    }

    /// Uploads pixel data to the GPU and immediately frees the RAM copy
    /// (prevents keeping two copies of the same image in both RAM and VRAM).
    fn upload_texture(&mut self) {
        if let Some(player) = &mut self.video_player {
            if let Some(frame_rgba) = player.poll_frame() {
                if !frame_rgba.is_empty() {
                    let img = egui::ColorImage::from_rgba_unmultiplied(
                        [player.width, player.height],
                        &frame_rgba,
                    );
                    if let Some(tex) = &mut self.texture {
                        tex.set(img, egui::TextureOptions::LINEAR);
                    } else {
                        self.texture = Some(self.ctx.load_texture(
                            "video_frame",
                            img,
                            egui::TextureOptions::LINEAR,
                        ));
                    }
                }
            }
        }

        if self.texture.is_some() {
            return;
        }
        if let PreviewContent::Animation {
            frames,
            width,
            height,
            ..
        } = &mut self.preview
        {
            let (w, h) = (*width, *height);
            for (i, (rgba, delay)) in std::mem::take(frames).into_iter().enumerate() {
                let img = egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba);
                let tex =
                    self.ctx
                        .load_texture(format!("anim{i}"), img, egui::TextureOptions::LINEAR);
                self.anim.push((tex, delay));
            }
            self.texture = self.anim.first().map(|(t, _)| t.clone());
            self.anim_start = std::time::Instant::now();
            return;
        }
        let data = match &mut self.preview {
            PreviewContent::Image {
                rgba,
                width,
                height,
                ..
            } => Some((std::mem::take(rgba), *width, *height)),
            PreviewContent::Pdf { page_image, .. } => page_image.take(),
            PreviewContent::Media { thumbnail, .. } => thumbnail.take(),
            _ => None,
        };
        if let Some((rgba, w, h)) = data {
            if !rgba.is_empty() {
                let img = egui::ColorImage::from_rgba_unmultiplied([w, h], &rgba);
                self.texture =
                    Some(self.ctx.load_texture("preview", img, egui::TextureOptions::LINEAR));
            }
        }
    }

    fn scan_siblings(path: &Path) -> (Vec<PathBuf>, usize) {
        let Some(parent) = path.parent() else {
            return (vec![path.to_path_buf()], 0);
        };

        let mut list: Vec<(PathBuf, bool)> = std::fs::read_dir(parent)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                let hidden = e.file_name().to_string_lossy().starts_with('.');
                // Skip hidden files unless the opened file itself is hidden
                if hidden && p != path {
                    return None;
                }
                let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
                Some((p, is_dir))
            })
            .collect();

        // Same order as Dolphin: directories first, then natural sort (file2 < file10)
        list.sort_by(|(a, a_dir), (b, b_dir)| {
            b_dir.cmp(a_dir).then_with(|| {
                natural_cmp(
                    &a.file_name().unwrap_or_default().to_string_lossy(),
                    &b.file_name().unwrap_or_default().to_string_lossy(),
                )
            })
        });

        let mut list: Vec<PathBuf> = list.into_iter().map(|(p, _)| p).collect();
        if list.is_empty() {
            list.push(path.to_path_buf());
        }
        let idx = list.iter().position(|p| p == path).unwrap_or(0);
        (list, idx)
    }

    fn switch_to_path(&mut self, path: PathBuf) {
        // Stop any running video stream and drop previous texture handle
        self.video_player = None;
        self.texture = None;
        self.anim.clear();
        self.code_galley = None;
        self.zoom = 1.0;
        self.pdf_page = 1;

        let canonical = std::fs::canonicalize(&path).unwrap_or(path);
        if canonical.parent() != self.current_path.parent()
            || !self.siblings.contains(&canonical)
        {
            let (siblings, idx) = Self::scan_siblings(&canonical);
            self.siblings = siblings;
            self.sibling_index = idx;
        } else if let Some(idx) = self.siblings.iter().position(|p| p == &canonical) {
            self.sibling_index = idx;
        }

        self.metadata = FileMetadata::from_path(&canonical);
        self.current_path = canonical;
        self.start_load(1);
    }

    fn navigate_sibling(&mut self, delta: isize) {
        if self.siblings.len() <= 1 {
            return;
        }
        let len = self.siblings.len() as isize;
        let next = (self.sibling_index as isize + delta).rem_euclid(len) as usize;
        if let Some(target) = self.siblings.get(next).cloned() {
            self.sibling_index = next;
            self.switch_to_path(target);
        }
    }

    fn change_pdf_page(&mut self, new_page: usize) {
        self.pdf_page = new_page.max(1);
        self.start_load(self.pdf_page);
    }

    fn close_window(&mut self, ctx: &egui::Context) {
        self.video_player = None;
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn open_with_default_app(&mut self, ctx: &egui::Context) {
        self.video_player = None;
        let _ = Command::new("xdg-open").arg(&self.current_path).spawn();
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn configure_breeze_style(ctx: &egui::Context) {
        let mut visuals = egui::Visuals::dark();
        // Match KDE Plasma Breeze Dark color palette
        visuals.panel_fill = Color32::from_rgba_unmultiplied(32, 35, 40, 248);
        visuals.window_fill = Color32::from_rgba_unmultiplied(32, 35, 40, 248);
        visuals.extreme_bg_color = Color32::from_rgb(23, 26, 30);
        visuals.selection.bg_fill = Color32::from_rgb(61, 174, 233); // Breeze blue accent
        visuals.widgets.noninteractive.bg_stroke =
            Stroke::new(1.0_f32, Color32::from_rgb(58, 63, 70));
        ctx.set_visuals(visuals);
    }

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        let mut close_requested = false;
        let mut open_external = false;
        let mut nav_delta: isize = 0;
        let mut zoom_delta: f32 = 0.0;
        let mut toggle_pause = false;
        let mut toggle_mute = false;

        ctx.input(|i| {
            if i.key_pressed(Key::Space) || i.key_pressed(Key::Escape) || i.key_pressed(Key::Q) {
                close_requested = true;
            }
            if i.key_pressed(Key::Enter) {
                open_external = true;
            }
            if i.key_pressed(Key::ArrowLeft) {
                nav_delta = -1;
            }
            if i.key_pressed(Key::ArrowRight) {
                nav_delta = 1;
            }
            if i.key_pressed(Key::Plus) || i.key_pressed(Key::Equals) {
                zoom_delta = 0.25;
            }
            if i.key_pressed(Key::Minus) {
                zoom_delta = -0.25;
            }
            if i.key_pressed(Key::K) || i.key_pressed(Key::P) {
                toggle_pause = true;
            }
            if i.key_pressed(Key::M) {
                toggle_mute = true;
            }
        });

        if close_requested {
            self.close_window(ctx);
            return;
        }
        if open_external {
            self.open_with_default_app(ctx);
            return;
        }
        if toggle_pause {
            if let Some(player) = &mut self.video_player {
                player.toggle_pause();
            }
        }
        if toggle_mute {
            if let Some(player) = &mut self.video_player {
                player.toggle_mute();
            }
        }
        if nav_delta != 0 {
            self.navigate_sibling(nav_delta);
        }
        if zoom_delta != 0.0 {
            self.zoom = (self.zoom + zoom_delta).clamp(0.25, 5.0);
        }
    }
}

impl eframe::App for QuickLookApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.12, 0.14, 0.16, 0.97]
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.theme_initialized {
            Self::configure_breeze_style(ctx);
            self.theme_initialized = true;
        }

        // Process IPC messages from the Unix socket (close on Space toggle or switch to new file)
        while let Ok(cmd) = self.ipc_rx.try_recv() {
            match cmd {
                IpcCommand::Toggle => {
                    self.close_window(ctx);
                    return;
                }
                IpcCommand::Open(new_path) => {
                    let canonical = std::fs::canonicalize(&new_path).unwrap_or(new_path);
                    if canonical == self.current_path {
                        // Pressing Space again on the same file closes the preview window
                        self.close_window(ctx);
                        return;
                    } else {
                        self.switch_to_path(canonical);
                        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                    }
                }
            }
        }

        self.handle_shortcuts(ctx);
        self.poll_loader();
        self.upload_texture();

        // Top Header Bar (Draggable QuickLook Header)
        egui::TopBottomPanel::top("header_bar")
            .exact_height(42.0)
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    // Previous / Next file arrows
                    if ui
                        .add_enabled(self.siblings.len() > 1, egui::Button::new("‹"))
                        .on_hover_text("Previous File (Left Arrow)")
                        .clicked()
                    {
                        self.navigate_sibling(-1);
                    }
                    if ui
                        .add_enabled(self.siblings.len() > 1, egui::Button::new("›"))
                        .on_hover_text("Next File (Right Arrow)")
                        .clicked()
                    {
                        self.navigate_sibling(1);
                    }

                    ui.separator();

                    // File name (Click and drag to move window)
                    let title_label = ui.add(
                        egui::Label::new(
                            RichText::new(&self.metadata.file_name)
                                .strong()
                                .size(15.0)
                                .color(Color32::from_rgb(239, 240, 241)),
                        )
                        .sense(egui::Sense::click_and_drag()),
                    );
                    if title_label.drag_started() {
                        ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
                    }

                    // Right-aligned control buttons
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .button(RichText::new("×").color(Color32::from_rgb(237, 106, 94)))
                            .on_hover_text("Close (Space / Esc)")
                            .clicked()
                        {
                            self.close_window(ctx);
                        }

                        if ui
                            .button(
                                RichText::new("Open With App")
                                    .color(Color32::from_rgb(61, 174, 233)),
                            )
                            .on_hover_text("Open in default application (Enter)")
                            .clicked()
                        {
                            self.open_with_default_app(ctx);
                        }

                        // Zoom controls for Image or PDF
                        if matches!(
                            self.preview,
                            PreviewContent::Image { .. }
                                | PreviewContent::Animation { .. }
                                | PreviewContent::Pdf { .. }
                        ) {
                            if ui.button("+").on_hover_text("Zoom In (+)").clicked() {
                                self.zoom = (self.zoom + 0.25).clamp(0.25, 5.0);
                            }
                            ui.label(format!("{:.0}%", self.zoom * 100.0));
                            if ui.button("−").on_hover_text("Zoom Out (-)").clicked() {
                                self.zoom = (self.zoom - 0.25).clamp(0.25, 5.0);
                            }
                        }

                        // Source/Rendered toggle for Markdown
                        if matches!(self.preview, PreviewContent::Code { is_markdown: true, .. }) {
                            ui.toggle_value(&mut self.markdown_raw, "Raw Source");
                        }
                    });
                });
            });

        // Bottom Status Bar (File Size, Type, Modified Date, Shortcut Hints)
        egui::TopBottomPanel::bottom("status_bar")
            .exact_height(28.0)
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "{}  •  {}  •  {}",
                            self.metadata.kind_label,
                            self.metadata.human_size,
                            self.metadata.modified_str
                        ))
                        .size(12.0)
                        .color(Color32::from_rgb(160, 168, 176)),
                    );

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!(
                                "[{}/{}]  Space/Esc: Close  •  ←/→: Navigate  •  Enter: Open",
                                self.sibling_index + 1,
                                self.siblings.len()
                            ))
                            .size(11.5)
                            .color(Color32::from_rgb(125, 133, 144)),
                        );
                    });
                });
            });

        // Main Preview Area
        egui::CentralPanel::default().show(ctx, |ui| {
            if self.loading && self.texture.is_none() {
                ui.centered_and_justified(|ui| {
                    ui.spinner();
                });
                return;
            }
            self.render_preview_body(ui, ctx);
        });
    }
}

impl QuickLookApp {
    fn render_preview_body(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        match &self.preview {
            PreviewContent::Image {
                width,
                height,
                original_dimensions,
                ..
            }
            | PreviewContent::Animation {
                width,
                height,
                original_dimensions,
                ..
            } => {
                // Select active animation frame and schedule repaint for the next frame
                let tex = if self.anim.len() > 1 {
                    let total: u64 = self.anim.iter().map(|(_, d)| *d as u64).sum();
                    let mut t = self.anim_start.elapsed().as_millis() as u64 % total.max(1);
                    let mut chosen = &self.anim[0];
                    for frame in &self.anim {
                        if t < frame.1 as u64 {
                            chosen = frame;
                            break;
                        }
                        t -= frame.1 as u64;
                    }
                    let remaining = (chosen.1 as u64).saturating_sub(t).max(10);
                    ctx.request_repaint_after(std::time::Duration::from_millis(remaining));
                    &chosen.0
                } else {
                    let Some(tex) = &self.texture else {
                        return;
                    };
                    tex
                };

                let avail = ui.available_size();
                let img_w = *width as f32;
                let img_h = *height as f32;
                let base_scale = ((avail.x - 24.0) / img_w)
                    .min((avail.y - 36.0) / img_h)
                    .min(1.0);
                let final_size = Vec2::new(
                    img_w * base_scale * self.zoom,
                    img_h * base_scale * self.zoom,
                );

                ui.vertical_centered(|ui| {
                    ui.label(
                        RichText::new(format!(
                            "Resolution: {} × {} px",
                            original_dimensions.0, original_dimensions.1
                        ))
                        .size(12.0)
                        .color(Color32::GRAY),
                    );
                    egui::ScrollArea::both().show(ui, |ui| {
                        ui.centered_and_justified(|ui| {
                            ui.add(egui::Image::from_texture(
                                egui::load::SizedTexture::new(tex.id(), final_size),
                            ));
                        });
                    });
                });
            }

            PreviewContent::Code {
                highlighted_sections,
                raw_text,
                syntax_name,
                truncated,
                is_markdown,
            } => {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("Language / Format: {syntax_name}"))
                            .size(12.0)
                            .color(Color32::from_rgb(61, 174, 233)),
                    );
                    if *truncated {
                        ui.label(
                            RichText::new(
                                "Note: Showing first 128 KB to keep memory usage low",
                            )
                            .size(12.0)
                            .color(Color32::from_rgb(246, 116, 0)),
                        );
                    }
                });
                ui.separator();

                egui::ScrollArea::both()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        if *is_markdown && !self.markdown_raw {
                            Self::render_simple_markdown(ui, raw_text);
                        } else {
                            let galley = self
                                .code_galley
                                .get_or_insert_with(|| {
                                    let mut job = egui::text::LayoutJob::default();
                                    for (style_color, bg_color, text_piece) in
                                        highlighted_sections
                                    {
                                        job.append(
                                            text_piece,
                                            0.0,
                                            egui::TextFormat {
                                                font_id: egui::FontId::monospace(13.5),
                                                color: *style_color,
                                                background: *bg_color,
                                                ..Default::default()
                                            },
                                        );
                                    }
                                    ui.fonts(|f| f.layout_job(job))
                                })
                                .clone();
                            ui.add(egui::Label::new(galley).selectable(true));
                        }
                    });
            }

            PreviewContent::Pdf {
                current_page,
                total_pages,
                info_lines,
                ..
            } => {
                let cur_page = *current_page;
                let tot_pages = *total_pages;
                let info_clone = info_lines.clone();
                let has_img = self.texture.is_some();

                let mut requested_page: Option<usize> = None;
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(cur_page > 1, egui::Button::new("‹ Previous Page"))
                        .clicked()
                    {
                        requested_page = Some(cur_page - 1);
                    }
                    ui.label(
                        RichText::new(format!(
                            "Page {} / {}",
                            cur_page,
                            tot_pages.max(cur_page)
                        ))
                        .strong(),
                    );
                    if ui
                        .add_enabled(
                            tot_pages == 0 || cur_page < tot_pages,
                            egui::Button::new("Next Page ›"),
                        )
                        .clicked()
                    {
                        requested_page = Some(cur_page + 1);
                    }
                });
                ui.separator();

                if has_img {
                    if let Some(tex) = &self.texture {
                        let tex_size = tex.size_vec2();
                        let avail = ui.available_size();
                        let scale = ((avail.x - 24.0) / tex_size.x)
                            .min((avail.y - 24.0) / tex_size.y)
                            .min(1.0)
                            * self.zoom;
                        let draw_size = tex_size * scale;

                        egui::ScrollArea::both().show(ui, |ui| {
                            ui.centered_and_justified(|ui| {
                                ui.add(egui::Image::from_texture(
                                    egui::load::SizedTexture::new(tex.id(), draw_size),
                                ));
                            });
                        });
                    }
                } else {
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        for line in &info_clone {
                            ui.label(RichText::new(line).monospace().size(13.0));
                        }
                    });
                }

                if let Some(p) = requested_page {
                    self.change_pdf_page(p);
                }
            }

            PreviewContent::Media {
                kind_title,
                is_video,
                metadata_pairs,
                ..
            } => {
                let is_vid = *is_video;
                let title_clone = kind_title.clone();
                let meta_clone = metadata_pairs.clone();

                ui.vertical_centered(|ui| {
                    if !is_vid {
                        ui.add_space(6.0);
                        ui.heading(
                            RichText::new(&title_clone).color(Color32::from_rgb(61, 174, 233)),
                        );
                        ui.add_space(6.0);
                    }

                    if let Some(tex) = &self.texture {
                        let size = tex.size_vec2();
                        let cap_w = if is_vid { 780.0 } else { 520.0 };
                        let max_w = (ui.available_width() - 24.0).min(cap_w);
                        let max_h = (ui.available_height() * if is_vid { 0.68 } else { 0.50 })
                            .min(if is_vid { 440.0 } else { 320.0 });
                        let scale = (max_w / size.x).min(max_h / size.y).min(1.0);
                        let img_resp = ui.add(
                            egui::Image::from_texture(egui::load::SizedTexture::new(
                                tex.id(),
                                size * scale,
                            ))
                            .rounding(6.0)
                            .sense(egui::Sense::click()),
                        );
                        if is_vid && img_resp.clicked() {
                            if let Some(player) = &mut self.video_player {
                                player.toggle_pause();
                            }
                        }
                        ui.add_space(8.0);
                    }

                    // Interactive Video Player Controls Bar
                    if let Some(player) = &mut self.video_player {
                        egui::Frame::none()
                            .fill(Color32::from_rgb(24, 27, 32))
                            .stroke(Stroke::new(1.0_f32, Color32::from_rgb(52, 57, 65)))
                            .rounding(6.0)
                            .inner_margin(egui::Margin::symmetric(12.0, 6.0))
                            .show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    let play_label =
                                        if player.paused { "▶ Play" } else { "⏸ Pause" };
                                    if ui
                                        .button(RichText::new(play_label).strong().size(13.0))
                                        .clicked()
                                    {
                                        player.toggle_pause();
                                    }

                                    if ui.button("⏪ -5s").clicked() {
                                        player.seek(player.position - 5.0);
                                    }
                                    if ui.button("+5s ⏩").clicked() {
                                        player.seek(player.position + 5.0);
                                    }

                                    let mute_label =
                                        if player.muted { "🔇 Muted" } else { "🔊 Audio" };
                                    if ui.button(mute_label).clicked() {
                                        player.toggle_mute();
                                    }

                                    if player.duration > 0.0 {
                                        let mut pos = player.position;
                                        let dur = player.duration;
                                        ui.spacing_mut().slider_width =
                                            (ui.available_width() - 115.0).max(80.0);
                                        let slider_resp = ui.add(
                                            egui::Slider::new(&mut pos, 0.0..=dur)
                                                .show_value(false),
                                        );
                                        if slider_resp.changed() {
                                            player.seek(pos);
                                        }
                                        ui.label(
                                            RichText::new(format!(
                                                "{} / {}",
                                                format_duration(player.position),
                                                format_duration(dur)
                                            ))
                                            .monospace()
                                            .size(12.0)
                                            .color(Color32::from_rgb(180, 188, 198)),
                                        );
                                    }
                                });
                            });
                        ui.add_space(8.0);
                    } else if ui
                        .button(RichText::new("Play in Media Player").size(14.0).strong())
                        .clicked()
                    {
                        let _ = Command::new("xdg-open").arg(&self.current_path).spawn();
                    }

                    egui::Grid::new("media_meta_grid")
                        .striped(true)
                        .spacing([20.0, 4.0])
                        .show(ui, |ui| {
                            for (k, v) in &meta_clone {
                                ui.label(
                                    RichText::new(k)
                                        .strong()
                                        .size(12.5)
                                        .color(Color32::from_rgb(160, 168, 176)),
                                );
                                ui.label(RichText::new(v).monospace().size(12.5));
                                ui.end_row();
                            }
                        });
                });
            }

            PreviewContent::DirectoryOrArchive {
                summary,
                entries,
            } => {
                ui.label(
                    RichText::new(summary)
                        .strong()
                        .size(14.0)
                        .color(Color32::from_rgb(61, 174, 233)),
                );
                ui.separator();

                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        egui::Grid::new("archive_entries_grid")
                            .striped(true)
                            .num_columns(2)
                            .spacing([24.0, 4.0])
                            .show(ui, |ui| {
                                for (name, size_str) in entries {
                                    ui.label(RichText::new(name).monospace().size(13.0));
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            ui.label(
                                                RichText::new(size_str)
                                                    .monospace()
                                                    .size(12.0)
                                                    .color(Color32::GRAY),
                                            );
                                        },
                                    );
                                    ui.end_row();
                                }
                            });
                    });
            }

            PreviewContent::Unsupported { reason, hex_preview } => {
                ui.vertical_centered(|ui| {
                    ui.add_space(16.0);
                    ui.label(RichText::new(&self.metadata.icon).size(48.0));
                    ui.heading(&self.metadata.file_name);
                    ui.label(RichText::new(reason).color(Color32::GRAY));
                    ui.add_space(12.0);
                });

                if !hex_preview.is_empty() {
                    ui.separator();
                    ui.label(
                        RichText::new("Binary (Hex) Header Preview (First 512 Bytes):")
                            .size(12.0)
                            .color(Color32::GRAY),
                    );
                    egui::ScrollArea::vertical().show(ui, |ui| {
                        ui.add(
                            egui::Label::new(
                                RichText::new(hex_preview)
                                    .monospace()
                                    .size(12.5)
                                    .color(Color32::from_rgb(190, 198, 206)),
                            )
                            .selectable(true),
                        );
                    });
                }
            }
        }
    }

    fn render_simple_markdown(ui: &mut egui::Ui, markdown: &str) {
        let mut in_code_block = false;
        let mut code_block_buf = String::new();
        let mut table_rows: Vec<Vec<String>> = Vec::new();
        let mut table_id = 0usize;

        let flush_table = |ui: &mut egui::Ui, rows: &mut Vec<Vec<String>>, id: &mut usize| {
            if rows.is_empty() {
                return;
            }
            *id += 1;
            egui::Frame::none()
                .fill(Color32::from_rgb(26, 29, 34))
                .stroke(Stroke::new(1.0_f32, Color32::from_rgb(52, 57, 65)))
                .rounding(4.0)
                .inner_margin(egui::Margin::symmetric(8.0, 6.0))
                .show(ui, |ui| {
                    egui::Grid::new(format!("md_table_{}", *id))
                        .striped(true)
                        .spacing([18.0, 6.0])
                        .show(ui, |ui| {
                            for (r_idx, row) in rows.iter().enumerate() {
                                for cell in row {
                                    let color = if r_idx == 0 {
                                        Color32::from_rgb(130, 200, 245)
                                    } else {
                                        Color32::from_rgb(220, 224, 230)
                                    };
                                    let job = Self::markdown_inline_job(
                                        cell.trim(),
                                        13.5,
                                        color,
                                        r_idx == 0,
                                        false,
                                    );
                                    ui.add(egui::Label::new(job).selectable(true));
                                }
                                ui.end_row();
                            }
                        });
                });
            ui.add_space(4.0);
            rows.clear();
        };

        for line in markdown.lines() {
            let trimmed_start = line.trim_start();

            if trimmed_start.starts_with("```") {
                flush_table(ui, &mut table_rows, &mut table_id);
                if in_code_block {
                    let text = std::mem::take(&mut code_block_buf);
                    egui::Frame::none()
                        .fill(Color32::from_rgb(21, 24, 28))
                        .stroke(Stroke::new(1.0_f32, Color32::from_rgb(52, 57, 65)))
                        .rounding(5.0)
                        .inner_margin(egui::Margin::symmetric(10.0, 8.0))
                        .show(ui, |ui| {
                            ui.set_min_width(ui.available_width());
                            ui.add(
                                egui::Label::new(
                                    RichText::new(text.trim_end_matches('\n'))
                                        .monospace()
                                        .size(13.0)
                                        .color(Color32::from_rgb(205, 214, 224)),
                                )
                                .selectable(true),
                            );
                        });
                    ui.add_space(4.0);
                    in_code_block = false;
                } else {
                    in_code_block = true;
                }
                continue;
            }

            if in_code_block {
                code_block_buf.push_str(line);
                code_block_buf.push('\n');
                continue;
            }

            // Markdown table rows (`| col1 | col2 |`)
            let trimmed = line.trim();
            if trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.len() > 2 {
                let cells: Vec<String> = trimmed[1..trimmed.len() - 1]
                    .split('|')
                    .map(|c| c.trim().to_string())
                    .collect();
                let is_sep = cells.iter().all(|c| {
                    !c.is_empty() && c.chars().all(|ch| ch == '-' || ch == ':' || ch == ' ')
                });
                if !is_sep {
                    table_rows.push(cells);
                }
                continue;
            } else {
                flush_table(ui, &mut table_rows, &mut table_id);
            }

            if trimmed.is_empty() {
                ui.add_space(5.0);
                continue;
            }

            // Horizontal rules (`---`, `***`, `___`)
            if matches!(trimmed, "---" | "***" | "___")
                || (trimmed.len() >= 3
                    && (trimmed.chars().all(|c| c == '-') || trimmed.chars().all(|c| c == '*')))
            {
                ui.add_space(4.0);
                ui.separator();
                ui.add_space(4.0);
                continue;
            }

            // Headings (`# ` .. `###### `)
            if let Some((level, heading_text)) = Self::parse_heading(trimmed_start) {
                let (size, color, top_space, sep) = match level {
                    1 => (22.0, Color32::from_rgb(61, 174, 233), 8.0, true),
                    2 => (18.5, Color32::from_rgb(130, 200, 245), 6.0, false),
                    3 => (16.0, Color32::from_rgb(239, 240, 241), 4.0, false),
                    _ => (14.5, Color32::from_rgb(200, 208, 218), 3.0, false),
                };
                ui.add_space(top_space);
                let job = Self::markdown_inline_job(heading_text, size, color, true, false);
                ui.add(egui::Label::new(job).selectable(true));
                if sep {
                    ui.separator();
                }
                continue;
            }

            // Blockquote (`> `)
            if let Some(quote) = trimmed_start
                .strip_prefix("> ")
                .or_else(|| (trimmed_start == ">").then_some(""))
            {
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new("▎ ")
                            .size(14.5)
                            .color(Color32::from_rgb(61, 174, 233)),
                    );
                    let job = Self::markdown_inline_job(
                        quote,
                        14.0,
                        Color32::from_rgb(180, 188, 198),
                        false,
                        true,
                    );
                    ui.add(egui::Label::new(job).selectable(true));
                });
                continue;
            }

            // Bullet or task list (`- `, `* `, `+ `)
            let indent_spaces = line.len() - trimmed_start.len();
            if let Some(bullet) = trimmed_start
                .strip_prefix("- ")
                .or_else(|| trimmed_start.strip_prefix("* "))
                .or_else(|| trimmed_start.strip_prefix("+ "))
            {
                let (marker, item_text) = if let Some(rest) = bullet.strip_prefix("[ ] ") {
                    ("☐ ", rest)
                } else if let Some(rest) = bullet
                    .strip_prefix("[x] ")
                    .or_else(|| bullet.strip_prefix("[X] "))
                {
                    ("☑ ", rest)
                } else {
                    ("• ", bullet)
                };
                let pad = " ".repeat(indent_spaces.min(12) + 2);
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(format!("{pad}{marker}"))
                            .size(14.0)
                            .color(Color32::from_rgb(61, 174, 233)),
                    );
                    let job = Self::markdown_inline_job(
                        item_text,
                        14.0,
                        Color32::from_rgb(222, 226, 232),
                        false,
                        false,
                    );
                    ui.add(egui::Label::new(job).selectable(true));
                });
                continue;
            }

            // Numbered list (`1. `, `2. `, ...)
            if let Some((num_prefix, rest)) = Self::parse_numbered_list(trimmed_start) {
                let pad = " ".repeat(indent_spaces.min(12) + 2);
                ui.horizontal_wrapped(|ui| {
                    ui.label(
                        RichText::new(format!("{pad}{num_prefix}. "))
                            .strong()
                            .size(14.0)
                            .color(Color32::from_rgb(61, 174, 233)),
                    );
                    let job = Self::markdown_inline_job(
                        rest,
                        14.0,
                        Color32::from_rgb(222, 226, 232),
                        false,
                        false,
                    );
                    ui.add(egui::Label::new(job).selectable(true));
                });
                continue;
            }

            // Regular paragraph with inline Markdown formatting
            let job = Self::markdown_inline_job(
                line,
                14.0,
                Color32::from_rgb(222, 226, 232),
                false,
                false,
            );
            ui.add(egui::Label::new(job).selectable(true));
        }

        flush_table(ui, &mut table_rows, &mut table_id);
    }

    fn parse_heading(line: &str) -> Option<(usize, &str)> {
        let hashes = line.bytes().take_while(|&b| b == b'#').count();
        if (1..=6).contains(&hashes) {
            let rest = &line[hashes..];
            if let Some(stripped) = rest.strip_prefix(' ') {
                return Some((hashes, stripped.trim()));
            }
        }
        None
    }

    fn parse_numbered_list(line: &str) -> Option<(&str, &str)> {
        let digits = line.bytes().take_while(|b| b.is_ascii_digit()).count();
        if digits > 0 {
            let after_digits = &line[digits..];
            if let Some(rest) = after_digits.strip_prefix(". ") {
                return Some((&line[..digits], rest));
            }
        }
        None
    }

    /// Parses inline Markdown (`**bold**`, `__bold__`, `*italic*`, `_italic_`, `` `code` ``,
    /// `~~strikethrough~~`, and `[label](url)`) into a styled `egui::text::LayoutJob`.
    fn markdown_inline_job(
        text: &str,
        size: f32,
        base_color: Color32,
        force_bold: bool,
        force_italic: bool,
    ) -> egui::text::LayoutJob {
        let mut job = egui::text::LayoutJob::default();
        let bold_family = egui::FontFamily::Name("bold".into());

        let append_styled = |job: &mut egui::text::LayoutJob,
                             s: &str,
                             bold: bool,
                             italic: bool,
                             code: bool,
                             strike: bool,
                             link: bool| {
            if s.is_empty() {
                return;
            }
            let is_bold = bold || force_bold;
            let is_italic = italic || force_italic;
            let font_id = if code {
                egui::FontId::monospace((size - 0.5).max(11.0))
            } else if is_bold {
                egui::FontId::new(size, bold_family.clone())
            } else {
                egui::FontId::proportional(size)
            };

            let color = if code {
                Color32::from_rgb(246, 193, 119)
            } else if link {
                Color32::from_rgb(61, 174, 233)
            } else if is_bold && !force_bold {
                Color32::WHITE
            } else {
                base_color
            };

            let background = if code {
                Color32::from_rgb(45, 50, 58)
            } else {
                Color32::TRANSPARENT
            };

            let underline = if link {
                Stroke::new(1.0_f32, Color32::from_rgb(61, 174, 233))
            } else {
                Stroke::NONE
            };

            let strikethrough = if strike {
                Stroke::new(1.0_f32, color)
            } else {
                Stroke::NONE
            };

            job.append(
                s,
                0.0,
                egui::TextFormat {
                    font_id,
                    color,
                    background,
                    italics: is_italic,
                    underline,
                    strikethrough,
                    ..Default::default()
                },
            );
        };

        let mut rest = text;
        let mut plain = String::new();

        while !rest.is_empty() {
            // Inline code: `...`
            if let Some(after_tick) = rest.strip_prefix('`') {
                if let Some(end) = after_tick.find('`') {
                    append_styled(&mut job, &plain, false, false, false, false, false);
                    plain.clear();
                    append_styled(&mut job, &after_tick[..end], false, false, true, false, false);
                    rest = &after_tick[end + 1..];
                    continue;
                }
            }

            // Bold + Italic: ***...***
            if let Some(after) = rest.strip_prefix("***") {
                if let Some(end) = after.find("***") {
                    if end > 0 {
                        append_styled(&mut job, &plain, false, false, false, false, false);
                        plain.clear();
                        append_styled(&mut job, &after[..end], true, true, false, false, false);
                        rest = &after[end + 3..];
                        continue;
                    }
                }
            }

            // Bold: **...** or __...__
            if let Some((delim, after)) = rest
                .strip_prefix("**")
                .map(|a| ("**", a))
                .or_else(|| rest.strip_prefix("__").map(|a| ("__", a)))
            {
                if let Some(end) = after.find(delim) {
                    if end > 0 {
                        append_styled(&mut job, &plain, false, false, false, false, false);
                        plain.clear();
                        append_styled(&mut job, &after[..end], true, false, false, false, false);
                        rest = &after[end + 2..];
                        continue;
                    }
                }
            }

            // Strikethrough: ~~...~~
            if let Some(after) = rest.strip_prefix("~~") {
                if let Some(end) = after.find("~~") {
                    if end > 0 {
                        append_styled(&mut job, &plain, false, false, false, false, false);
                        plain.clear();
                        append_styled(&mut job, &after[..end], false, false, false, true, false);
                        rest = &after[end + 2..];
                        continue;
                    }
                }
            }

            // Markdown link: [label](url)
            if let Some(after_bracket) = rest.strip_prefix('[') {
                if let Some(close_bracket) = after_bracket.find("](") {
                    let after_paren = &after_bracket[close_bracket + 2..];
                    if let Some(close_paren) = after_paren.find(')') {
                        let label = &after_bracket[..close_bracket];
                        append_styled(&mut job, &plain, false, false, false, false, false);
                        plain.clear();
                        append_styled(&mut job, label, false, false, false, false, true);
                        rest = &after_paren[close_paren + 1..];
                        continue;
                    }
                }
            }

            // Italic: *...* or _..._
            if let Some((delim, after)) = rest
                .strip_prefix('*')
                .map(|a| ('*', a))
                .or_else(|| rest.strip_prefix('_').map(|a| ('_', a)))
            {
                if let Some(end) = after.find(delim) {
                    if end > 0 && !after[..end].starts_with(' ') && !after[..end].ends_with(' ') {
                        append_styled(&mut job, &plain, false, false, false, false, false);
                        plain.clear();
                        append_styled(&mut job, &after[..end], false, true, false, false, false);
                        rest = &after[end + 1..];
                        continue;
                    }
                }
            }

            let ch = rest.chars().next().unwrap();
            plain.push(ch);
            rest = &rest[ch.len_utf8()..];
        }

        append_styled(&mut job, &plain, false, false, false, false, false);
        job
    }
}

/// Human-friendly natural sort: "file2" < "file10", case-insensitive.
fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut ai, mut bi) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return a.cmp(b),
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let mut na = String::new();
                while let Some(c) = ai.peek().copied().filter(char::is_ascii_digit) {
                    na.push(c);
                    ai.next();
                }
                let mut nb = String::new();
                while let Some(c) = bi.peek().copied().filter(char::is_ascii_digit) {
                    nb.push(c);
                    bi.next();
                }
                let ta = na.trim_start_matches('0');
                let tb = nb.trim_start_matches('0');
                let ord = ta.len().cmp(&tb.len()).then_with(|| ta.cmp(tb));
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            (Some(x), Some(y)) => {
                let ord = x.to_lowercase().cmp(y.to_lowercase());
                if ord != Ordering::Equal {
                    return ord;
                }
                ai.next();
                bi.next();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_natural_cmp() {
        let mut v = vec!["file10.txt", "File2.txt", "file1.txt", "b.txt", "A.txt"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, ["A.txt", "b.txt", "file1.txt", "File2.txt", "file10.txt"]);
    }

    #[test]
    fn test_markdown_inline_strips_syntax() {
        let job = QuickLookApp::markdown_inline_job(
            "Hello **deneme** and *italic* with `code` and [link](https://example.com)!",
            14.0,
            Color32::WHITE,
            false,
            false,
        );
        assert_eq!(job.text, "Hello deneme and italic with code and link!");
        assert!(job.sections.len() >= 7);
    }
}
