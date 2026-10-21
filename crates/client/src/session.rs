//! One RDP session: connect (TLS + NLA), then pump PDUs, input and resize requests.

use std::net::SocketAddr;

use anyhow::{Context, Result, bail};
use ironrdp_async::FramedWrite as _;
use ironrdp_connector::{
    ClientConnector, Config, ConnectorResult, Credentials, DesktopSize, ServerName,
};
use ironrdp_dvc::DrdynvcClient;
use ironrdp_input::{Database, MouseButton, MousePosition, Operation, Scancode, WheelRotations};
use ironrdp_pdu::gcc::KeyboardType;
use ironrdp_pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp_pdu::rdp::client_info::{PerformanceFlags, TimezoneInfo};
use ironrdp_session::image::DecodedImage;
use ironrdp_session::{ActiveStage, ActiveStageBuilder, ActiveStageOutput};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::gfx::{GfxClient, SharedSink};

pub struct ConnectOptions {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub width: u16,
    pub height: u16,
}

/// What the platform sends into a running session.
#[derive(Debug, Clone)]
pub enum Input {
    MouseMove {
        x: u16,
        y: u16,
    },
    MouseButton {
        button: u8,
        pressed: bool,
    },
    Wheel {
        vertical: bool,
        units: i16,
    },
    Key {
        scancode: u16,
        extended: bool,
        pressed: bool,
    },
    Unicode {
        ch: char,
        pressed: bool,
    },
    Resize {
        width: u32,
        height: u32,
    },
    Disconnect,
}

/// NLA with NTLM does not need network access beyond the RDP connection.
struct NoNetwork;

impl ironrdp_async::NetworkClient for NoNetwork {
    async fn send(
        &mut self,
        _request: &ironrdp_connector::sspi::generator::NetworkRequest,
    ) -> ConnectorResult<Vec<u8>> {
        Err(ironrdp_connector::general_err!(
            "network access is not available (Kerberos)"
        ))
    }
}

fn config(o: &ConnectOptions) -> Config {
    Config {
        credentials: Credentials::UsernamePassword {
            username: o.username.clone(),
            password: o.password.clone(),
        },
        domain: None,
        enable_tls: true,
        enable_credssp: true,
        enable_standard_rdp_security: false,
        keyboard_type: KeyboardType::IBM_ENHANCED,
        keyboard_subtype: 0,
        keyboard_layout: 0,
        keyboard_functional_keys_count: 12,
        ime_file_name: String::new(),
        dig_product_id: String::new(),
        desktop_size: DesktopSize {
            width: o.width,
            height: o.height,
        },
        monitor_layout: None,
        bitmap: None,
        client_build: 0,
        client_name: "glidedesk".to_owned(),
        client_dir: String::new(),
        platform: if cfg!(target_os = "android") {
            MajorPlatformType::ANDROID
        } else if cfg!(windows) {
            MajorPlatformType::WINDOWS
        } else {
            MajorPlatformType::UNIX
        },
        // The server's pointer shapes are drawn locally by the platform (later); for now
        // the platform shows its own pointer.
        enable_server_pointer: false,
        request_data: None,
        autologon: false,
        enable_audio_playback: false,
        enable_audio_capture: false,
        compression_type: None,
        pointer_software_rendering: false,
        multitransport_flags: None,
        support_dyn_vc_gfx_protocol: true,
        performance_flags: PerformanceFlags::default(),
        desktop_scale_factor: 0,
        hardware_id: None,
        license_cache: None,
        timezone_info: TimezoneInfo::default(),
        alternate_shell: String::new(),
        work_dir: String::new(),
        remote_application_mode: false,
        rail_support_level: ironrdp_pdu::rdp::capability_sets::RailSupportLevel::empty(),
        connection_type: ironrdp_pdu::gcc::ConnectionType::Lan,
    }
}

/// Connects and runs until the server ends the session or `Input::Disconnect` arrives.
pub async fn run(
    opts: ConnectOptions,
    sink: SharedSink,
    mut input: mpsc::UnboundedReceiver<Input>,
) -> Result<()> {
    let addr: SocketAddr = tokio::net::lookup_host((opts.host.as_str(), opts.port))
        .await?
        .next()
        .context("host not found")?;
    let tcp = TcpStream::connect(addr).await.context("connecting")?;
    tcp.set_nodelay(true)?;
    let local = tcp.local_addr()?;
    let mut framed = ironrdp_tokio::TokioFramed::new(tcp);

    let drdynvc = DrdynvcClient::new()
        .with_dynamic_channel(GfxClient::new(sink))
        .with_dynamic_channel(ironrdp_displaycontrol::client::DisplayControlClient::new(
            |_| Ok(Vec::new()),
        ));
    let mut connector = ClientConnector::new(config(&opts), local).with_static_channel(drdynvc);
    let should_upgrade = ironrdp_async::connect_begin(&mut framed, &mut connector).await?;
    let tcp = framed.into_inner_no_leftover();
    // The host certificate is self-signed; it is pinned on first use by the platform layer
    // (to do). The TLS public key still binds NLA to this connection.
    let (tls, cert) = ironrdp_tls::upgrade(tcp, &opts.host).await.context("TLS")?;
    let public_key = ironrdp_tls::extract_tls_server_public_key(&cert)
        .context("no public key in the server certificate")?
        .to_owned();
    let upgraded = ironrdp_async::mark_as_upgraded(should_upgrade, &mut connector);
    let mut framed = ironrdp_tokio::TokioFramed::new(tls);
    let result = ironrdp_async::connect_finalize(
        upgraded,
        connector,
        &mut framed,
        &mut NoNetwork,
        ServerName::new(opts.host.clone()),
        public_key,
        None,
    )
    .await
    .context("RDP connection")?;
    info!(
        width = result.desktop_size.width,
        height = result.desktop_size.height,
        "Connected"
    );

    let mut image = DecodedImage::new(
        ironrdp_graphics::image_processing::PixelFormat::RgbA32,
        result.desktop_size.width,
        result.desktop_size.height,
    );
    let mut stage: ActiveStage = ActiveStageBuilder {
        static_channels: result.static_channels,
        user_channel_id: result.user_channel_id,
        io_channel_id: result.io_channel_id,
        message_channel_id: result.message_channel_id,
        share_id: result.share_id,
        compression_type: result.compression_type,
        enable_server_pointer: result.enable_server_pointer,
        pointer_software_rendering: result.pointer_software_rendering,
    }
    .build();
    let mut keys = Database::new();
    // The size we want, sent once Display Control is open: IronRDP's connector echoes the
    // server's size in the capability exchange, so this is how the client gets its own.
    let mut wanted: Option<(u32, u32)> = Some((u32::from(opts.width), u32::from(opts.height)));

    loop {
        let outputs = tokio::select! {
            pdu = framed.read_pdu() => {
                let (action, frame) = pdu.context("reading from the server")?;
                stage.process(&mut image, action, &frame, None)?
            }
            ev = input.recv() => {
                let Some(ev) = ev else { break };
                match ev {
                    Input::Disconnect => {
                        let outs = stage.graceful_shutdown()?;
                        write_outputs(&mut framed, outs).await?;
                        break;
                    }
                    Input::Resize { width, height } => match stage.encode_resize(width, height, None, None) {
                        None => {
                            wanted = Some((width, height));
                            Vec::new()
                        }
                        Some(Ok(frame)) => {
                            framed.write_all(&frame).await?;
                            Vec::new()
                        }
                        Some(Err(e)) => {
                            warn!(%e, "Resize request failed");
                            Vec::new()
                        }
                    },
                    other => {
                        let events = keys.apply(operations(other));
                        stage.process_fastpath_input(&mut image, &events)?
                    }
                }
            }
        };
        if write_outputs(&mut framed, outputs).await? {
            break;
        }
        if let Some((w, h)) = wanted
            && let Some(frame) = stage.encode_resize(w, h, None, None)
        {
            framed.write_all(&frame?).await?;
            wanted = None;
        }
    }
    Ok(())
}

fn operations(ev: Input) -> Vec<Operation> {
    match ev {
        Input::MouseMove { x, y } => vec![Operation::MouseMove(MousePosition { x, y })],
        Input::MouseButton { button, pressed } => {
            let b = match button {
                1 => MouseButton::Middle,
                2 => MouseButton::Right,
                3 => MouseButton::X1,
                4 => MouseButton::X2,
                _ => MouseButton::Left,
            };
            vec![if pressed {
                Operation::MouseButtonPressed(b)
            } else {
                Operation::MouseButtonReleased(b)
            }]
        }
        Input::Wheel { vertical, units } => vec![Operation::WheelRotations(WheelRotations {
            is_vertical: vertical,
            rotation_units: units,
        })],
        Input::Key {
            scancode,
            extended,
            pressed,
        } => {
            let code = Scancode::from_u8(extended, scancode as u8);
            vec![if pressed {
                Operation::KeyPressed(code)
            } else {
                Operation::KeyReleased(code)
            }]
        }
        Input::Unicode { ch, pressed } => vec![if pressed {
            Operation::UnicodeKeyPressed(ch)
        } else {
            Operation::UnicodeKeyReleased(ch)
        }],
        Input::Resize { .. } | Input::Disconnect => Vec::new(),
    }
}

/// Writes responses; returns true when the session ended.
async fn write_outputs<S>(
    framed: &mut ironrdp_tokio::TokioFramed<S>,
    outputs: Vec<ActiveStageOutput>,
) -> Result<bool>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + Sync,
{
    for out in outputs {
        match out {
            ActiveStageOutput::ResponseFrame(frame) => framed.write_all(&frame).await?,
            ActiveStageOutput::Terminate(reason) => {
                info!(?reason, "Session ended");
                return Ok(true);
            }
            ActiveStageOutput::DeactivateAll => {
                bail!("deactivation-reactivation is not supported yet")
            }
            _ => {}
        }
    }
    Ok(false)
}
