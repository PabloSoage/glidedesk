//! JNI bridge of the Android client.
//!
//! Kotlin (`io.github.pablosoage.glidedesk.NativeSession`) opens a session, hands over the
//! `Surface` of its view and forwards touch and keyboard input. Video never crosses JNI:
//! each H.264 access unit from the session goes to an `AMediaCodec` decoder that renders
//! straight into that `Surface`.

#[cfg(target_os = "android")]
mod android {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use glidedesk_client::gfx::{SharedSink, VideoCodec, VideoSink};
    use glidedesk_client::session::{ConnectOptions, Input, run};
    use jni::JNIEnv;
    use jni::objects::{JClass, JObject, JString};
    use jni::sys::{jboolean, jint, jlong, jstring};
    use ndk::media::media_codec::{
        DequeuedInputBufferResult, DequeuedOutputBufferInfoResult, MediaCodec, MediaCodecDirection,
    };
    use ndk::media::media_format::MediaFormat;
    use ndk::native_window::NativeWindow;
    use tokio::sync::mpsc;

    /// Decodes into the view's surface. Without a surface (app in the background) frames are
    /// dropped and the next keyframe restarts the picture.
    struct Decoder {
        window: Option<NativeWindow>,
        mc: Option<MediaCodec>,
        size: (u32, u32),
        codec: VideoCodec,
        pts_us: u64,
        frames: u64,
    }

    // MediaCodec and NativeWindow are used from the session thread and the JNI thread,
    // always under the mutex.
    unsafe impl Send for Decoder {}

    impl Decoder {
        fn start(&mut self) {
            self.mc = None;
            let (Some(window), (w, h)) = (&self.window, self.size) else {
                return;
            };
            if w == 0 || h == 0 {
                return;
            }
            let mime = match self.codec {
                VideoCodec::H264 => "video/avc",
                VideoCodec::Hevc => "video/hevc",
            };
            let Some(codec) = MediaCodec::from_decoder_type(mime) else {
                return;
            };
            let mut format = MediaFormat::new();
            format.set_str("mime", mime);
            format.set_i32("width", w as i32);
            format.set_i32("height", h as i32);
            // Ask the decoder not to hold frames back (Android 11+; ignored before).
            format.set_i32("low-latency", 1);
            format.set_i32("priority", 0);
            if codec
                .configure(&format, Some(window), MediaCodecDirection::Decoder)
                .is_ok()
                && codec.start().is_ok()
            {
                self.mc = Some(codec);
            }
        }
    }

    impl VideoSink for Decoder {
        fn reset(&mut self, width: u32, height: u32, codec: VideoCodec) {
            self.size = (width, height);
            self.codec = codec;
            self.start();
        }

        fn frame(&mut self, h264: &[u8]) {
            let Some(codec) = &self.mc else { return };
            self.frames += 1;
            if let Ok(DequeuedInputBufferResult::Buffer(mut buf)) =
                codec.dequeue_input_buffer(Duration::from_millis(5))
            {
                let dst = buf.buffer_mut();
                let n = h264.len().min(dst.len());
                for (d, s) in dst[..n].iter_mut().zip(&h264[..n]) {
                    d.write(*s);
                }
                self.pts_us += 16_667;
                let _ = codec.queue_input_buffer(buf, 0, n, self.pts_us, 0);
            }
            // Render whatever is decoded right away: no presentation clock, lowest latency.
            while let Ok(out) = codec.dequeue_output_buffer(Duration::ZERO) {
                match out {
                    DequeuedOutputBufferInfoResult::Buffer(b) => {
                        let _ = codec.release_output_buffer(b, true);
                    }
                    DequeuedOutputBufferInfoResult::TryAgainLater => break,
                    _ => {}
                }
            }
        }
    }

    #[derive(Default)]
    struct Status {
        connected: bool,
        ended: bool,
        error: Option<String>,
    }

    struct Session {
        runtime: tokio::runtime::Runtime,
        input: mpsc::UnboundedSender<Input>,
        decoder: Arc<Mutex<Decoder>>,
        status: Arc<Mutex<Status>>,
    }

    /// Tracks the first frame to report "connected".
    struct Sink {
        decoder: Arc<Mutex<Decoder>>,
        status: Arc<Mutex<Status>>,
    }

    impl VideoSink for Sink {
        fn reset(&mut self, width: u32, height: u32, codec: VideoCodec) {
            self.status.lock().unwrap().connected = true;
            self.decoder.lock().unwrap().reset(width, height, codec);
        }
        fn frame(&mut self, h264: &[u8]) {
            self.decoder.lock().unwrap().frame(h264);
        }
    }

    fn session<'a>(handle: jlong) -> Option<&'a Session> {
        // SAFETY: handles come from nativeConnect and stay valid until nativeClose.
        (handle != 0).then(|| unsafe { &*(handle as *const Session) })
    }

    fn string(env: &mut JNIEnv, s: &JString) -> String {
        env.get_string(s).map(Into::into).unwrap_or_default()
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_io_github_pablosoage_glidedesk_NativeSession_nativeConnect(
        mut env: JNIEnv,
        _class: JClass,
        host: JString,
        port: jint,
        user: JString,
        password: JString,
        width: jint,
        height: jint,
    ) -> jlong {
        let opts = ConnectOptions {
            host: string(&mut env, &host),
            port: port as u16,
            username: string(&mut env, &user),
            password: string(&mut env, &password),
            width: width.clamp(200, 8192) as u16 & !1,
            height: height.clamp(200, 8192) as u16 & !1,
        };
        let Ok(runtime) = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
        else {
            return 0;
        };
        let decoder = Arc::new(Mutex::new(Decoder {
            window: None,
            mc: None,
            size: (0, 0),
            codec: VideoCodec::H264,
            pts_us: 0,
            frames: 0,
        }));
        let status: Arc<Mutex<Status>> = Arc::default();
        let (tx, rx) = mpsc::unbounded_channel();
        let sink: SharedSink = Arc::new(Mutex::new(Sink {
            decoder: Arc::clone(&decoder),
            status: Arc::clone(&status),
        }));
        let st = Arc::clone(&status);
        runtime.spawn(async move {
            let r = run(opts, sink, rx).await;
            let mut s = st.lock().unwrap();
            s.ended = true;
            if let Err(e) = r {
                s.error = Some(format!("{e:#}"));
            }
        });
        Box::into_raw(Box::new(Session {
            runtime,
            input: tx,
            decoder,
            status,
        })) as jlong
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_io_github_pablosoage_glidedesk_NativeSession_nativeSetSurface(
        env: JNIEnv,
        _class: JClass,
        handle: jlong,
        surface: JObject,
    ) {
        let Some(s) = session(handle) else { return };
        let window = if surface.is_null() {
            None
        } else {
            // SAFETY: a valid JNIEnv and android.view.Surface from the caller.
            unsafe {
                NativeWindow::from_surface(env.get_raw() as *mut _, surface.as_raw() as *mut _)
            }
        };
        let mut d = s.decoder.lock().unwrap();
        d.window = window;
        d.start();
    }

    fn send(handle: jlong, ev: Input) {
        if let Some(s) = session(handle) {
            let _ = s.input.send(ev);
        }
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_io_github_pablosoage_glidedesk_NativeSession_nativeMouseMove(
        _env: JNIEnv,
        _c: JClass,
        h: jlong,
        x: jint,
        y: jint,
    ) {
        send(
            h,
            Input::MouseMove {
                x: x.max(0) as u16,
                y: y.max(0) as u16,
            },
        );
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_io_github_pablosoage_glidedesk_NativeSession_nativeMouseButton(
        _env: JNIEnv,
        _c: JClass,
        h: jlong,
        button: jint,
        pressed: jboolean,
    ) {
        send(
            h,
            Input::MouseButton {
                button: button as u8,
                pressed: pressed != 0,
            },
        );
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_io_github_pablosoage_glidedesk_NativeSession_nativeWheel(
        _env: JNIEnv,
        _c: JClass,
        h: jlong,
        vertical: jboolean,
        units: jint,
    ) {
        send(
            h,
            Input::Wheel {
                vertical: vertical != 0,
                units: units as i16,
            },
        );
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_io_github_pablosoage_glidedesk_NativeSession_nativeKey(
        _env: JNIEnv,
        _c: JClass,
        h: jlong,
        scancode: jint,
        extended: jboolean,
        pressed: jboolean,
    ) {
        send(
            h,
            Input::Key {
                scancode: scancode as u16,
                extended: extended != 0,
                pressed: pressed != 0,
            },
        );
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_io_github_pablosoage_glidedesk_NativeSession_nativeUnicode(
        _env: JNIEnv,
        _c: JClass,
        h: jlong,
        codepoint: jint,
        pressed: jboolean,
    ) {
        if let Some(ch) = char::from_u32(codepoint as u32) {
            send(
                h,
                Input::Unicode {
                    ch,
                    pressed: pressed != 0,
                },
            );
        }
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_io_github_pablosoage_glidedesk_NativeSession_nativeResize(
        _env: JNIEnv,
        _c: JClass,
        h: jlong,
        width: jint,
        height: jint,
    ) {
        send(
            h,
            Input::Resize {
                width: (width.max(200) as u32) & !1,
                height: (height.max(200) as u32) & !1,
            },
        );
    }

    /// 0 connecting, 1 showing the desktop, 2 ended, 3 failed (see nativeError).
    #[unsafe(no_mangle)]
    pub extern "system" fn Java_io_github_pablosoage_glidedesk_NativeSession_nativeState(
        _env: JNIEnv,
        _c: JClass,
        h: jlong,
    ) -> jint {
        let Some(s) = session(h) else { return 2 };
        let st = s.status.lock().unwrap();
        match (st.ended, st.error.is_some(), st.connected) {
            (true, true, _) => 3,
            (true, false, _) => 2,
            (false, _, true) => 1,
            _ => 0,
        }
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_io_github_pablosoage_glidedesk_NativeSession_nativeError(
        env: JNIEnv,
        _c: JClass,
        h: jlong,
    ) -> jstring {
        let msg = session(h)
            .and_then(|s| s.status.lock().unwrap().error.clone())
            .unwrap_or_default();
        env.new_string(msg)
            .map(|s| s.into_raw())
            .unwrap_or(std::ptr::null_mut())
    }

    #[unsafe(no_mangle)]
    pub extern "system" fn Java_io_github_pablosoage_glidedesk_NativeSession_nativeClose(
        _env: JNIEnv,
        _c: JClass,
        h: jlong,
    ) {
        if h == 0 {
            return;
        }
        // SAFETY: the handle is not used again by the Kotlin side after close.
        let s = unsafe { Box::from_raw(h as *mut Session) };
        let _ = s.input.send(Input::Disconnect);
        s.runtime.shutdown_timeout(Duration::from_secs(2));
    }
}
