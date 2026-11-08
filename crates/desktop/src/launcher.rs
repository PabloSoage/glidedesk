//! The connection window shown when the client starts without a host, in the manner of
//! `mstsc`: computer (with the recent ones) and user, and the options in tabs. It builds the
//! same [`Args`] as the command line, and saves and opens them as `.glidedesk` profiles.

use std::path::{Path, PathBuf};

use egui::{Color32, Vec2};
use egui_wgpu::ScreenDescriptor;
use winit::window::Window;

use crate::profile::{History, KnownHosts, Profile, config_dir};
use crate::{Args, CodecChoice, decode, platform};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tab {
    General,
    Display,
    Performance,
    LocalResources,
    Advanced,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ResolutionPreset {
    R720p,
    R1080p,
    R1440p,
    R4k,
    Custom,
}

impl ResolutionPreset {
    const ALL: [(Self, &'static str); 5] = [
        (Self::R720p, "1280 × 720"),
        (Self::R1080p, "1920 × 1080"),
        (Self::R1440p, "2560 × 1440"),
        (Self::R4k, "3840 × 2160"),
        (Self::Custom, "Custom"),
    ];

    fn label(self) -> &'static str {
        Self::ALL
            .iter()
            .find(|(p, _)| *p == self)
            .map_or("", |(_, l)| l)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GpuOption {
    Auto,
    Amd,
    Intel,
    Nvidia,
}

pub struct Launcher {
    pub host: String,
    pub user: String,
    pub port_str: String,
    pub rdp: bool,
    pub show_options: bool,
    pub selected_tab: Tab,
    pub resolution: ResolutionPreset,
    pub custom_width: u32,
    pub custom_height: u32,
    pub fullscreen: bool,
    pub codec: CodecChoice,
    pub gpu: GpuOption,
    pub software_decode: bool,
    pub mute: bool,
    pub clipboard: bool,
    pub history: History,
    pub known_hosts: KnownHosts,
    pub fingerprint: String,
    pub profile_path: String,
    pub status_msg: Option<(String, bool)>,
}

impl Launcher {
    pub fn new() -> Self {
        let history = History::load();
        let known_hosts = KnownHosts::load();
        let host = history.hosts.first().cloned().unwrap_or_default();
        let user = history.last_user.clone().unwrap_or_default();
        let default_profile = config_dir()
            .join("default.glidedesk")
            .to_string_lossy()
            .to_string();
        Self {
            host,
            user,
            port_str: String::new(),
            rdp: false,
            show_options: false,
            selected_tab: Tab::General,
            resolution: ResolutionPreset::R720p,
            custom_width: 1280,
            custom_height: 720,
            fullscreen: false,
            codec: CodecChoice::Auto,
            gpu: GpuOption::Auto,
            software_decode: false,
            mute: false,
            clipboard: true,
            history,
            known_hosts,
            fingerprint: String::new(),
            profile_path: default_profile,
            status_msg: None,
        }
    }

    fn size(&self) -> (u32, u32) {
        match self.resolution {
            ResolutionPreset::R720p => (1280, 720),
            ResolutionPreset::R1080p => (1920, 1080),
            ResolutionPreset::R1440p => (2560, 1440),
            ResolutionPreset::R4k => (3840, 2160),
            ResolutionPreset::Custom => (self.custom_width.max(200), self.custom_height.max(200)),
        }
    }

    fn port(&self) -> Result<Option<u16>, String> {
        let p = self.port_str.trim();
        if p.is_empty() {
            return Ok(None);
        }
        p.parse()
            .map(Some)
            .map_err(|_| format!("{p:?} is not a port number"))
    }

    fn fingerprint(&self) -> Option<String> {
        let f = self.fingerprint.trim();
        (!f.is_empty()).then(|| f.to_owned())
    }

    fn user(&self) -> Option<String> {
        let u = self.user.trim();
        (!u.is_empty()).then(|| u.to_owned())
    }

    pub fn load_profile(&mut self, path: &Path) -> Result<(), String> {
        let p = Profile::load_file(path).map_err(|e| format!("{e:#}"))?;
        self.host = p.host;
        if let Some(u) = p.user {
            self.user = u;
        }
        self.port_str = p.port.map(|p| p.to_string()).unwrap_or_default();
        self.rdp = p.rdp.unwrap_or(false);
        if let (Some(w), Some(h)) = (p.width, p.height) {
            self.custom_width = w;
            self.custom_height = h;
            self.resolution = match (w, h) {
                (1280, 720) => ResolutionPreset::R720p,
                (1920, 1080) => ResolutionPreset::R1080p,
                (2560, 1440) => ResolutionPreset::R1440p,
                (3840, 2160) => ResolutionPreset::R4k,
                _ => ResolutionPreset::Custom,
            };
        }
        self.fullscreen = p.fullscreen.unwrap_or(false);
        self.mute = p.mute.unwrap_or(false);
        self.software_decode = p.software_decode.unwrap_or(false);
        self.codec = p.codec.unwrap_or(CodecChoice::Auto);
        self.clipboard = p.clipboard.unwrap_or(true);
        self.gpu = match p.gpu.as_deref().map(str::to_ascii_lowercase).as_deref() {
            Some("amd") => GpuOption::Amd,
            Some("intel") => GpuOption::Intel,
            Some("nvidia") => GpuOption::Nvidia,
            _ => GpuOption::Auto,
        };
        self.fingerprint = p.fingerprint.unwrap_or_default();
        self.profile_path = path.to_string_lossy().to_string();
        self.status_msg = Some((format!("Opened {}", path.display()), false));
        Ok(())
    }

    pub fn save_profile(&self, path: &Path) -> Result<(), String> {
        let (w, h) = self.size();
        let p = Profile {
            host: self.host.trim().to_owned(),
            user: self.user(),
            port: self.port()?,
            rdp: self.rdp.then_some(true),
            width: Some(w),
            height: Some(h),
            fullscreen: self.fullscreen.then_some(true),
            mute: self.mute.then_some(true),
            software_decode: self.software_decode.then_some(true),
            gpu: match self.gpu {
                GpuOption::Auto => None,
                GpuOption::Amd => Some("amd".into()),
                GpuOption::Intel => Some("intel".into()),
                GpuOption::Nvidia => Some("nvidia".into()),
            },
            fingerprint: self.fingerprint(),
            codec: (self.codec != CodecChoice::Auto).then_some(self.codec),
            clipboard: (!self.clipboard).then_some(false),
        };
        p.save_file(path).map_err(|e| format!("{e:#}"))
    }

    /// The options as command line arguments, or what is wrong with them.
    pub fn build_args(&mut self) -> Result<Args, String> {
        let host = self.host.trim().to_owned();
        if host.is_empty() {
            return Err("Enter a computer name or address".into());
        }
        let port = self.port()?;
        let fingerprint = self.fingerprint();
        if let Some(f) = &fingerprint {
            glidedesk_client::trust::parse_fingerprint(f)?;
        }
        let (width, height) = self.size();
        self.history.add(&host, self.user().as_deref());
        if let Err(e) = self.history.save() {
            tracing::warn!(error = format!("{e:#}"), "Could not save the history");
        }
        Ok(Args {
            host: Some(host),
            user: self.user(),
            password: None,
            port,
            rdp: self.rdp,
            width,
            height,
            fullscreen: self.fullscreen,
            mute: self.mute,
            software_decode: self.software_decode,
            gpu: match self.gpu {
                GpuOption::Auto => None,
                GpuOption::Amd => Some(0x1002),
                GpuOption::Intel => Some(0x8086),
                GpuOption::Nvidia => Some(decode::VENDOR_NVIDIA),
            },
            fingerprint,
            codec: self.codec,
            no_clipboard: !self.clipboard,
        })
    }
}

pub enum LauncherAction {
    None,
    Connect(Box<Args>),
    Exit,
}

pub fn render_ui(launcher: &mut Launcher, ui: &mut egui::Ui) -> LauncherAction {
    let mut action = LauncherAction::None;

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.heading("glidedesk");
        ui.label(
            egui::RichText::new("Remote desktop connection")
                .italics()
                .color(Color32::from_gray(180)),
        );
    });
    ui.add_space(4.0);
    ui.separator();
    ui.add_space(6.0);

    ui.group(|ui| {
        egui::Grid::new("quick_conn_grid")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                ui.label(egui::RichText::new("Computer:").strong());
                ui.horizontal(|ui| {
                    ui.add_sized(
                        [220.0, 22.0],
                        egui::TextEdit::singleline(&mut launcher.host).hint_text("name or address"),
                    );
                    if !launcher.history.hosts.is_empty() {
                        egui::ComboBox::from_id_salt("history_combo")
                            .selected_text("Recent")
                            .width(120.0)
                            .show_ui(ui, |ui| {
                                for h in &launcher.history.hosts {
                                    if ui.selectable_label(launcher.host == *h, h).clicked() {
                                        launcher.host = h.clone();
                                    }
                                }
                            });
                    }
                });
                ui.end_row();

                ui.label(egui::RichText::new("User:").strong());
                ui.add_sized(
                    [220.0, 22.0],
                    egui::TextEdit::singleline(&mut launcher.user)
                        .hint_text("asked for when empty"),
                );
                ui.end_row();
            });

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let connect = egui::Button::new(
                egui::RichText::new(" Connect ")
                    .strong()
                    .color(Color32::WHITE),
            )
            .fill(Color32::from_rgb(0, 102, 204))
            .min_size(Vec2::new(100.0, 28.0));
            if ui.add(connect).clicked() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                match launcher.build_args() {
                    Ok(args) => action = LauncherAction::Connect(Box::new(args)),
                    Err(e) => launcher.status_msg = Some((e, true)),
                }
            }
            let options = if launcher.show_options {
                "Hide options"
            } else {
                "Show options"
            };
            if ui.button(options).clicked() {
                launcher.show_options = !launcher.show_options;
            }
            if ui.button("Quit").clicked() {
                action = LauncherAction::Exit;
            }
        });
    });

    if let Some((msg, is_err)) = &launcher.status_msg {
        ui.add_space(4.0);
        let col = if *is_err {
            Color32::RED
        } else {
            Color32::from_rgb(50, 180, 50)
        };
        ui.label(egui::RichText::new(msg).color(col));
    }

    if !launcher.show_options {
        return action;
    }

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.selectable_value(&mut launcher.selected_tab, Tab::General, "General");
        ui.selectable_value(&mut launcher.selected_tab, Tab::Display, "Display");
        ui.selectable_value(&mut launcher.selected_tab, Tab::Performance, "Performance");
        ui.selectable_value(
            &mut launcher.selected_tab,
            Tab::LocalResources,
            "Local resources",
        );
        ui.selectable_value(&mut launcher.selected_tab, Tab::Advanced, "Advanced");
    });
    ui.separator();
    ui.add_space(4.0);

    egui::ScrollArea::vertical()
        .max_height(240.0)
        .show(ui, |ui| match launcher.selected_tab {
            Tab::General => {
                ui.group(|ui| {
                    ui.label(egui::RichText::new("Connection").strong());
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut launcher.rdp, false, "glidedesk (low latency)");
                        ui.radio_value(&mut launcher.rdp, true, "RDP");
                    });
                    ui.horizontal(|ui| {
                        ui.label("Port:");
                        ui.add_sized(
                            [80.0, 20.0],
                            egui::TextEdit::singleline(&mut launcher.port_str)
                                .hint_text(if launcher.rdp { "3389" } else { "3391" }),
                        );
                    });
                });
                ui.add_space(6.0);
                ui.group(|ui| {
                    ui.label(egui::RichText::new("Connection profile (.glidedesk)").strong());
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label("File:");
                        ui.add_sized(
                            [260.0, 20.0],
                            egui::TextEdit::singleline(&mut launcher.profile_path),
                        );
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Save").clicked() {
                            let p = PathBuf::from(&launcher.profile_path);
                            launcher.status_msg = Some(match launcher.save_profile(&p) {
                                Ok(()) => (format!("Saved {}", p.display()), false),
                                Err(e) => (format!("Could not save: {e}"), true),
                            });
                        }
                        if ui.button("Open").clicked() {
                            let p = PathBuf::from(&launcher.profile_path);
                            if let Err(e) = launcher.load_profile(&p) {
                                launcher.status_msg = Some((format!("Could not open: {e}"), true));
                            }
                        }
                    });
                });
            }

            Tab::Display => {
                ui.group(|ui| {
                    ui.label(egui::RichText::new("Display").strong());
                    ui.add_space(4.0);
                    ui.checkbox(&mut launcher.fullscreen, "Full screen");
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label("Size:");
                        egui::ComboBox::from_id_salt("res_combo")
                            .selected_text(launcher.resolution.label())
                            .show_ui(ui, |ui| {
                                for (preset, label) in ResolutionPreset::ALL {
                                    ui.selectable_value(&mut launcher.resolution, preset, label);
                                }
                            });
                    });
                    if launcher.resolution == ResolutionPreset::Custom {
                        ui.horizontal(|ui| {
                            ui.label("Width:");
                            ui.add(
                                egui::DragValue::new(&mut launcher.custom_width).range(320..=7680),
                            );
                            ui.label("Height:");
                            ui.add(
                                egui::DragValue::new(&mut launcher.custom_height).range(240..=4320),
                            );
                        });
                    }
                    ui.label(
                        egui::RichText::new("In a window the desktop follows the window's size.")
                            .size(11.0)
                            .color(Color32::from_gray(160)),
                    );
                });
            }

            Tab::Performance => {
                ui.group(|ui| {
                    ui.label(egui::RichText::new("Video").strong());
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label("Codec:");
                        let label = |c| match c {
                            CodecChoice::Auto => "Automatic (HEVC where the GPU decodes it)",
                            CodecChoice::H264 => "H.264",
                            CodecChoice::Hevc => "HEVC",
                        };
                        egui::ComboBox::from_id_salt("codec_combo")
                            .selected_text(label(launcher.codec))
                            .show_ui(ui, |ui| {
                                for c in [CodecChoice::Auto, CodecChoice::H264, CodecChoice::Hevc] {
                                    ui.selectable_value(&mut launcher.codec, c, label(c));
                                }
                            });
                    });
                    ui.horizontal(|ui| {
                        ui.label("GPU:");
                        let label = |g| match g {
                            GpuOption::Auto => "Default",
                            GpuOption::Amd => "AMD",
                            GpuOption::Intel => "Intel",
                            GpuOption::Nvidia => "NVIDIA",
                        };
                        egui::ComboBox::from_id_salt("gpu_combo")
                            .selected_text(label(launcher.gpu))
                            .show_ui(ui, |ui| {
                                for g in [
                                    GpuOption::Auto,
                                    GpuOption::Amd,
                                    GpuOption::Intel,
                                    GpuOption::Nvidia,
                                ] {
                                    ui.selectable_value(&mut launcher.gpu, g, label(g));
                                }
                            });
                    });
                    ui.checkbox(
                        &mut launcher.software_decode,
                        "Decode on the CPU (H.264 only)",
                    );
                    ui.label(
                        egui::RichText::new("RDP always uses H.264.")
                            .size(11.0)
                            .color(Color32::from_gray(160)),
                    );
                });
            }

            Tab::LocalResources => {
                ui.group(|ui| {
                    ui.label(egui::RichText::new("Sound and clipboard").strong());
                    ui.add_space(4.0);
                    ui.checkbox(&mut launcher.mute, "No sound");
                    ui.checkbox(
                        &mut launcher.clipboard,
                        "Share the clipboard (text, glidedesk protocol)",
                    );
                });
            }

            Tab::Advanced => {
                ui.group(|ui| {
                    ui.label(egui::RichText::new("Trusted servers").strong());
                    ui.add_space(4.0);
                    if launcher.known_hosts.hosts.is_empty() {
                        ui.label(
                            egui::RichText::new("None yet: the first connection asks.").italics(),
                        );
                    } else {
                        let mut to_forget = None;
                        for (k, fp) in &launcher.known_hosts.hosts {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(k).strong());
                                let short = if fp.len() > 16 {
                                    format!("{}…{}", &fp[..8], &fp[fp.len() - 8..])
                                } else {
                                    fp.clone()
                                };
                                ui.label(
                                    egui::RichText::new(short)
                                        .monospace()
                                        .color(Color32::from_gray(170)),
                                )
                                .on_hover_text(fp);
                                if ui.button("Forget").clicked() {
                                    to_forget = Some(k.clone());
                                }
                            });
                        }
                        if let Some(target) = to_forget
                            && let Some((h, p)) = target.rsplit_once(':')
                            && let Ok(port) = p.parse::<u16>()
                        {
                            launcher.known_hosts.forget(h, port);
                            if let Err(e) = launcher.known_hosts.save() {
                                launcher.status_msg =
                                    Some((format!("Could not save: {e:#}"), true));
                            }
                        }
                    }
                });
                ui.add_space(6.0);
                ui.group(|ui| {
                    ui.label(egui::RichText::new("Server certificate (SHA-256)").strong());
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(
                            "Pin it for this connection: 64 hex digits, as the server logs it.",
                        )
                        .size(11.0),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut launcher.fingerprint)
                            .hint_text("empty: the trusted one, or ask"),
                    );
                });
                ui.add_space(6.0);
                ui.group(|ui| {
                    ui.label(egui::RichText::new("Log").strong());
                    ui.add_space(4.0);
                    let log = platform::log_path();
                    ui.label(
                        egui::RichText::new(log.display().to_string())
                            .monospace()
                            .size(11.0),
                    );
                    if ui.button("Open the log").clicked() {
                        platform::open_log();
                    }
                });
            }
        });

    action
}

pub struct LauncherState {
    pub launcher: Launcher,
    pub egui_state: egui_winit::State,
    pub egui_renderer: egui_wgpu::Renderer,
}

impl LauncherState {
    pub fn new(window: &Window, device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let egui_ctx = egui::Context::default();
        let viewport_id = egui_ctx.viewport_id();
        let egui_state = egui_winit::State::new(
            egui_ctx,
            viewport_id,
            window,
            Some(window.scale_factor() as f32),
            None,
            None,
        );
        let egui_renderer =
            egui_wgpu::Renderer::new(device, format, egui_wgpu::RendererOptions::default());
        let launcher = Launcher::new();

        Self {
            launcher,
            egui_state,
            egui_renderer,
        }
    }

    pub fn on_event(&mut self, window: &Window, event: &winit::event::WindowEvent) -> bool {
        self.egui_state.on_window_event(window, event).consumed
    }

    pub fn draw(
        &mut self,
        window: &Window,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        view: &wgpu::TextureView,
    ) -> LauncherAction {
        let input = self.egui_state.take_egui_input(window);
        let mut action = LauncherAction::None;
        let l = &mut self.launcher;

        let full_output = self.egui_state.egui_ctx().run_ui(input, |ui| {
            action = render_ui(l, ui);
        });

        self.egui_state
            .handle_platform_output(window, full_output.platform_output);
        let tris = self
            .egui_state
            .egui_ctx()
            .tessellate(full_output.shapes, full_output.pixels_per_point);

        for (id, deltas) in &full_output.textures_delta.set {
            for delta in deltas {
                self.egui_renderer.update_texture(device, queue, *id, delta);
            }
        }

        let screen = ScreenDescriptor {
            size_in_pixels: [
                window.inner_size().width.max(1),
                window.inner_size().height.max(1),
            ],
            pixels_per_point: full_output.pixels_per_point,
        };

        let mut enc = device.create_command_encoder(&Default::default());
        self.egui_renderer
            .update_buffers(device, queue, &mut enc, &tris, &screen);
        {
            let mut pass = enc
                .begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("egui_launcher"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view,
                        depth_slice: None,
                        resolve_target: None,
                        ops: wgpu::Operations {
                            load: wgpu::LoadOp::Clear(wgpu::Color {
                                r: 0.12,
                                g: 0.12,
                                b: 0.14,
                                a: 1.0,
                            }),
                            store: wgpu::StoreOp::Store,
                        },
                    })],
                    ..Default::default()
                })
                .forget_lifetime();
            self.egui_renderer.render(&mut pass, &tris, &screen);
        }
        for id in &full_output.textures_delta.free {
            self.egui_renderer.free_texture(id);
        }
        queue.submit([enc.finish()]);

        action
    }
}
