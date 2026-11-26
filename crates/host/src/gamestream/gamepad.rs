//! Moonlight gamepads (moonlight-common-c `Input.h` `NV_MULTI_CONTROLLER_PACKET` and
//! `ControlStream.c`, Sunshine `src/input.cpp` and `src/stream.cpp`, commits in
//! THIRD_PARTY.md): each controller on the client becomes a virtual Xbox 360 pad in the
//! session, and its rumble goes back to the client.
//!
//! The client sends the whole state of one controller per message (LE fields): its number,
//! the mask of the controllers it has, the buttons (XInput's bits, plus paddles and others in
//! a second word), the triggers (0-255) and the sticks (-32768-32767, up is positive). A
//! controller whose bit leaves the mask is unplugged.
//!
//! Linux: a uinput device per controller with the evdev codes of the kernel's `xpad` driver
//! and the Xbox 360 USB ids, so SDL, Steam and games treat it as one; `/dev/uinput` has to be
//! writable by the user (README). Windows: an Xbox 360 target of the ViGEmBus driver, which has
//! to be installed. Rumble comes back from the game (force feedback on Linux, ViGEm
//! notifications on Windows) and goes to the client as control message 0x010b.

use std::sync::Arc;

use anyhow::Result;
use tracing::{info, warn};

pub const MULTI_CONTROLLER: u32 = 0x0c;
/// The magic of generation 4 hosts; Moonlight uses it with older servers.
pub const MULTI_CONTROLLER_OLD: u32 = 0x0d;

/// Most controllers the activity mask has room for.
const MAX_PADS: usize = 16;

/// Moonlight's button bits (the first 16 are XInput's).
pub mod button {
    pub const UP: u32 = 0x0001;
    pub const DOWN: u32 = 0x0002;
    pub const LEFT: u32 = 0x0004;
    pub const RIGHT: u32 = 0x0008;
    pub const START: u32 = 0x0010;
    pub const BACK: u32 = 0x0020;
    pub const LEFT_STICK: u32 = 0x0040;
    pub const RIGHT_STICK: u32 = 0x0080;
    pub const LB: u32 = 0x0100;
    pub const RB: u32 = 0x0200;
    pub const GUIDE: u32 = 0x0400;
    pub const A: u32 = 0x1000;
    pub const B: u32 = 0x2000;
    pub const X: u32 = 0x4000;
    pub const Y: u32 = 0x8000;
}

/// One controller's state.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PadState {
    pub buttons: u32,
    pub left_trigger: u8,
    pub right_trigger: u8,
    /// x, y; up is positive.
    pub left: (i16, i16),
    pub right: (i16, i16),
}

/// The body of a controller message (after size and type): controller number, activity mask
/// and state.
pub fn parse(body: &[u8]) -> Option<(usize, u16, PadState)> {
    let u16_at = |at: usize| {
        body.get(at..at + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
    };
    let i16_at = |at: usize| u16_at(at).map(|v| v as i16);
    let number = usize::from(u16_at(2)?);
    let mask = u16_at(4)?;
    // The second button word is a Sunshine extension; older clients leave it out or zero.
    let extra = u32::from(u16_at(22).unwrap_or(0));
    Some((
        number,
        mask,
        PadState {
            buttons: u32::from(u16_at(8)?) | extra << 16,
            left_trigger: *body.get(10)?,
            right_trigger: *body.get(11)?,
            left: (i16_at(12)?, i16_at(14)?),
            right: (i16_at(16)?, i16_at(18)?),
        },
    ))
}

/// Rumble for the client: controller number, low and high frequency motors (0-65535).
pub type Rumble = Arc<dyn Fn(u16, u16, u16) + Send + Sync>;

trait Pad: Send {
    fn set(&mut self, s: &PadState) -> Result<()>;
}

/// The virtual pads of one client.
pub struct Gamepads {
    pads: Vec<Option<Box<dyn Pad>>>,
    rumble: Rumble,
    /// Creating a pad failed once (no permission, no driver): said once, not per message.
    failed: bool,
}

impl Gamepads {
    pub fn new(rumble: Rumble) -> Self {
        Self {
            pads: (0..MAX_PADS).map(|_| None).collect(),
            rumble,
            failed: false,
        }
    }

    /// A controller message: pads missing from the mask go away, this one is created if new
    /// and set.
    pub fn update(&mut self, number: usize, mask: u16, state: &PadState) {
        for (i, pad) in self.pads.iter_mut().enumerate() {
            if mask & (1 << i) == 0 && pad.take().is_some() {
                info!(number = i, "Moonlight gamepad unplugged");
            }
        }
        if number >= MAX_PADS || mask & (1 << number) == 0 {
            return;
        }
        if self.pads[number].is_none() {
            if self.failed {
                return;
            }
            match backend::create(number, Arc::clone(&self.rumble)) {
                Ok(pad) => {
                    info!(number, "Moonlight gamepad plugged in");
                    self.pads[number] = Some(pad);
                }
                Err(e) => {
                    warn!(
                        error = format!("{e:#}"),
                        "No virtual gamepad: Moonlight's controllers do nothing"
                    );
                    self.failed = true;
                    return;
                }
            }
        }
        if let Some(pad) = self.pads[number].as_mut()
            && let Err(e) = pad.set(state)
        {
            tracing::debug!(%e, "Gamepad update failed");
        }
    }

    /// The client went away: every pad is unplugged.
    pub fn clear(&mut self) {
        for pad in &mut self.pads {
            *pad = None;
        }
        self.failed = false;
    }
}

#[cfg(target_os = "linux")]
mod backend {
    //! uinput, with the codes of the kernel's `xpad` driver.

    use std::collections::HashMap;
    use std::os::fd::AsRawFd;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result};
    use evdev::uinput::VirtualDevice;
    use evdev::{
        AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, EventSummary, EventType, FFEffectCode,
        FFEffectKind, InputEvent, InputId, KeyCode, UInputCode, UinputAbsSetup,
    };

    use super::{Pad, PadState, Rumble, button};

    /// Moonlight buttons and the key codes `xpad` gives them.
    const KEYS: [(u32, KeyCode); 11] = [
        (button::A, KeyCode::BTN_SOUTH),
        (button::B, KeyCode::BTN_EAST),
        (button::X, KeyCode::BTN_NORTH),
        (button::Y, KeyCode::BTN_WEST),
        (button::LB, KeyCode::BTN_TL),
        (button::RB, KeyCode::BTN_TR),
        (button::BACK, KeyCode::BTN_SELECT),
        (button::START, KeyCode::BTN_START),
        (button::GUIDE, KeyCode::BTN_MODE),
        (button::LEFT_STICK, KeyCode::BTN_THUMBL),
        (button::RIGHT_STICK, KeyCode::BTN_THUMBR),
    ];

    pub struct UinputPad {
        dev: Arc<Mutex<VirtualDevice>>,
        stop: Arc<AtomicBool>,
        last: Option<PadState>,
    }

    impl Drop for UinputPad {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    pub fn create(number: usize, rumble: Rumble) -> Result<Box<dyn Pad>> {
        let stick = AbsInfo::new(0, -32768, 32767, 16, 128, 0);
        let trigger = AbsInfo::new(0, 0, 255, 0, 0, 0);
        let hat = AbsInfo::new(0, -1, 1, 0, 0, 0);
        let mut builder = VirtualDevice::builder()
            .context("opening /dev/uinput")?
            .name("glidedesk gamepad (Xbox 360)")
            // The Xbox 360 wired controller's ids, so games use its layout.
            .input_id(InputId::new(BusType::BUS_USB, 0x045e, 0x028e, 0x0110))
            .with_keys(&KEYS.iter().map(|&(_, k)| k).collect::<AttributeSet<_>>())?
            .with_ff(&AttributeSet::from_iter([FFEffectCode::FF_RUMBLE]))?
            .with_ff_effects_max(16);
        for (axis, info) in [
            (AbsoluteAxisCode::ABS_X, stick),
            (AbsoluteAxisCode::ABS_Y, stick),
            (AbsoluteAxisCode::ABS_RX, stick),
            (AbsoluteAxisCode::ABS_RY, stick),
            (AbsoluteAxisCode::ABS_Z, trigger),
            (AbsoluteAxisCode::ABS_RZ, trigger),
            (AbsoluteAxisCode::ABS_HAT0X, hat),
            (AbsoluteAxisCode::ABS_HAT0Y, hat),
        ] {
            builder = builder.with_absolute_axis(&UinputAbsSetup::new(axis, info))?;
        }
        let dev = builder.build().context("creating the virtual gamepad")?;
        // Reads (force feedback) must not block writes.
        let fd = dev.as_raw_fd();
        // SAFETY: fcntl on a file descriptor this device owns.
        unsafe {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
        }
        let dev = Arc::new(Mutex::new(dev));
        let stop = Arc::new(AtomicBool::new(false));
        {
            let (dev, stop) = (Arc::clone(&dev), Arc::clone(&stop));
            std::thread::Builder::new()
                .name("gs-rumble".into())
                .spawn(move || force_feedback(&dev, fd, &stop, number as u16, &rumble))?;
        }
        Ok(Box::new(UinputPad {
            dev,
            stop,
            last: None,
        }))
    }

    /// Stick y: Moonlight's up is positive, evdev's down.
    fn flip(v: i16) -> i32 {
        (-i32::from(v)).min(32767)
    }

    fn hat(neg: bool, pos: bool) -> i32 {
        i32::from(pos) - i32::from(neg)
    }

    impl Pad for UinputPad {
        fn set(&mut self, s: &PadState) -> Result<()> {
            let old = self.last;
            let mut events = Vec::new();
            let key = |code: KeyCode, down: bool| {
                InputEvent::new(EventType::KEY.0, code.0, i32::from(down))
            };
            let abs =
                |code: AbsoluteAxisCode, v: i32| InputEvent::new(EventType::ABSOLUTE.0, code.0, v);
            for (bit, code) in KEYS {
                let down = s.buttons & bit != 0;
                if old.is_none_or(|o| (o.buttons & bit != 0) != down) {
                    events.push(key(code, down));
                }
            }
            let axes = |p: &PadState| {
                [
                    (AbsoluteAxisCode::ABS_X, i32::from(p.left.0)),
                    (AbsoluteAxisCode::ABS_Y, flip(p.left.1)),
                    (AbsoluteAxisCode::ABS_RX, i32::from(p.right.0)),
                    (AbsoluteAxisCode::ABS_RY, flip(p.right.1)),
                    (AbsoluteAxisCode::ABS_Z, i32::from(p.left_trigger)),
                    (AbsoluteAxisCode::ABS_RZ, i32::from(p.right_trigger)),
                    (
                        AbsoluteAxisCode::ABS_HAT0X,
                        hat(
                            p.buttons & button::LEFT != 0,
                            p.buttons & button::RIGHT != 0,
                        ),
                    ),
                    (
                        AbsoluteAxisCode::ABS_HAT0Y,
                        hat(p.buttons & button::UP != 0, p.buttons & button::DOWN != 0),
                    ),
                ]
            };
            let before = old.map(|o| axes(&o));
            for (k, (code, v)) in axes(s).into_iter().enumerate() {
                if before.is_none_or(|b| b[k].1 != v) {
                    events.push(abs(code, v));
                }
            }
            self.last = Some(*s);
            if events.is_empty() {
                return Ok(());
            }
            self.dev.lock().unwrap().emit(&events)?;
            Ok(())
        }
    }

    /// Force feedback the game uploads and plays on the pad, sent to the client as rumble.
    fn force_feedback(
        dev: &Mutex<VirtualDevice>,
        fd: i32,
        stop: &AtomicBool,
        number: u16,
        rumble: &Rumble,
    ) {
        // Effect id: strong and weak magnitude, length.
        let mut effects: HashMap<i16, (u16, u16, Duration)> = HashMap::new();
        let mut playing: Option<(i16, Option<Instant>)> = None;
        while !stop.load(Ordering::Relaxed) {
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd; the device (and its fd) outlives this thread's use.
            unsafe { libc::poll(&mut pfd, 1, 50) };
            if let Some((_, Some(end))) = playing
                && Instant::now() >= end
            {
                rumble(number, 0, 0);
                playing = None;
            }
            if pfd.revents & libc::POLLIN == 0 {
                continue;
            }
            let mut dev = dev.lock().unwrap();
            let events: Vec<InputEvent> = match dev.fetch_events() {
                Ok(e) => e.collect(),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(_) => return,
            };
            for event in events {
                match event.destructure() {
                    EventSummary::UInput(e, UInputCode::UI_FF_UPLOAD, ..) => {
                        let Ok(mut up) = dev.process_ff_upload(e) else {
                            continue;
                        };
                        let data = up.effect();
                        let id = up.effect_id();
                        if let FFEffectKind::Rumble {
                            strong_magnitude,
                            weak_magnitude,
                        } = data.kind
                        {
                            let length = Duration::from_millis(u64::from(data.replay.length));
                            effects.insert(id, (strong_magnitude, weak_magnitude, length));
                            // A game changes a playing effect by uploading it again.
                            if playing.is_some_and(|(p, _)| p == id) {
                                rumble(number, strong_magnitude, weak_magnitude);
                            }
                        }
                        up.set_retval(0);
                    }
                    EventSummary::UInput(e, UInputCode::UI_FF_ERASE, ..) => {
                        if let Ok(er) = dev.process_ff_erase(e) {
                            effects.remove(&(er.effect_id() as i16));
                        }
                    }
                    EventSummary::ForceFeedback(_, code, value) => {
                        let id = code.0 as i16;
                        match (value, effects.get(&id)) {
                            (v, Some(&(strong, weak, length))) if v > 0 => {
                                rumble(number, strong, weak);
                                let end = (!length.is_zero()).then(|| Instant::now() + length);
                                playing = Some((id, end));
                            }
                            _ => {
                                if playing.is_some_and(|(p, _)| p == id) {
                                    rumble(number, 0, 0);
                                    playing = None;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        if playing.is_some() {
            rumble(number, 0, 0);
        }
    }
}

#[cfg(windows)]
mod backend {
    //! ViGEmBus: an Xbox 360 target per controller.

    use anyhow::{Context, Result};
    use vigem_client::{Client, TargetId, XButtons, XGamepad, Xbox360Wired};

    use super::{Pad, PadState, Rumble};

    pub struct ViGEmPad {
        target: Xbox360Wired<Client>,
    }

    pub fn create(number: usize, rumble: Rumble) -> Result<Box<dyn Pad>> {
        let client = Client::connect().context("connecting to ViGEmBus (is it installed?)")?;
        let mut target = Xbox360Wired::new(client, TargetId::XBOX360_WIRED);
        target.plugin().context("plugging in the virtual pad")?;
        target.wait_ready().context("waiting for the virtual pad")?;
        // Rumble: ViGEm gives the motors as 0-255; the thread ends when the target goes.
        let notifications = target.request_notification()?;
        let number = number as u16;
        notifications.spawn_thread(move |_, n| {
            rumble(
                number,
                u16::from(n.large_motor) * 257,
                u16::from(n.small_motor) * 257,
            );
        });
        Ok(Box::new(ViGEmPad { target }))
    }

    impl Pad for ViGEmPad {
        fn set(&mut self, s: &PadState) -> Result<()> {
            self.target.update(&XGamepad {
                // XInput's bits are Moonlight's first 16.
                buttons: XButtons((s.buttons & 0xffff) as u16),
                left_trigger: s.left_trigger,
                right_trigger: s.right_trigger,
                thumb_lx: s.left.0,
                thumb_ly: s.left.1,
                thumb_rx: s.right.0,
                thumb_ry: s.right.1,
            })?;
            Ok(())
        }
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
mod backend {
    use super::{Pad, Rumble};

    pub fn create(_: usize, _: Rumble) -> anyhow::Result<Box<dyn Pad>> {
        anyhow::bail!("virtual gamepads are not supported on this system")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A controller message body as moonlight-common-c builds it.
    pub fn body(number: u16, mask: u16, s: &PadState) -> Vec<u8> {
        let mut b = Vec::new();
        for v in [0x001a, number, mask, 0x0014, (s.buttons & 0xffff) as u16] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.push(s.left_trigger);
        b.push(s.right_trigger);
        for v in [s.left.0, s.left.1, s.right.0, s.right.1] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        for v in [0x009c, (s.buttons >> 16) as u16, 0x0055] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b
    }

    #[test]
    fn controller_messages_parse() {
        let s = PadState {
            buttons: button::A | button::UP | 0x01_0000,
            left_trigger: 10,
            right_trigger: 255,
            left: (-32768, 32767),
            right: (5, -6),
        };
        assert_eq!(parse(&body(2, 0b101, &s)), Some((2, 0b101, s)));
        assert_eq!(parse(&body(2, 0b101, &s)[..10]), None);
    }

    /// Creates a pad through uinput and reads what it reports. Needs write access to
    /// /dev/uinput: `cargo test -- --ignored uinput` as root or with the udev rule.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore]
    fn uinput_pad_reports_the_state() {
        use std::sync::Mutex;
        let rumbles = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&rumbles);
        let mut pads = Gamepads::new(Arc::new(move |n, lo, hi| {
            seen.lock().unwrap().push((n, lo, hi))
        }));
        let first = PadState::default();
        pads.update(0, 1, &first);
        assert!(!pads.failed, "no uinput access");
        std::thread::sleep(std::time::Duration::from_millis(300));
        let path = std::fs::read_dir("/dev/input")
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| {
                evdev::Device::open(p)
                    .ok()
                    .and_then(|d| d.name().map(|n| n.starts_with("glidedesk gamepad")))
                    .unwrap_or(false)
            })
            .expect("the virtual pad in /dev/input");
        let mut dev = evdev::Device::open(&path).unwrap();
        let id = dev.input_id();
        assert_eq!((id.vendor(), id.product()), (0x045e, 0x028e));
        let s = PadState {
            buttons: button::A | button::RIGHT,
            left_trigger: 200,
            right_trigger: 0,
            left: (1000, 32767),
            right: (0, -32768),
        };
        pads.update(0, 1, &s);
        std::thread::sleep(std::time::Duration::from_millis(100));
        let keys = dev.get_key_state().unwrap();
        assert!(keys.contains(evdev::KeyCode::BTN_SOUTH));
        assert!(!keys.contains(evdev::KeyCode::BTN_EAST));
        let abs = dev.get_abs_state().unwrap();
        assert_eq!(abs[evdev::AbsoluteAxisCode::ABS_X.0 as usize].value, 1000);
        assert_eq!(abs[evdev::AbsoluteAxisCode::ABS_Y.0 as usize].value, -32767);
        assert_eq!(abs[evdev::AbsoluteAxisCode::ABS_RY.0 as usize].value, 32767);
        assert_eq!(abs[evdev::AbsoluteAxisCode::ABS_Z.0 as usize].value, 200);
        assert_eq!(abs[evdev::AbsoluteAxisCode::ABS_HAT0X.0 as usize].value, 1);

        // A game's rumble: upload an effect and play it.
        let effect = evdev::FFEffectData {
            direction: 0,
            trigger: evdev::FFTrigger::default(),
            replay: evdev::FFReplay {
                length: 300,
                delay: 0,
            },
            kind: evdev::FFEffectKind::Rumble {
                strong_magnitude: 0x8000,
                weak_magnitude: 0x1000,
            },
        };
        let mut ff = dev.upload_ff_effect(effect).unwrap();
        ff.play(1).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(600));
        let got = rumbles.lock().unwrap().clone();
        assert_eq!(got.first(), Some(&(0, 0x8000, 0x1000)), "{got:?}");
        // The effect's 300 ms run out: the motors stop.
        assert_eq!(got.last(), Some(&(0, 0, 0)), "{got:?}");
        drop(ff);
        pads.update(0, 0, &s);
        assert!(pads.pads[0].is_none());
    }
}
