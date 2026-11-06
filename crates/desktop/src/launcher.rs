//! Desktop client launcher GUI (mstsc-style) using egui.
//!
//! Provides a compact view (host with history dropdown, user, Connect, Options toggle)
//! and an expanded tabbed view (General, Pantalla, Rendimiento, Recursos locales, Avanzado).

use std::path::{Path, PathBuf};
use egui::{Color32, Vec2};
use winit::window::Window;
use egui_wgpu::ScreenDescriptor;

use crate::decode;
use crate::platform;
use crate::profile::{config_dir, History, KnownHosts, Profile};
use crate::Args;

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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CodecOption {
    Auto,
    H264,
    Hevc,
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
    pub codec: CodecOption,
    pub gpu: GpuOption,
    pub software_decode: bool,
    pub mute: bool,
    pub multimon: bool,
    pub history: History,
    pub known_hosts: KnownHosts,
    pub profile_path: String,
    pub status_msg: Option<(String, bool)>,
}

impl Launcher {
    pub fn new() -> Self {
        let history = History::load();
        let known_hosts = KnownHosts::load();
        let host = history.hosts.first().cloned().unwrap_or_default();
        let user = history.last_user.clone().unwrap_or_default();
        let default_profile = config_dir().join("default.glidedesk").to_string_lossy().to_string();

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
            codec: CodecOption::Auto,
            gpu: GpuOption::Auto,
            software_decode: false,
            mute: false,
            multimon: false,
            history,
            known_hosts,
            profile_path: default_profile,
            status_msg: None,
        }
    }

    pub fn load_profile(&mut self, path: &Path) -> Result<(), String> {
        let p = Profile::load_file(path).map_err(|e| e.to_string())?;
        self.host = p.host;
        if let Some(u) = p.user { self.user = u; }
        if let Some(port) = p.port { self.port_str = port.to_string(); }
        if let Some(rdp) = p.rdp { self.rdp = rdp; }
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
        if let Some(fs) = p.fullscreen { self.fullscreen = fs; }
        if let Some(m) = p.mute { self.mute = m; }
        if let Some(sw) = p.software_decode { self.software_decode = sw; }
        if let Some(g) = p.gpu {
            self.gpu = match g.to_ascii_lowercase().as_str() {
                "amd" => GpuOption::Amd,
                "intel" => GpuOption::Intel,
                "nvidia" => GpuOption::Nvidia,
                _ => GpuOption::Auto,
            };
        }
        self.profile_path = path.to_string_lossy().to_string();
        self.status_msg = Some((format!("Perfil cargado: {}", path.display()), false));
        Ok(())
    }

    pub fn save_profile(&self, path: &Path) -> Result<(), String> {
        let (w, h) = match self.resolution {
            ResolutionPreset::R720p => (1280, 720),
            ResolutionPreset::R1080p => (1920, 1080),
            ResolutionPreset::R1440p => (2560, 1440),
            ResolutionPreset::R4k => (3840, 2160),
            ResolutionPreset::Custom => (self.custom_width, self.custom_height),
        };
        let gpu_str = match self.gpu {
            GpuOption::Auto => None,
            GpuOption::Amd => Some("amd".into()),
            GpuOption::Intel => Some("intel".into()),
            GpuOption::Nvidia => Some("nvidia".into()),
        };
        let port = self.port_str.trim().parse::<u16>().ok();
        let p = Profile {
            host: self.host.clone(),
            user: if self.user.trim().is_empty() { None } else { Some(self.user.clone()) },
            port,
            rdp: if self.rdp { Some(true) } else { None },
            width: Some(w),
            height: Some(h),
            fullscreen: if self.fullscreen { Some(true) } else { None },
            mute: if self.mute { Some(true) } else { None },
            software_decode: if self.software_decode { Some(true) } else { None },
            gpu: gpu_str,
        };
        p.save_file(path).map_err(|e| e.to_string())
    }

    pub fn build_args(&mut self) -> Args {
        let (width, height) = match self.resolution {
            ResolutionPreset::R720p => (1280, 720),
            ResolutionPreset::R1080p => (1920, 1080),
            ResolutionPreset::R1440p => (2560, 1440),
            ResolutionPreset::R4k => (3840, 2160),
            ResolutionPreset::Custom => (self.custom_width.max(200), self.custom_height.max(200)),
        };
        let port = if self.port_str.trim().is_empty() {
            None
        } else {
            self.port_str.trim().parse::<u16>().ok()
        };
        let gpu = match self.gpu {
            GpuOption::Auto => None,
            GpuOption::Amd => Some(0x1002),
            GpuOption::Intel => Some(0x8086),
            GpuOption::Nvidia => Some(decode::VENDOR_NVIDIA),
        };

        // Update history
        if !self.host.trim().is_empty() {
            self.history.add(
                self.host.trim(),
                if self.user.trim().is_empty() { None } else { Some(self.user.trim()) },
            );
        }

        Args {
            host: Some(self.host.trim().to_owned()),
            user: if self.user.trim().is_empty() { None } else { Some(self.user.trim().to_owned()) },
            password: None,
            port,
            rdp: self.rdp,
            width,
            height,
            fullscreen: self.fullscreen,
            mute: self.mute,
            software_decode: self.software_decode,
            gpu,
        }
    }
}

pub enum LauncherAction {
    None,
    Connect,
    Exit,
}

pub fn render_ui(launcher: &mut Launcher, ui: &mut egui::Ui) -> LauncherAction {
    let mut action = LauncherAction::None;

    ui.add_space(8.0);
    ui.horizontal(|ui| {
        ui.heading("glidedesk");
        ui.label(egui::RichText::new("Conexión a Escritorio Remoto").italics().color(Color32::from_gray(180)));
    });
    ui.add_space(4.0);
    ui.separator();
    ui.add_space(6.0);

    // Compact view / Quick connection
    ui.group(|ui| {
        egui::Grid::new("quick_conn_grid")
            .num_columns(2)
            .spacing([12.0, 8.0])
            .show(ui, |ui| {
                ui.label(egui::RichText::new("Equipo:").strong());
                ui.horizontal(|ui| {
                    ui.add_sized([220.0, 22.0], egui::TextEdit::singleline(&mut launcher.host).hint_text("host o ip"));

                    if !launcher.history.hosts.is_empty() {
                        egui::ComboBox::from_id_salt("history_combo")
                            .selected_text("Recientes ▼")
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

                ui.label(egui::RichText::new("Usuario:").strong());
                ui.horizontal(|ui| {
                    ui.add_sized([220.0, 22.0], egui::TextEdit::singleline(&mut launcher.user).hint_text("usuario (opcional)"));
                });
                ui.end_row();
            });

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let conn_btn = egui::Button::new(egui::RichText::new(" Conectar ").strong().color(Color32::WHITE))
                .fill(Color32::from_rgb(0, 102, 204))
                .min_size(Vec2::new(100.0, 28.0));

            if ui.add(conn_btn).clicked() || ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                if !launcher.host.trim().is_empty() {
                    action = LauncherAction::Connect;
                } else {
                    launcher.status_msg = Some(("Introduzca un nombre de equipo o dirección IP".into(), true));
                }
            }

            let opt_text = if launcher.show_options { "Opciones <<" } else { "Opciones >>" };
            if ui.button(opt_text).clicked() {
                launcher.show_options = !launcher.show_options;
            }

            if ui.button("Salir").clicked() {
                action = LauncherAction::Exit;
            }
        });
    });

    if let Some((msg, is_err)) = &launcher.status_msg {
        ui.add_space(4.0);
        let col = if *is_err { Color32::RED } else { Color32::from_rgb(50, 180, 50) };
        ui.label(egui::RichText::new(msg).color(col));
    }

    if !launcher.show_options {
        return action;
    }

    ui.add_space(8.0);

    // Tab bar
    ui.horizontal(|ui| {
        ui.selectable_value(&mut launcher.selected_tab, Tab::General, "General");
        ui.selectable_value(&mut launcher.selected_tab, Tab::Display, "Pantalla");
        ui.selectable_value(&mut launcher.selected_tab, Tab::Performance, "Rendimiento");
        ui.selectable_value(&mut launcher.selected_tab, Tab::LocalResources, "Recursos locales");
        ui.selectable_value(&mut launcher.selected_tab, Tab::Advanced, "Avanzado");
    });
    ui.separator();
    ui.add_space(4.0);

    // Tab contents
    egui::ScrollArea::vertical().max_height(240.0).show(ui, |ui| {
        match launcher.selected_tab {
            Tab::General => {
                ui.group(|ui| {
                    ui.label(egui::RichText::new("Configuración de conexión").strong());
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut launcher.rdp, false, "Protocolo nativo glidedesk (baja latencia)");
                        ui.radio_value(&mut launcher.rdp, true, "RDP estándar");
                    });

                    ui.horizontal(|ui| {
                        ui.label("Puerto personalizado:");
                        ui.add_sized([80.0, 20.0], egui::TextEdit::singleline(&mut launcher.port_str).hint_text(if launcher.rdp { "3389" } else { "3391" }));
                    });
                });

                ui.add_space(6.0);
                ui.group(|ui| {
                    ui.label(egui::RichText::new("Perfiles de conexión (.glidedesk)").strong());
                    ui.add_space(4.0);
                    ui.horizontal(|ui| {
                        ui.label("Ruta:");
                        ui.add_sized([260.0, 20.0], egui::TextEdit::singleline(&mut launcher.profile_path));
                    });
                    ui.horizontal(|ui| {
                        if ui.button("Guardar perfil").clicked() {
                            let p = PathBuf::from(&launcher.profile_path);
                            match launcher.save_profile(&p) {
                                Ok(()) => {
                                    launcher.status_msg = Some((format!("Guardado en {}", p.display()), false));
                                }
                                Err(e) => {
                                    launcher.status_msg = Some((format!("Error guardando: {e}"), true));
                                }
                            }
                        }
                        if ui.button("Cargar perfil").clicked() {
                            let p = PathBuf::from(&launcher.profile_path);
                            if let Err(e) = launcher.load_profile(&p) {
                                launcher.status_msg = Some((format!("Error cargando: {e}"), true));
                            }
                        }
                    });
                });
            }

            Tab::Display => {
                ui.group(|ui| {
                    ui.label(egui::RichText::new("Resolución y pantalla").strong());
                    ui.add_space(4.0);
                    ui.checkbox(&mut launcher.fullscreen, "Iniciar en pantalla completa");
                    ui.checkbox(&mut launcher.multimon, "Usar todos mis monitores (ADR-0006 multimon)");

                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.label("Resolución:");
                        egui::ComboBox::from_id_salt("res_combo")
                            .selected_text(match launcher.resolution {
                                ResolutionPreset::R720p => "1280 x 720 (HD)",
                                ResolutionPreset::R1080p => "1920 x 1080 (Full HD)",
                                ResolutionPreset::R1440p => "2560 x 1440 (2K QHD)",
                                ResolutionPreset::R4k => "3840 x 2160 (4K UHD)",
                                ResolutionPreset::Custom => "Personalizada",
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut launcher.resolution, ResolutionPreset::R720p, "1280 x 720 (HD)");
                                ui.selectable_value(&mut launcher.resolution, ResolutionPreset::R1080p, "1920 x 1080 (Full HD)");
                                ui.selectable_value(&mut launcher.resolution, ResolutionPreset::R1440p, "2560 x 1440 (2K QHD)");
                                ui.selectable_value(&mut launcher.resolution, ResolutionPreset::R4k, "3840 x 2160 (4K UHD)");
                                ui.selectable_value(&mut launcher.resolution, ResolutionPreset::Custom, "Personalizada");
                            });
                    });

                    if launcher.resolution == ResolutionPreset::Custom {
                        ui.horizontal(|ui| {
                            ui.label("Ancho:");
                            ui.add(egui::DragValue::new(&mut launcher.custom_width).range(320..=7680));
                            ui.label("Alto:");
                            ui.add(egui::DragValue::new(&mut launcher.custom_height).range(240..=4320));
                        });
                    }
                });
            }

            Tab::Performance => {
                ui.group(|ui| {
                    ui.label(egui::RichText::new("Códec y aceleración por GPU").strong());
                    ui.add_space(4.0);

                    ui.horizontal(|ui| {
                        ui.label("Códec preferido:");
                        egui::ComboBox::from_id_salt("codec_combo")
                            .selected_text(match launcher.codec {
                                CodecOption::Auto => "Automático (HEVC si GPU lo soporta, o H.264)",
                                CodecOption::H264 => "H.264 AVC (universal)",
                                CodecOption::Hevc => "H.265 HEVC (alta compresión)",
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut launcher.codec, CodecOption::Auto, "Automático");
                                ui.selectable_value(&mut launcher.codec, CodecOption::H264, "H.264 AVC");
                                ui.selectable_value(&mut launcher.codec, CodecOption::Hevc, "H.265 HEVC");
                            });
                    });

                    ui.horizontal(|ui| {
                        ui.label("GPU de decodificación y pantalla:");
                        egui::ComboBox::from_id_salt("gpu_combo")
                            .selected_text(match launcher.gpu {
                                GpuOption::Auto => "Automático (GPU predeterminada)",
                                GpuOption::Amd => "AMD Radeon (PCI 0x1002)",
                                GpuOption::Intel => "Intel HD / Iris / Arc (PCI 0x8086)",
                                GpuOption::Nvidia => "NVIDIA GeForce / RTX (PCI 0x10DE)",
                            })
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut launcher.gpu, GpuOption::Auto, "Automático");
                                ui.selectable_value(&mut launcher.gpu, GpuOption::Amd, "AMD Radeon");
                                ui.selectable_value(&mut launcher.gpu, GpuOption::Intel, "Intel Graphics");
                                ui.selectable_value(&mut launcher.gpu, GpuOption::Nvidia, "NVIDIA GeForce");
                            });
                    });

                    ui.checkbox(&mut launcher.software_decode, "Decodificación por software en CPU (H.264)");
                });
            }

            Tab::LocalResources => {
                ui.group(|ui| {
                    ui.label(egui::RichText::new("Audio y Portapapeles").strong());
                    ui.add_space(4.0);
                    ui.checkbox(&mut launcher.mute, "Silenciar audio del escritorio remoto");
                    ui.label(egui::RichText::new("✓ Portapapeles bidireccional activado por defecto").color(Color32::from_gray(160)));
                });
            }

            Tab::Advanced => {
                ui.group(|ui| {
                    ui.label(egui::RichText::new("Certificados y equipos conocidos (TOFU)").strong());
                    ui.add_space(4.0);

                    if launcher.known_hosts.hosts.is_empty() {
                        ui.label(egui::RichText::new("No hay huellas registradas.").italics());
                    } else {
                        let mut to_forget = None;
                        for (k, fp) in &launcher.known_hosts.hosts {
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(k).strong());
                                let short_fp = if fp.len() > 16 {
                                    format!("{}...{}", &fp[..8], &fp[fp.len() - 8..])
                                } else {
                                    fp.clone()
                                };
                                ui.label(egui::RichText::new(short_fp).monospace().color(Color32::from_gray(170)));
                                if ui.button("Olvidar").clicked() {
                                    to_forget = Some(k.clone());
                                }
                            });
                        }
                        if let Some(target) = to_forget
                            && let Some((h, p)) = target.split_once(':')
                            && let Ok(port) = p.parse::<u16>()
                        {
                            launcher.known_hosts.forget(h, port);
                        }
                    }
                });

                ui.add_space(6.0);
                ui.group(|ui| {
                    ui.label(egui::RichText::new("Diagnóstico y registros").strong());
                    ui.add_space(4.0);
                    let log = platform::log_path();
                    ui.label(egui::RichText::new(format!("Archivo: {}", log.display())).monospace().size(11.0));
                    if ui.button("Abrir archivo de registro").clicked() {
                        platform::open_log();
                    }
                });
            }
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
    pub fn new(
        window: &Window,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
    ) -> Self {
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
        let egui_renderer = egui_wgpu::Renderer::new(device, format, egui_wgpu::RendererOptions::default());
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

        self.egui_state.handle_platform_output(window, full_output.platform_output);
        let tris = self.egui_state.egui_ctx().tessellate(full_output.shapes, full_output.pixels_per_point);

        for (id, deltas) in &full_output.textures_delta.set {
            for delta in deltas {
                self.egui_renderer.update_texture(device, queue, *id, delta);
            }
        }

        let screen = ScreenDescriptor {
            size_in_pixels: [window.inner_size().width.max(1), window.inner_size().height.max(1)],
            pixels_per_point: full_output.pixels_per_point,
        };

        let mut enc = device.create_command_encoder(&Default::default());
        self.egui_renderer.update_buffers(device, queue, &mut enc, &tris, &screen);
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("egui_launcher"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.12, g: 0.12, b: 0.14, a: 1.0 }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            }).forget_lifetime();
            self.egui_renderer.render(&mut pass, &tris, &screen);
        }
        for id in &full_output.textures_delta.free {
            self.egui_renderer.free_texture(id);
        }
        queue.submit([enc.finish()]);

        action
    }
}
