use std::time::{Duration, Instant};

use wayland_client::{
    event_enum,
    protocol::{wl_keyboard::KeyState, wl_seat::WlSeat},
    DispatchData, Display, Filter, GlobalManager, Main,
};

use wayland_protocols::misc::zwp_input_method_v2::client::{
    zwp_input_method_keyboard_grab_v2::{Event as KeyEvent, ZwpInputMethodKeyboardGrabV2},
    zwp_input_method_manager_v2::ZwpInputMethodManagerV2,
    zwp_input_method_v2::{Event as ImEvent, ZwpInputMethodV2},
};
use zwp_virtual_keyboard::virtual_keyboard_unstable_v1::{
    zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1,
    zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
};

use kime_engine_cffi::{
    Config, InputEngine, InputResult, InputResult_CONSUMED, InputResult_HAS_COMMIT,
    InputResult_HAS_PREEDIT, InputResult_LANGUAGE_CHANGED, InputResult_NOT_READY, ModifierState,
    ModifierState_ALT, ModifierState_CONTROL, ModifierState_SHIFT, ModifierState_SUPER,
};

use mio::{unix::SourceFd, Events as MioEvents, Interest, Poll, Token};
use mio_timerfd::{ClockId, TimerFd};

event_enum! {
    Events |
    Key => ZwpInputMethodKeyboardGrabV2,
    Im => ZwpInputMethodV2
}

struct InputMethodState {
    activate: bool,
    deactivate: bool,
}

impl Default for InputMethodState {
    fn default() -> Self {
        Self {
            activate: false,
            deactivate: false,
        }
    }
}

#[derive(Clone, Copy)]
struct RepeatInfo {
    /// The rate of repeating keys in characters per second
    rate: i32,
    /// Delay in milliseconds since key down until repeating starts
    delay: i32,
}

#[derive(Clone, Copy)]
enum PressState {
    /// User is pressing no key, or user lifted last pressed key. But kime-wayland is ready for key
    /// long-press.
    NotPressing,
    /// User is pressing a key.
    Pressing {
        /// User started pressing a key at this moment.
        pressed_at: Instant,
        /// `false` if user just started pressing a key. Soon, key repeating will be begin. `true`
        /// if user have pressed a key for a long enough time, key repeating is happening right
        /// now.
        is_repeating: bool,

        /// Key code used by wayland
        key: u32,
        /// Timestamp with millisecond granularity used by wayland. Their base is undefined, so
        /// they can't be compared against system time (as obtained with clock_gettime or
        /// gettimeofday). They can be compared with each other though, and for instance be used to
        /// identify sequences of button presses as double or triple clicks.
        ///
        /// #### Reference
        /// - https://wayland.freedesktop.org/docs/html/ch04.html#sect-Protocol-Input
        wayland_time: u32,
    },
}

impl PressState {
    fn is_pressing(&self, query_key: u32) -> bool {
        if let PressState::Pressing { key, .. } = self {
            *key == query_key
        } else {
            false
        }
    }
}

struct KimeContext {
    config: Config,
    engine: InputEngine,
    mod_state: ModifierState,
    current_state: InputMethodState,
    pending_state: InputMethodState,
    vk: Main<ZwpVirtualKeyboardV1>,
    im: Main<ZwpInputMethodV2>,
    grab: Main<ZwpInputMethodKeyboardGrabV2>,
    engine_ready: bool,
    keymap_init: bool,
    grab_activate: bool,
    serial: u32,
    // Have to concern Multi seats?

    // Key repeat contexts
    timer: TimerFd,
    /// `None` if `KimeContext` have never received a `RepeatInfo` or repeat is disabled (i.e. rate
    /// is zero). `Some(..)` if `RepeatInfo` is known and kime-wayland started tracking the press
    /// state of keys.
    repeat_state: Option<(RepeatInfo, PressState)>,
}

impl Drop for KimeContext {
    fn drop(&mut self) {
        self.grab.release();
        self.vk.destroy();
        self.im.destroy();
    }
}

impl KimeContext {
    pub fn new(
        vk: Main<ZwpVirtualKeyboardV1>,
        im: Main<ZwpInputMethodV2>,
        grab: Main<ZwpInputMethodKeyboardGrabV2>,
        timer: TimerFd,
    ) -> Self {
        let config = Config::load();
        Self {
            engine: InputEngine::new(&config),
            config,
            mod_state: 0,
            current_state: InputMethodState::default(),
            pending_state: InputMethodState::default(),
            serial: 0,
            engine_ready: true,
            keymap_init: false,
            grab_activate: false,
            vk,
            im,
            grab,
            timer,
            repeat_state: None,
        }
    }

    pub fn new_data<'a>(data: &'a mut DispatchData) -> &'a mut Self {
        data.get::<Self>().unwrap()
    }

    fn process_input_result(&mut self, ret: InputResult) -> bool {
        if ret & InputResult_NOT_READY != 0 {
            self.engine_ready = false;
        }

        if ret & InputResult_LANGUAGE_CHANGED != 0 {
            self.engine.update_layout_state();
        }

        if ret & InputResult_HAS_PREEDIT != 0 {
            let preedit = self.engine.preedit_str().into();
            self.preedit(preedit);
        } else {
            self.clear_preedit();
        }

        if ret & InputResult_HAS_COMMIT != 0 {
            self.commit_string(self.engine.commit_str().into());
            self.engine.clear_commit();
        }

        self.commit();

        ret & InputResult_CONSUMED == 0
    }

    fn commit(&mut self) {
        self.im.commit(self.serial);
        self.serial += 1;
    }

    fn commit_string(&mut self, s: String) {
        if !s.is_empty() {
            self.im.commit_string(s);
        }
    }

    fn clear_preedit(&mut self) {
        self.im.set_preedit_string(String::new(), -1, -1);
    }

    fn preedit(&mut self, s: String) {
        let len = s.len();
        self.im.set_preedit_string(s, 0, len as _);
    }

    pub fn handle_im_ev(&mut self, ev: ImEvent) {
        match ev {
            ImEvent::Activate => {
                self.pending_state.activate = true;
            }
            ImEvent::Deactivate => {
                self.pending_state.deactivate = true;
            }
            ImEvent::Unavailable => {
                log::error!("Receive Unavailable event is another server already running?");
                panic!("Unavailable")
            }
            ImEvent::Done => {
                if !self.current_state.activate && self.pending_state.activate {
                    self.engine.update_layout_state();
                    if !self.engine_ready {
                        if self.engine.check_ready() {
                            let ret = self.engine.end_ready();
                            self.process_input_result(ret);
                            self.engine_ready = true;
                        }
                    }
                    self.grab_activate = true;
                } else if !self.current_state.deactivate && self.pending_state.deactivate {
                    // Focus lost, reset states
                    if self.engine_ready {
                        self.engine.reset();
                    }
                    self.grab_activate = false;

                    // Input deactivated, stop repeating
                    self.timer.disarm().unwrap();
                    if let Some((_, ref mut press_state)) = self.repeat_state {
                        *press_state = PressState::NotPressing
                    }
                }
                self.current_state = std::mem::take(&mut self.pending_state);
            }
            _ => {}
        }
    }

    pub fn handle_key_ev(&mut self, ev: KeyEvent) {
        match ev {
            KeyEvent::Keymap { fd, format, size } => {
                if !self.keymap_init {
                    self.vk.keymap(format as _, fd, size);
                    self.keymap_init = true;
                }
                unsafe {
                    libc::close(fd);
                }
            }
            KeyEvent::Key {
                state, key, time, ..
            } => {
                // NOTE: Never read `serial` of KeyEvent. You should rely on serial of KimeContext
                if state == KeyState::Pressed {
                    if self.grab_activate {
                        let ret =
                            self.engine
                                .press_key(&self.config, (key + 8) as u16, self.mod_state);

                        let bypassed = self.process_input_result(ret);

                        if bypassed {
                            // Bypassed key's repeat will be handled by the clients.
                            //
                            // Reference:
                            //   https://github.com/swaywm/sway/pull/4932#issuecomment-774113129
                            self.vk.key(time, key, state as _);
                        } else {
                            // If the key was not bypassed by IME, key repeat should be handled by the
                            // IME. Start waiting for the key hold timer event.
                            match self.repeat_state {
                                Some((info, ref mut press_state))
                                    if !press_state.is_pressing(key) =>
                                {
                                    let duration = Duration::from_millis(info.delay as u64);
                                    self.timer.set_timeout(&duration).unwrap();
                                    *press_state = PressState::Pressing {
                                        pressed_at: Instant::now(),
                                        is_repeating: false,
                                        key,
                                        wayland_time: time,
                                    };
                                }
                                _ => {}
                            }
                        }
                    } else {
                        // not activated so just skip
                        self.vk.key(time, key, state as _);
                    }
                } else {
                    // If user released the last pressed key, clear the timer and state
                    if let Some((.., ref mut press_state)) = self.repeat_state {
                        if press_state.is_pressing(key) {
                            self.timer.disarm().unwrap();
                            *press_state = PressState::NotPressing;
                        }
                    }

                    self.vk.key(time, key, state as _);
                }
            }
            KeyEvent::Modifiers {
                mods_depressed,
                mods_latched,
                mods_locked,
                group,
                ..
            } => {
                self.mod_state = 0;
                if mods_depressed & 0x1 != 0 {
                    self.mod_state |= ModifierState_SHIFT;
                }
                if mods_depressed & 0x4 != 0 {
                    self.mod_state |= ModifierState_CONTROL;
                }
                if mods_depressed & 0x8 != 0 {
                    self.mod_state |= ModifierState_ALT;
                }
                if mods_depressed & 0x40 != 0 {
                    self.mod_state |= ModifierState_SUPER;
                }
                self.vk
                    .modifiers(mods_depressed, mods_latched, mods_locked, group);
            }
            KeyEvent::RepeatInfo { rate, delay } => {
                self.repeat_state = if rate == 0 {
                    // Zero rate means disabled repeat
                    //
                    // Reference:
                    //   https://github.com/swaywm/wlroots/blob/3d46d3f7/protocol/input-method-unstable-v2.xml#L444-L455
                    None
                } else {
                    let info = RepeatInfo { rate, delay };
                    let press_state = self.repeat_state.map(|pair| pair.1);
                    Some((info, press_state.unwrap_or(PressState::NotPressing)))
                }
            }
            _ => {}
        }
    }

    pub fn handle_timer_ev(&mut self) -> std::io::Result<()> {
        // Read timer, this MUST be called or timer will be broken
        let overrun_count = self.timer.read()?;
        if overrun_count != 1 {
            log::warn!("Some timer events were not properly handled!");
        }

        if let Some((
            info,
            PressState::Pressing {
                pressed_at,
                ref mut is_repeating,
                key,
                wayland_time,
            },
        )) = self.repeat_state
        {
            if !*is_repeating {
                // Start repeat
                log::trace!("Start repeating {}", key);
                let interval = &Duration::from_secs_f64(1.0 / info.rate as f64);
                self.timer.set_timeout_interval(interval)?;
                *is_repeating = true;
            }

            // Emit key repeat event
            let ev = KeyEvent::Key {
                serial: self.serial,
                time: wayland_time + pressed_at.elapsed().as_millis() as u32,
                key,
                state: KeyState::Pressed,
            };
            self.serial += 1;
            self.handle_key_ev(ev);
        } else {
            log::warn!("Received timer event when it has never received RepeatInfo.");
        }

        Ok(())
    }
}

fn main() {
    kime_version::cli_boilerplate!((),);

    assert!(
        kime_engine_cffi::check_api_version(),
        "Engine version mismatched"
    );

    let display = Display::connect_to_env().expect("Failed to connect wayland display");
    let mut event_queue = display.create_event_queue();
    let attached_display = display.attach(event_queue.token());
    let globals = GlobalManager::new(&attached_display);

    event_queue.sync_roundtrip(&mut (), |_, _, _| ()).unwrap();

    let seat = globals.instantiate_exact::<WlSeat>(1).expect("Load Seat");
    let im_manager = globals
        .instantiate_exact::<ZwpInputMethodManagerV2>(1)
        .expect("Load InputManager");
    let vk_manager = globals
        .instantiate_exact::<ZwpVirtualKeyboardManagerV1>(1)
        .expect("Load VirtualKeyboardManager");

    let filter = Filter::new(|ev, _filter, mut data| {
        let ctx = KimeContext::new_data(&mut data);

        match ev {
            Events::Key { event, .. } => {
                ctx.handle_key_ev(event);
            }
            Events::Im { event, .. } => {
                ctx.handle_im_ev(event);
            }
        }
    });

    let vk = vk_manager.create_virtual_keyboard(&seat);
    let im = im_manager.get_input_method(&seat);
    let grab = im.grab_keyboard();
    grab.assign(filter.clone());
    im.assign(filter);

    // Initialize timer
    let mut timer = TimerFd::new(ClockId::Monotonic).expect("Initialize timer");

    // Initialize epoll() object
    let mut poll = Poll::new().expect("Initialize epoll()");
    let registry = poll.registry();

    const POLL_WAYLAND: Token = Token(0);
    registry
        .register(
            &mut SourceFd(&display.get_connection_fd()),
            POLL_WAYLAND,
            Interest::READABLE | Interest::WRITABLE,
        )
        .expect("Register wayland socket to the epoll()");

    const POLL_TIMER: Token = Token(1);
    registry
        .register(&mut timer, POLL_TIMER, Interest::READABLE)
        .expect("Register timer to the epoll()");

    // Initialize kime context
    let mut kime_ctx = KimeContext::new(vk, im, grab, timer);
    event_queue
        .sync_roundtrip(&mut kime_ctx, |_, _, _| ())
        .unwrap();

    log::info!("Server init success!");

    // Non-blocking event loop
    //
    // Reference:
    //   https://docs.rs/wayland-client/0.28.3/wayland_client/struct.EventQueue.html
    let mut events = MioEvents::with_capacity(1024);
    let stop_reason = 'main: loop {
        use std::io::ErrorKind;

        // Sleep until next event
        if let Err(e) = poll.poll(&mut events, None) {
            // Should retry on EINTR
            //
            // Reference:
            //   https://www.gnu.org/software/libc/manual/html_node/Interrupted-Primitives.html
            if e.kind() == ErrorKind::Interrupted {
                continue;
            }
            break Err(e);
        }

        for event in &events {
            match event.token() {
                POLL_WAYLAND => {}
                POLL_TIMER => {
                    if let Err(e) = kime_ctx.handle_timer_ev() {
                        break 'main Err(e);
                    }
                }
                _ => unreachable!(),
            }
        }

        // Perform read() only when it's ready, returns None when there're already pending events
        if let Some(guard) = event_queue.prepare_read() {
            if let Err(e) = guard.read_events() {
                // EWOULDBLOCK here means there's no new messages to read
                if e.kind() != ErrorKind::WouldBlock {
                    break Err(e);
                }
            }
        }

        if let Err(e) = event_queue.dispatch_pending(&mut kime_ctx, |_, _, _| {}) {
            break Err(e);
        }

        // Flush pending writes
        if let Err(e) = display.flush() {
            // EWOULDBLOCK here means there're so many to write, retry later
            if e.kind() != ErrorKind::WouldBlock {
                break Err(e);
            }
        }
    };

    match stop_reason {
        Ok(()) => log::info!("Server finished gracefully"),
        Err(e) => log::error!("Server aborted due to IO Error: {}", e),
    }
}
