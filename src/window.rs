/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! Abstraction of window setup, OpenGL context creation and event handling.
//!
//! Implemented using the sdl2 crate (a Rust wrapper for SDL2). All usage of
//! SDL should be confined to this module.
//!
//! There is currently no separation of concerns between a single window and
//! window system interaction in general, because it is assumed only one window
//! will be needed for the runtime of the app.

use crate::gles::present::present_frame;
use crate::gles::{create_gles1_ctx_no_parent_stack, GLESContext, GLES};
use crate::image::Image;
use crate::matrix::Matrix;
use crate::options::Options;
#[cfg(not(target_os = "android"))]
use crate::sync::model::{LocalOrRemote, SnapshotEntry};
#[cfg(not(target_os = "android"))]
use crate::sync::reconcile::{ConflictChoice, RemoteVersionId, SyncPlan};
use crate::Environment;
use sdl2::mouse::MouseButton;
use sdl2::pixels::PixelFormatEnum;
use sdl2::surface::Surface;
use sdl2_sys::SDL_PowerState;
use std::collections::{HashMap, VecDeque};
use std::env;
use std::f32::consts::{FRAC_PI_2, PI};
use std::num::NonZeroU32;
use std::ptr::null_mut;
use std::time::{Duration, Instant};

#[allow(non_camel_case_types)]
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum DeviceFamily {
    iPhone,
    iPad,
}
impl std::fmt::Display for DeviceFamily {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}
impl DeviceFamily {
    pub fn portrait_size(&self) -> (u32, u32) {
        match self {
            DeviceFamily::iPhone => (320, 480),
            DeviceFamily::iPad => (768, 1024),
        }
    }
}
impl TryFrom<u64> for DeviceFamily {
    type Error = ();
    fn try_from(value: u64) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(DeviceFamily::iPhone),
            2 => Ok(DeviceFamily::iPad),
            _ => Err(()),
        }
    }
}
impl TryFrom<&str> for DeviceFamily {
    type Error = ();
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "iphone" => Ok(DeviceFamily::iPhone),
            "ipad" => Ok(DeviceFamily::iPad),
            _ => Err(()),
        }
    }
}

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum DeviceOrientation {
    Portrait,
    PortraitUpsideDown,
    LandscapeLeft,
    LandscapeRight,
}
fn size_for_orientation(
    family: DeviceFamily,
    orientation: DeviceOrientation,
    scale_hack: NonZeroU32,
) -> (u32, u32) {
    let (width, height) = family.portrait_size();
    let scale_hack = scale_hack.get();
    match orientation {
        DeviceOrientation::Portrait => (width * scale_hack, height * scale_hack),
        DeviceOrientation::PortraitUpsideDown => (width * scale_hack, height * scale_hack),
        DeviceOrientation::LandscapeLeft => (height * scale_hack, width * scale_hack),
        DeviceOrientation::LandscapeRight => (height * scale_hack, width * scale_hack),
    }
}
fn rotate_fullscreen_size(orientation: DeviceOrientation, screen_size: (u32, u32)) -> (u32, u32) {
    let (short_side, long_side) = if screen_size.0 < screen_size.1 {
        (screen_size.0, screen_size.1)
    } else {
        (screen_size.1, screen_size.0)
    };
    match orientation {
        DeviceOrientation::Portrait | DeviceOrientation::PortraitUpsideDown => {
            (short_side, long_side)
        }
        DeviceOrientation::LandscapeLeft | DeviceOrientation::LandscapeRight => {
            (long_side, short_side)
        }
    }
}
/// Tell SDL2 what orientation we want. Only useful on Android.
fn set_sdl2_orientation(orientation: DeviceOrientation) {
    // Despite the name, this hint works on Android too.
    sdl2::hint::set(
        "SDL_IOS_ORIENTATIONS",
        match orientation {
            DeviceOrientation::Portrait => "Portrait",
            // The inversion is deliberate. These probably correspond to
            // iPhone OS content orientations?
            DeviceOrientation::PortraitUpsideDown => "PortraitUpsideDown",
            DeviceOrientation::LandscapeLeft => "LandscapeRight",
            DeviceOrientation::LandscapeRight => "LandscapeLeft",
        },
    );
}

fn set_sdl2_picker_orientations() {
    sdl2::hint::set(
        "SDL_IOS_ORIENTATIONS",
        "Portrait LandscapeLeft LandscapeRight",
    );
}

fn device_orientation_from_display(
    orientation: sdl2_sys::SDL_DisplayOrientation,
) -> Option<DeviceOrientation> {
    match orientation {
        sdl2_sys::SDL_DisplayOrientation::SDL_ORIENTATION_LANDSCAPE => {
            Some(DeviceOrientation::LandscapeLeft)
        }
        sdl2_sys::SDL_DisplayOrientation::SDL_ORIENTATION_LANDSCAPE_FLIPPED => {
            Some(DeviceOrientation::LandscapeRight)
        }
        sdl2_sys::SDL_DisplayOrientation::SDL_ORIENTATION_PORTRAIT => {
            Some(DeviceOrientation::Portrait)
        }
        sdl2_sys::SDL_DisplayOrientation::SDL_ORIENTATION_PORTRAIT_FLIPPED => {
            Some(DeviceOrientation::PortraitUpsideDown)
        }
        sdl2_sys::SDL_DisplayOrientation::SDL_ORIENTATION_UNKNOWN => None,
    }
}

fn picker_scale_hack(
    display_size: (u32, u32),
    family: DeviceFamily,
    orientation: DeviceOrientation,
) -> NonZeroU32 {
    let (display_width, display_height) = rotate_fullscreen_size(orientation, display_size);
    let (logical_width, logical_height) =
        size_for_orientation(family, orientation, NonZeroU32::new(1).unwrap());
    let scale = (display_width / logical_width)
        .min(display_height / logical_height)
        .clamp(1, 4);
    NonZeroU32::new(scale).unwrap()
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum FingerId {
    Mouse,
    Touch(i64),
    VirtualCursor,
    ButtonToTouch(crate::options::Button),
    StickToTouch,
    DpadToTouch,
}
pub type Coords = (f32, f32);

struct DpadState {
    left: bool,
    right: bool,
    up: bool,
    down: bool,
    active: bool,
}

#[derive(Debug)]
pub enum TextInputEvent {
    Text(String),
    Backspace,
    Return,
}

#[derive(Debug)]
pub enum Event {
    /// User requested quit.
    Quit,
    /// OS has informed touchHLE it will soon become inactive.
    /// (iOS `applicationWillResignActive:`, Android `onPause()`)
    AppWillResignActive,
    /// OS has informed touchHLE it will soon terminate.
    /// (iOS `applicationWillTerminate:`, Android `onDestroy()`)
    AppWillTerminate,
    TouchesDown(HashMap<FingerId, Coords>),
    TouchesMove(HashMap<FingerId, Coords>),
    TouchesUp(HashMap<FingerId, Coords>),
    /// User pressed F12, requesting that execution be paused and the debugger
    /// take over.
    EnterDebugger,
    TextInput(TextInputEvent),
}

pub enum BatteryState {
    Unknown,
    OnBattery,
    NoBattery,
    Charging,
    Full,
}

pub enum GLVersion {
    /// OpenGL ES 1.1
    GLES11,
    /// OpenGL 2.1 compatibility profile
    GL21Compat,
}

pub struct GLContext(sdl2::video::GLContext);

impl GLContext {
    pub fn is_current(&self) -> bool {
        self.0.is_current()
    }
}

fn surface_from_image(image: &Image) -> Surface<'_> {
    let src_pixels = image.pixels();
    let (width, height) = image.dimensions();

    let mut surface = Surface::new(width, height, PixelFormatEnum::RGBA32).unwrap();
    let (width, height) = (width as usize, height as usize);
    let pitch = surface.pitch() as usize;
    surface.with_lock_mut(|dst_pixels| {
        for y in 0..height {
            for x in 0..width {
                for channel in 0..4 {
                    let src_idx = y * width * 4 + x * 4 + channel;
                    let dst_idx = y * pitch + x * 4 + channel;
                    dst_pixels[dst_idx] = src_pixels[src_idx];
                }
            }
        }
    });
    surface
}

pub struct Window {
    _sdl_ctx: sdl2::Sdl,
    video_ctx: sdl2::VideoSubsystem,
    window: sdl2::video::Window,
    event_pump: sdl2::EventPump,
    event_queue: VecDeque<Event>,
    last_polled: Instant,
    /// Separate queue for extremely high-priority events (e.g. app about to
    /// terminate).
    high_priority_event: Option<Event>,
    enable_event_polling: bool,
    #[cfg(target_os = "macos")]
    max_height: u32,
    #[cfg(target_os = "macos")]
    viewport_y_offset: u32,
    /// Copy of `fullscreen` on [Options]. Note that this is meaningless when
    /// [Self::rotatable_fullscreen] returns [true].
    fullscreen: bool,
    picker_auto_rotate: bool,
    scale_hack: NonZeroU32,
    internal_gl_ins: Option<Box<dyn GLESContext>>,
    splash_image: Option<Image>,
    device_family: DeviceFamily,
    device_orientation: DeviceOrientation,
    controller_ctx: sdl2::GameControllerSubsystem,
    controllers: Vec<sdl2::controller::GameController>,
    dpad_state: DpadState,
    stick_active: bool,
    _sensor_ctx: sdl2::SensorSubsystem,
    accelerometer: Option<sdl2::sensor::Sensor>,
    virtual_cursor_last: Option<(f32, f32, bool, bool)>,
    virtual_cursor_last_unsticky: Option<(f32, f32, Instant)>,
    virtual_accelerometer_last: Option<(f32, f32, bool)>,
    /// Whether or not we are on the "main" environment stack (rather than
    /// a coroutine stack). Checked in various functions to make sure that
    /// certain SDL functions (that call JNI functions) are on the main
    /// stack on Android.
    pub(super) on_main_stack: bool,
    guest_launch_started_at: Option<Instant>,
}

impl Window {
    /// Returns [true] if touchHLE is running on a device where we should always
    /// display fullscreen, but SDL2 will let us control the orientation, i.e.
    /// Android devices.
    pub fn rotatable_fullscreen() -> bool {
        env::consts::OS == "android"
    }
    pub fn new(
        title: &str,
        icon: Option<Image>,
        launch_image: Option<Image>,
        options: &mut Options,
        is_app_picker: bool,
    ) -> Window {
        let sdl_ctx = sdl2::init().unwrap();
        let video_ctx = sdl_ctx.video().unwrap();

        // The "hidapi" feature of rust-sdl2 is enabled so that sdl2::sensor
        // is available, but we don't want to enable SDL's HIDAPI controller
        // drivers because they cause duplicated controllers on macOS
        // (https://github.com/libsdl-org/SDL/issues/7479). Once that's fixed,
        // remove this (https://github.com/touchHLE/touchHLE/issues/85).
        sdl2::hint::set("SDL_JOYSTICK_HIDAPI", "0");

        if env::consts::OS == "android" {
            // It's important to set context version BEFORE window creation
            // ref. https://wiki.libsdl.org/SDL2/SDL_GLattr
            let attr = video_ctx.gl_attr();
            attr.set_context_version(1, 1);
            attr.set_context_profile(sdl2::video::GLProfile::GLES);

            // Disable blocking of event loop when app is paused.
            sdl2::hint::set("SDL_ANDROID_BLOCK_ON_PAUSE", "0");
        }

        // Separate mouse and touch events
        sdl2::hint::set("SDL_TOUCH_MOUSE_EVENTS", "0");

        // SDL2 disables the screen saver by default, but iPhone OS enables
        // the idle timer that triggers sleep by default, so we turn it back on
        // here, and then the app can disable it if it wants to.
        video_ctx.enable_screen_saver();

        let picker_auto_rotate = is_app_picker && Self::rotatable_fullscreen();
        if picker_auto_rotate {
            set_sdl2_picker_orientations();
            let display_size = video_ctx.display_bounds(0).unwrap().size();
            let orientation = unsafe { sdl2_sys::SDL_GetDisplayOrientation(0) };
            options.initial_orientation =
                device_orientation_from_display(orientation).unwrap_or(options.initial_orientation);
            options.scale_hack = picker_scale_hack(
                display_size,
                options.device_family.unwrap_or(DeviceFamily::iPhone),
                options.initial_orientation,
            );
            log!(
                "Android picker display {:?}, orientation {:?}, render scale {}x",
                display_size,
                options.initial_orientation,
                options.scale_hack
            );
        }

        let scale_hack = options.scale_hack;
        // TODO: some apps specify their orientation in Info.plist, we could use
        // that here.
        let device_family = options.device_family.unwrap_or(DeviceFamily::iPhone);
        let device_orientation = options.initial_orientation;
        let fullscreen = options.fullscreen;

        let mut window = if Self::rotatable_fullscreen() {
            // Without this, SDL will force fullscreen mode to be portrait.
            if !picker_auto_rotate {
                set_sdl2_orientation(device_orientation);
            }
            let screen_size = video_ctx.display_bounds(0).unwrap().size();
            let (width, height) = rotate_fullscreen_size(device_orientation, screen_size);
            let window = if picker_auto_rotate {
                video_ctx
                    .window(title, width, height)
                    .fullscreen()
                    .opengl()
                    .resizable()
                    .build()
                    .unwrap()
            } else {
                video_ctx
                    .window(title, width, height)
                    .fullscreen()
                    .opengl()
                    .build()
                    .unwrap()
            };
            window
        } else if fullscreen {
            let (width, height) = video_ctx.display_bounds(0).unwrap().size();
            let window = video_ctx
                .window(title, width, height)
                .fullscreen_desktop()
                .opengl()
                .build()
                .unwrap();
            window
        } else {
            let (width, height) =
                size_for_orientation(device_family, device_orientation, scale_hack);
            let window = video_ctx
                .window(title, width, height)
                .position_centered()
                .opengl()
                .build()
                .unwrap();
            window
        };

        if env::consts::OS == "android" {
            // Sanity check
            let gl_attr = video_ctx.gl_attr();
            debug_assert_eq!(gl_attr.context_profile(), sdl2::video::GLProfile::GLES);
            debug_assert_eq!(gl_attr.context_version(), (1, 1));
        }

        if let Some(icon) = icon {
            window.set_icon(surface_from_image(&icon));
        }

        let event_pump = sdl_ctx.event_pump().unwrap();

        let controller_ctx = sdl_ctx.game_controller().unwrap();

        let sensor_ctx = sdl_ctx.sensor().unwrap();
        let mut accelerometer: Option<sdl2::sensor::Sensor> = None;
        if let Ok(num_sensors) = sensor_ctx.num_sensors() {
            for sensor_idx in 0..num_sensors {
                if let Ok(sensor) = sensor_ctx.open(sensor_idx) {
                    if sensor.sensor_type() == sdl2::sensor::SensorType::Accelerometer {
                        log!("Accelerometer detected: {}.", sensor.name());
                        accelerometer = Some(sensor);
                        break;
                    }
                }
            }
        }

        #[cfg(target_os = "macos")]
        let max_height = window.size().1;

        let mut window = Window {
            _sdl_ctx: sdl_ctx,
            video_ctx,
            window,
            event_pump,
            event_queue: VecDeque::new(),
            last_polled: Instant::now() - Duration::from_secs(1),
            high_priority_event: None,
            enable_event_polling: true,
            #[cfg(target_os = "macos")]
            max_height,
            #[cfg(target_os = "macos")]
            viewport_y_offset: 0,
            fullscreen,
            picker_auto_rotate,
            scale_hack,
            internal_gl_ins: None,
            splash_image: launch_image,
            device_family,
            device_orientation,
            controller_ctx,
            controllers: Vec::new(),
            dpad_state: DpadState {
                left: false,
                right: false,
                up: false,
                down: false,
                active: false,
            },
            stick_active: false,
            _sensor_ctx: sensor_ctx,
            accelerometer,
            virtual_cursor_last: None,
            virtual_cursor_last_unsticky: None,
            virtual_accelerometer_last: None,
            on_main_stack: true,
            guest_launch_started_at: None,
        };

        // Set up OpenGL ES context used for splash screen and app UI rendering
        // (see src/frameworks/core_animation/composition.rs). OpenGL ES is used
        // because SDL2 won't let us use more than one graphics API in the same
        // window, and we also need OpenGL ES for the app's own rendering.
        let mut gl_ins = create_gles1_ctx_no_parent_stack(&mut window, options);
        {
            let gl_ctx = gl_ins.make_current(&mut window);
            log!("Driver info: {}", unsafe { gl_ctx.driver_description() });
        }
        window.internal_gl_ins = Some(gl_ins);

        if window.splash_image.is_some() {
            window.display_splash();
        }

        window
    }

    /// Poll for events from the OS. This needs to be done reasonably often
    /// (60Hz is probably fine) so that the host OS doesn't consider touchHLE
    /// to be unresponsive. Note that events are not returned by this function,
    /// since we often need to defer actually handling them.
    ///
    /// Since polling can be quite expensive, this function will skip it if it
    /// was called too recently.
    pub fn poll_for_events(&mut self, options: &Options) {
        assert!(self.on_main_stack);
        let now = Instant::now();
        // poll roughly twice per frame to try to avoid missing frames sometimes
        if now.duration_since(self.last_polled) < Duration::from_secs_f64(1.0 / 120.0) {
            return;
        }
        self.last_polled = now;

        if self.picker_auto_rotate {
            let display_orientation = unsafe { sdl2_sys::SDL_GetDisplayOrientation(0) };
            if let Some(orientation) = device_orientation_from_display(display_orientation) {
                self.device_orientation = orientation;
            }
        }

        fn transform_input_coords(
            window: &Window,
            (in_x, in_y): (f32, f32),
            independent_of_viewport: bool,
        ) -> (f32, f32) {
            let (vx, vy, vw, vh) = if independent_of_viewport {
                let (width, height) = size_for_orientation(
                    window.device_family,
                    window.device_orientation,
                    NonZeroU32::new(1).unwrap(),
                );
                (0, 0, width, height)
            } else {
                window.viewport()
            };
            // normalize to unit square centred on origin
            let x = (in_x - vx as f32) / vw as f32 - 0.5;
            let y = (in_y - vy as f32) / vh as f32 - 0.5;
            // rotate
            let matrix = window.rotation_matrix().inverse().unwrap();
            let [x, y] = matrix.transform([x, y]);
            // back to pixels
            let (out_w, out_h) = window.size_unrotated_unscaled();
            let out_x = (x + 0.5) * out_w as f32;
            let out_y = (y + 0.5) * out_h as f32;
            // Round to match touch precision of official devices.
            (out_x.round(), out_y.round())
        }
        fn transform_virt_accel_coords(window: &Window, (in_x, in_y): (i32, i32)) -> (f32, f32) {
            let (_, _, vw, vh) = window.viewport();
            let out_x = ((in_x as f32 / vw as f32) * 2.0 - 1.0).clamp(-1.0, 1.0);
            let out_y = ((in_y as f32 / vh as f32) * 2.0 - 1.0).clamp(-1.0, 1.0);
            (out_x, out_y)
        }
        fn translate_button(button: sdl2::controller::Button) -> Option<crate::options::Button> {
            match button {
                sdl2::controller::Button::DPadLeft => Some(crate::options::Button::DPadLeft),
                sdl2::controller::Button::DPadUp => Some(crate::options::Button::DPadUp),
                sdl2::controller::Button::DPadRight => Some(crate::options::Button::DPadRight),
                sdl2::controller::Button::DPadDown => Some(crate::options::Button::DPadDown),
                sdl2::controller::Button::Start => Some(crate::options::Button::Start),
                sdl2::controller::Button::A => Some(crate::options::Button::A),
                sdl2::controller::Button::B => Some(crate::options::Button::B),
                sdl2::controller::Button::X => Some(crate::options::Button::X),
                sdl2::controller::Button::Y => Some(crate::options::Button::Y),
                sdl2::controller::Button::LeftShoulder => {
                    Some(crate::options::Button::LeftShoulder)
                }
                _ => None,
            }
        }
        fn finger_absolute_coords(window: &Window, (x, y): (f32, f32)) -> (f32, f32) {
            let (screen_width, screen_height) = window.window.drawable_size();
            (screen_width as f32 * x, screen_height as f32 * y)
        }

        let mut controller_updated = false;
        // event_pump doesn't have a method to peek on events
        // so, we keep track of an unconsumed one from a previous loop iteration
        // FIXME: use peek_event() from even_subsystem
        let mut previous_event: Option<sdl2::event::Event> = None;
        while self.enable_event_polling {
            use sdl2::event::Event as E;
            let event = if let Some(e) = previous_event.take() {
                match e {
                    E::Unknown { .. } => (),
                    _ => log_dbg!("Consuming previous event: {:?}", e),
                }
                e
            } else if let Some(e) = self.event_pump.poll_event() {
                match e {
                    E::Unknown { .. } => (),
                    _ => log_dbg!("Consuming new event: {:?}", e),
                }
                e
            } else {
                break;
            };

            // Virtual accelerometer
            match event {
                E::MouseButtonDown {
                    x,
                    y,
                    mouse_btn: MouseButton::Right,
                    ..
                } => {
                    let (x, y) = transform_virt_accel_coords(self, (x, y));
                    self.virtual_accelerometer_last = Some((x, y, true));
                }
                E::MouseMotion {
                    x, y, mousestate, ..
                } if mousestate.right() => {
                    let (x, y) = transform_virt_accel_coords(self, (x, y));
                    self.virtual_accelerometer_last = Some((x, y, true));
                }
                E::MouseButtonUp {
                    x,
                    y,
                    mouse_btn: MouseButton::Right,
                    ..
                } => {
                    let (x, y) = transform_virt_accel_coords(self, (x, y));
                    self.virtual_accelerometer_last = Some((x, y, false));
                }
                _ => {}
            }

            self.event_queue.push_back(match event {
                E::Quit { .. } => Event::Quit,
                E::MouseButtonDown {
                    x,
                    y,
                    mouse_btn: MouseButton::Left,
                    ..
                } => {
                    let coords = transform_input_coords(self, (x as f32, y as f32), false);
                    log_dbg!("MouseButtonDown x {}, y {}, coords {:?}", x, y, coords);
                    Event::TouchesDown(HashMap::from([(FingerId::Mouse, coords)]))
                }
                E::MouseMotion {
                    x, y, mousestate, ..
                } if mousestate.left() => {
                    let coords = transform_input_coords(self, (x as f32, y as f32), false);
                    log_dbg!("MouseMotion x {}, y {}, coords {:?}", x, y, coords);
                    Event::TouchesMove(HashMap::from([(FingerId::Mouse, coords)]))
                }
                E::MouseButtonUp {
                    x,
                    y,
                    mouse_btn: MouseButton::Left,
                    ..
                } => {
                    let coords = transform_input_coords(self, (x as f32, y as f32), false);
                    log_dbg!("MouseButtonUp x {}, y {}, coords {:?}", x, y, coords);
                    Event::TouchesUp(HashMap::from([(FingerId::Mouse, coords)]))
                }
                E::ControllerDeviceAdded { which, .. } => {
                    self.controller_added(which);
                    continue;
                }
                E::ControllerDeviceRemoved { which, .. } => {
                    self.controller_removed(which);
                    continue;
                }
                // Note that accelerometer simulation with analog sticks is
                // handled with polling, rather than being event-based.
                E::ControllerButtonUp { button, .. } | E::ControllerButtonDown { button, .. } => {
                    controller_updated = true;
                    let Some(button) = translate_button(button) else {
                        continue;
                    };
                    // Called whenever a DPad direction is pressed or released
                    if (button == crate::options::Button::DPadLeft
                        || button == crate::options::Button::DPadUp
                        || button == crate::options::Button::DPadRight
                        || button == crate::options::Button::DPadDown)
                        && options.dpad_to_touch.is_some()
                    {
                        let Some((x, y, w, h)) = options.dpad_to_touch else {
                            unreachable!();
                        };

                        // Update held state
                        let pressed = matches!(event, E::ControllerButtonDown { .. });
                        match button {
                            crate::options::Button::DPadLeft => self.dpad_state.left = pressed,
                            crate::options::Button::DPadRight => self.dpad_state.right = pressed,
                            crate::options::Button::DPadUp => self.dpad_state.up = pressed,
                            crate::options::Button::DPadDown => self.dpad_state.down = pressed,
                            _ => unreachable!(),
                        }

                        // Compute center
                        let cx = x + w * 0.5;
                        let cy = y + h * 0.5;

                        // Compute combined delta
                        let mut dx = 0.0;
                        let mut dy = 0.0;

                        if self.dpad_state.left {
                            dx -= 0.5 * w;
                        }
                        if self.dpad_state.right {
                            dx += 0.5 * w;
                        }
                        if self.dpad_state.up {
                            dy -= 0.5 * h;
                        }
                        if self.dpad_state.down {
                            dy += 0.5 * h;
                        }

                        // Final coords: center + movement
                        let coords = transform_input_coords(self, (cx + dx, cy + dy), true);

                        // Send TouchDown if any dpad is held, TouchUp if none
                        let any_held = self.dpad_state.left
                            || self.dpad_state.right
                            || self.dpad_state.up
                            || self.dpad_state.down;

                        if !self.dpad_state.active && any_held {
                            // New touch
                            self.dpad_state.active = true;
                            Event::TouchesDown(HashMap::from([(FingerId::DpadToTouch, coords)]))
                        } else if self.dpad_state.active && any_held {
                            // Move existing touch
                            Event::TouchesMove(HashMap::from([(FingerId::DpadToTouch, coords)]))
                        } else if self.dpad_state.active && !any_held {
                            // Release touch
                            self.dpad_state.active = false;
                            Event::TouchesUp(HashMap::from([(FingerId::DpadToTouch, coords)]))
                        } else {
                            continue;
                        }
                    } else {
                        let Some(&(x, y)) = options.button_to_touch.get(&button) else {
                            continue;
                        };
                        match event {
                            E::ControllerButtonUp { .. } => {
                                let coords = transform_input_coords(self, (x, y), true);
                                Event::TouchesUp(HashMap::from([(
                                    FingerId::ButtonToTouch(button),
                                    coords,
                                )]))
                            }
                            E::ControllerButtonDown { .. } => {
                                let coords = transform_input_coords(self, (x, y), true);
                                Event::TouchesDown(HashMap::from([(
                                    FingerId::ButtonToTouch(button),
                                    coords,
                                )]))
                            }
                            _ => unreachable!(),
                        }
                    }
                }
                E::ControllerAxisMotion { axis, .. } => {
                    controller_updated = true;
                    let Some((x, y, w, h)) = options.stick_to_touch else {
                        continue;
                    };
                    if axis == sdl2::controller::Axis::LeftX
                        || axis == sdl2::controller::Axis::LeftY
                    {
                        let (stick_x, stick_y, _) = self.get_controller_stick(options, true);
                        let coords = transform_input_coords(
                            self,
                            (
                                x + ((stick_x + 1.0) / 2.0) * w,
                                y + ((stick_y + 1.0) / 2.0) * h,
                            ),
                            true,
                        );
                        if stick_x.abs() < options.deadzone && stick_y.abs() < options.deadzone {
                            if !self.stick_active {
                                // Ignore deadzone events when stick is inactive
                                continue;
                            } else {
                                // Release touch when stick returns to deadzone
                                self.stick_active = false;
                                Event::TouchesUp(HashMap::from([(FingerId::StickToTouch, coords)]))
                            }
                        } else if !self.stick_active {
                            // New touch
                            self.stick_active = true;
                            Event::TouchesDown(HashMap::from([(FingerId::StickToTouch, coords)]))
                        } else {
                            // Move existing touch
                            Event::TouchesMove(HashMap::from([(FingerId::StickToTouch, coords)]))
                        }
                    } else {
                        continue;
                    }
                }
                E::AppWillEnterBackground { .. } => {
                    log!("Received app-will-resign-active event.");
                    assert!(self.high_priority_event.is_none());
                    self.high_priority_event = Some(Event::AppWillResignActive);
                    // For some reason, if we don't pause event polling, we will
                    // never finish handling the event.
                    // TODO: Add a mechanism for re-enabling polling, if at some
                    // point we support returning touchHLE to the foreground.
                    self.enable_event_polling = false;
                    continue;
                }
                E::AppTerminating { .. } => {
                    log!("Received app-will-terminate event.");
                    assert!(self.high_priority_event.is_none());
                    self.high_priority_event = Some(Event::AppWillTerminate);
                    self.enable_event_polling = false;
                    continue;
                }
                E::FingerUp {
                    timestamp,
                    finger_id,
                    x,
                    y,
                    ..
                }
                | E::FingerMotion {
                    timestamp,
                    finger_id,
                    x,
                    y,
                    ..
                }
                | E::FingerDown {
                    timestamp,
                    finger_id,
                    x,
                    y,
                    ..
                } => {
                    log_dbg!("Starting multi-touch for {:?}", event);
                    // To implement multi-touch we accumulate here same touch
                    // events at the same timestamp. This is consistent with
                    // UIKit, but could be broken if events come out of order.
                    // (in worst case we separate multi-touches in several ones)
                    // TODO: handle out of order touches
                    let curr_timestamp = timestamp;
                    let abs_coords = finger_absolute_coords(self, (x, y));
                    let coords = transform_input_coords(self, abs_coords, false);
                    log_dbg!("Finger event x {}, y {}, coords {:?}", x, y, coords);
                    let mut map = HashMap::from([(FingerId::Touch(finger_id), coords)]);
                    while let Some(next) = self.event_pump.poll_event() {
                        match next {
                            E::Unknown { .. } => (),
                            _ => log_dbg!("Next possible multi-touch event: {:?}", next),
                        }
                        match next {
                            E::FingerUp {
                                timestamp,
                                finger_id,
                                x,
                                y,
                                ..
                            }
                            | E::FingerMotion {
                                timestamp,
                                finger_id,
                                x,
                                y,
                                ..
                            }
                            | E::FingerDown {
                                timestamp,
                                finger_id,
                                x,
                                y,
                                ..
                            } if timestamp == curr_timestamp && next.is_same_kind_as(&event) => {
                                let abs_coords = finger_absolute_coords(self, (x, y));
                                let coords = transform_input_coords(self, abs_coords, false);
                                map.insert(FingerId::Touch(finger_id), coords);
                            }
                            E::MultiGesture { timestamp, .. } if timestamp == curr_timestamp => {
                                // TODO: handle gestures
                                continue;
                            }
                            _ => {
                                // event_pump doesn't have a method to peek on
                                // events, so we keep track of an unconsumed
                                // one from a previous loop iteration
                                assert!(previous_event.is_none());
                                previous_event = Some(next);
                                break;
                            }
                        }
                    }
                    log_dbg!("Finishing multi-touch for {:?} with {:?}", event, map);
                    match event {
                        E::FingerUp { .. } => Event::TouchesUp(map),
                        E::FingerMotion { .. } => Event::TouchesMove(map),
                        E::FingerDown { .. } => Event::TouchesDown(map),
                        _ => unreachable!(),
                    }
                }
                E::KeyDown {
                    keycode: Some(sdl2::keyboard::Keycode::F12),
                    ..
                } => {
                    // Log this so you can tell when touchHLE has received
                    // the event but it's stuck in the queue.
                    echo!("F12 pressed, EnterDebugger event queued.");
                    Event::EnterDebugger
                }
                E::KeyDown {
                    keycode: Some(sdl2::keyboard::Keycode::Backspace),
                    ..
                } => {
                    log_dbg!("SDL TextInput Backspace");
                    Event::TextInput(TextInputEvent::Backspace)
                }
                E::KeyDown {
                    keycode: Some(sdl2::keyboard::Keycode::Return),
                    ..
                } => {
                    log_dbg!("SDL TextInput Return");
                    Event::TextInput(TextInputEvent::Return)
                }
                E::TextInput { text, .. } => {
                    log_dbg!("SDL TextInput {}", text);
                    Event::TextInput(TextInputEvent::Text(text))
                }
                _ => continue,
            })
        }

        if controller_updated {
            let (new_x, new_y, pressed, pressed_changed, moved) =
                self.update_virtual_cursor(options);
            self.event_queue
                .push_back(match (pressed, pressed_changed, moved) {
                    (true, true, _) => {
                        let coords = transform_input_coords(self, (new_x, new_y), false);
                        Event::TouchesDown(HashMap::from([(FingerId::VirtualCursor, coords)]))
                    }
                    (false, true, _) => {
                        let coords = transform_input_coords(self, (new_x, new_y), false);
                        Event::TouchesUp(HashMap::from([(FingerId::VirtualCursor, coords)]))
                    }
                    (true, _, true) => {
                        let coords = transform_input_coords(self, (new_x, new_y), false);
                        Event::TouchesMove(HashMap::from([(FingerId::VirtualCursor, coords)]))
                    }
                    _ => return,
                });
        }
    }

    /// Pop an event from the queue (in FIFO order, except for high priority
    /// events)
    pub fn pop_event(&mut self) -> Option<Event> {
        self.high_priority_event
            .take()
            .or_else(|| self.event_queue.pop_front())
    }

    fn controller_added(&mut self, joystick_idx: u32) {
        let Ok(controller) = self.controller_ctx.open(joystick_idx) else {
            log!("Warning: A new controller was connected, but it couldn't be accessed!");
            return;
        };

        let controller_name = controller.name();
        if env::consts::OS == "android" && controller_name.starts_with("uinput-") {
            log!("ignoring fingerprint device: {}", controller_name);
            return;
        }
        log!(
            "New controller connected: {}. Left stick = device tilt. Right stick = touch input (press the stick or shoulder button to tap/hold).",
            controller_name
        );
        self.controllers.push(controller);
    }
    fn controller_removed(&mut self, instance_id: u32) {
        let Some(idx) = self
            .controllers
            .iter()
            .position(|controller| controller.instance_id() == instance_id)
        else {
            return;
        };
        let controller = self.controllers.remove(idx);
        log!("Warning: Controller disconnected: {}", controller.name());
    }
    pub fn print_accelerometer_notice(&self, options: &Options) {
        log!("This app uses the accelerometer.");

        if !self.controllers.is_empty() && options.analog_stick_tilt_controls {
            log!("Your connected controller's left analog stick will be used for accelerometer simulation.");
            if self.accelerometer.is_some() {
                log!("Disconnect the controller if you want to use your device's accelerometer.");
            }
        } else if self.accelerometer.is_some() {
            log!("Your device's accelerometer will be used for accelerometer simulation.");
            if options.analog_stick_tilt_controls {
                log!("Connect a controller if you would prefer to use an analog stick.");
            }
        } else if self.controllers.is_empty() && options.analog_stick_tilt_controls {
            log!("Connect a controller to get accelerometer simulation.");
        }

        if self.accelerometer.is_none() {
            log!(
                "You can {}hold right click and move the cursor to simulate the accelerometer.",
                if options.analog_stick_tilt_controls {
                    "also "
                } else {
                    ""
                }
            );
        }
    }

    /// Get the real or simulated accelerometer output.
    /// See also [crate::frameworks::uikit::ui_accelerometer].
    pub fn get_acceleration(&self, options: &Options) -> (f32, f32, f32) {
        if self.controllers.is_empty() || !options.analog_stick_tilt_controls {
            if let Some(ref accelerometer) = self.accelerometer {
                let data = accelerometer.get_data().unwrap();
                let sdl2::sensor::SensorData::Accel(data) = data else {
                    panic!();
                };
                let [x, y, z] = data;
                // UIAcceleration reports acceleration towards gravity, but SDL2
                // reports acceleration away from gravity.
                let (x, y, z) = (-x, -y, -z);
                // UIAcceleration reports acceleration in units of g-force, but
                // SDL2 reports acceleration in units of m/s^2.
                let gravity: f32 = 9.80665; // SDL_STANDARD_GRAVITY
                let (x, y, z) = (x / gravity, y / gravity, z / gravity);
                return (x, y, z);
            }
        }

        let (x, y) = if self
            .virtual_accelerometer_last
            .is_some_and(|(_x, _y, right_click_hold)| right_click_hold)
        {
            self.virtual_accelerometer_last
                .map(|(x, y, _right_click_hold)| (x, y))
                .unwrap()
        } else {
            // Get left analog stick input. The range is [-1, 1] on each axis.
            let (x, y, _) = self.get_controller_stick(options, true);
            (x, y)
        };

        // Correct for window rotation
        let [x, y] = self.rotation_matrix().inverse().unwrap().transform([x, y]);
        let (x, y) = (x.clamp(-1.0, 1.0), y.clamp(-1.0, 1.0)); // just in case

        // Let's simulate tilting the device based on the analog stick inputs.
        //
        // If an iPhone is lying flat on its back, level with the ground, and it
        // is on Earth, the accelerometer will report approximately (0, 0, -1).
        // The acceleration x and y axes are aligned with the screen's x and y
        // axes. +x points to the right of the screen, +y points to the top of
        // the screen, and +z points away from the screen. In the example
        // scenario, the z axis is parallel to gravity.

        let gravity: [f32; 3] = [0.0, 0.0, -1.0];

        let neutral_x = options.x_tilt_offset.to_radians();
        let neutral_y = options.y_tilt_offset.to_radians();
        let x_rotation_range = options.x_tilt_range.to_radians() / 2.0;
        let y_rotation_range = options.y_tilt_range.to_radians() / 2.0;
        // (x, y) are swapped because the controller Y axis usually corresponds
        // to forward/backward movement, but rotating about the Y axis means
        // tilting the device left/right.
        let x_rotation = neutral_x - x_rotation_range * y;
        let y_rotation = neutral_y - y_rotation_range * x;
        let matrix =
            Matrix::<3>::y_rotation(y_rotation).multiply(&Matrix::<3>::x_rotation(x_rotation));
        let [x, y, z] = matrix.transform(gravity);

        (x, y, z)
    }

    /// For use when redrawing the screen: Get the cached on-screen position and
    /// press state of the analog stick-controlled virtual cursor, if it is
    /// visible.
    pub fn virtual_cursor_visible_at(&self) -> Option<(f32, f32, bool)> {
        let (x, y, pressed, visible) = self.virtual_cursor_last?;
        if visible {
            // When stickyness is in use, the visual cursor movement appears
            // uncomfortably choppy. Showing the un-sticky position is a bit
            // misleading but it *feels* better, and it is documented.
            if let Some((x_unsticky, y_unsticky, _time)) = self.virtual_cursor_last_unsticky {
                Some((x_unsticky, y_unsticky, pressed))
            } else {
                Some((x, y, pressed))
            }
        } else {
            None
        }
    }

    /// Update the virtual cursor's position, click state and visibility, then
    /// return the new position, pressed state, whether the press state changed
    /// and whether the cursor moved.
    fn update_virtual_cursor(&mut self, options: &Options) -> (f32, f32, bool, bool, bool) {
        // Get right analog stick input. The range is [-1, 1] on each axis.
        let (x, y, pressed) = self.get_controller_stick(options, false);

        // The cursor is intended to only show up once you move the analog stick
        // out of its deadzone, or while the button is held.
        let visible = pressed || x != 0.0 || y != 0.0;

        // Though the analog stick output fits within a square, its actual range
        // is usually a circle enclosed by the square. So we need to cut out the
        // rectangular shape of the screen from that circle within the square.
        let (vx, vy, vw, vh) = self.viewport();
        let (vx, vy, vw, vh) = (vx as f32, vy as f32, vw as f32, vh as f32);

        let (x, y) = {
            // Use Pythagoras's theorem to find the largest size the rectangle
            // can have within the circle.
            let ratio = vw / vh;
            let rect_height = (ratio * ratio + 1.0).powf(-0.5);
            let rect_width = ratio * rect_height;

            let x_abs = x.abs().min(rect_width) / rect_width;
            let y_abs = y.abs().min(rect_height) / rect_height;
            (x_abs.copysign(x), y_abs.copysign(y))
        };

        // Convert to on-screen window co-ordinates
        let x = (x / 2.0 + 0.5) * vw + vx;
        let y = (y / 2.0 + 0.5) * vh + vy;

        let (old_x, old_y, old_pressed, _old_visible) =
            self.virtual_cursor_last.unwrap_or_default();

        let (x, y) = if let Some((smoothing_strength, sticky_radius)) =
            options.stabilize_virtual_cursor
        {
            let new_time = Instant::now();

            let (old_x_unsticky, old_y_unsticky, old_time) = self
                .virtual_cursor_last_unsticky
                .unwrap_or((0.0, 0.0, new_time));

            let delta_t = new_time.saturating_duration_since(old_time).as_secs_f32();

            // Apply a feedback-based smoothing with exponential decay, to try
            // to dampen shakiness in the stick movement.

            let smooth = |old: f32, new: f32| -> f32 {
                if smoothing_strength != 0.0 {
                    let lerp_factor = 1.0 - (0.5_f32).powf(delta_t * (1.0 / smoothing_strength));
                    old + (new - old) * lerp_factor
                } else {
                    new
                }
            };

            let new_x_unsticky = smooth(old_x_unsticky, x);
            let new_y_unsticky = smooth(old_y_unsticky, y);

            self.virtual_cursor_last_unsticky = Some((new_x_unsticky, new_y_unsticky, new_time));

            // Make the reported position "sticky" within a certain radius, i.e.
            // if the new position's distance from the old one is within the
            // radius, report no change in position.

            if (new_x_unsticky - old_x).hypot(new_y_unsticky - old_y) < sticky_radius {
                (old_x, old_y)
            } else {
                (new_x_unsticky, new_y_unsticky)
            }
        } else {
            (x, y)
        };

        self.virtual_cursor_last = Some((x, y, pressed, visible));

        (
            x,
            y,
            pressed,
            pressed != old_pressed,
            x != old_x || y != old_y,
        )
    }

    /// Get the summed X and Y positions and button state of the left or right
    /// analog stick of the game controllers. Each axis value is in the range
    /// [-1, 1].
    fn get_controller_stick(&self, options: &Options, left: bool) -> (f32, f32, bool) {
        fn convert_axis(axis: i16, deadzone: f32) -> f32 {
            assert!(deadzone >= 0.0);
            let axis = ((axis as f32) / (i16::MAX as f32)).clamp(-1.0, 1.0);
            let abs_axis = (axis.abs().max(deadzone) - deadzone) / (1.0 - deadzone);
            abs_axis.copysign(axis)
        }

        let (mut x, mut y) = (0.0, 0.0);
        let mut pressed = false;
        for controller in &self.controllers {
            use sdl2::controller::{Axis, Button};
            let (x_axis, y_axis, button1, button2) = if left {
                (
                    Axis::LeftX,
                    Axis::LeftY,
                    Button::LeftStick,
                    Button::LeftShoulder,
                )
            } else {
                (
                    Axis::RightX,
                    Axis::RightY,
                    Button::RightStick,
                    Button::RightShoulder,
                )
            };
            x += convert_axis(controller.axis(x_axis), options.deadzone);
            y += convert_axis(controller.axis(y_axis), options.deadzone);
            pressed |= controller.button(button1);
            pressed |= controller.button(button2);
        }
        let (x, y) = (x.clamp(-1.0, 1.0), y.clamp(-1.0, 1.0));

        (x, y, pressed)
    }

    pub fn create_gl_context(&self, version: GLVersion) -> Result<GLContext, String> {
        let attr = self.video_ctx.gl_attr();
        match version {
            GLVersion::GLES11 => {
                attr.set_context_version(1, 1);
                attr.set_context_profile(sdl2::video::GLProfile::GLES);
            }
            GLVersion::GL21Compat => {
                attr.set_context_version(2, 1);
                attr.set_context_profile(sdl2::video::GLProfile::Compatibility);
            }
        }

        let gl_ctx = self.window.gl_create_context()?;

        Ok(GLContext(gl_ctx))
    }

    pub fn gl_get_proc_address(&self, procname: &str) -> *const std::ffi::c_void {
        // For some reason, rust-sdl2 uses *const (), but () is not meant to be
        // used for void pointees (just void results), so let's fix that.
        self.video_ctx.gl_get_proc_address(procname) as *const _
    }

    pub fn set_share_with_current_context(&self, value: bool) {
        self.video_ctx
            .gl_attr()
            .set_share_with_current_context(value)
    }

    pub unsafe fn make_gl_context_current(&self, gl_ctx: &GLContext) {
        self.window.gl_make_current(&gl_ctx.0).unwrap();
    }

    /// Make the internal OpenGL ES context (for splash screen and UI rendering)
    /// current.
    #[must_use]
    pub fn make_internal_gl_ctx_current<'win>(&'win mut self) -> Box<dyn GLES + 'win> {
        // The invariant is held up here - since the instance we return is
        // bound to the lifetime of window, it can't outlive the internal GL
        // context and can't outlive the window.
        let gl_ins = unsafe {
            self.internal_gl_ins
                .as_mut()
                .unwrap()
                .make_current_unchecked_for_window(
                    &mut |gl_ctx| self.window.gl_make_current(&gl_ctx.0).unwrap(),
                    &mut |s| self.video_ctx.gl_get_proc_address(s) as *const _,
                )
        };
        gl_ins
    }

    fn display_splash(&mut self) {
        assert!(self.splash_image.is_some());

        // OpenGL ES expects bottom-to-top row order for image data, but our
        // image data will be top-to-bottom. A reflection transform compensates.
        let matrix = self.rotation_matrix().multiply(&Matrix::y_flip());
        let (vx, vy, vw, vh) = self.viewport();
        let viewport = (vx, vy + self.viewport_y_offset(), vw, vh);

        let image = self.splash_image.as_ref().unwrap();

        unsafe {
            let mut gl_ctx = self
                .internal_gl_ins
                .as_mut()
                .unwrap()
                .make_current_unchecked_for_window(
                    &mut |gl_ctx| self.window.gl_make_current(&gl_ctx.0).unwrap(),
                    &mut |s| self.video_ctx.gl_get_proc_address(s) as *const _,
                );

            use crate::gles::gles11_raw as gles11; // constants only

            let mut texture = 0;
            gl_ctx.GenTextures(1, &mut texture);
            gl_ctx.BindTexture(gles11::TEXTURE_2D, texture);
            let (width, height) = image.dimensions();
            gl_ctx.TexImage2D(
                gles11::TEXTURE_2D,
                0,
                gles11::RGBA as _,
                width as _,
                height as _,
                0,
                gles11::RGBA,
                gles11::UNSIGNED_BYTE,
                image.pixels().as_ptr() as *const _,
            );
            gl_ctx.TexParameteri(
                gles11::TEXTURE_2D,
                gles11::TEXTURE_MIN_FILTER,
                gles11::LINEAR as _,
            );
            gl_ctx.TexParameteri(
                gles11::TEXTURE_2D,
                gles11::TEXTURE_MAG_FILTER,
                gles11::LINEAR as _,
            );

            present_frame(
                gl_ctx.as_mut(),
                viewport,
                matrix,
                /* virtual_cursor_visible_at: */ None,
            );

            gl_ctx.DeleteTextures(1, &texture);
        };

        self.window.gl_swap_window();

        // hold onto GL context so the image doesn't disappear, and hold
        // onto image so we can rotate later if necessary
    }

    /// Swap front-buffer and back-buffer so the result of OpenGL rendering is
    /// presented.
    pub fn set_guest_launch_started_at(&mut self, started_at: Instant) {
        self.guest_launch_started_at = Some(started_at);
    }

    pub fn swap_window(&mut self) {
        self.window.gl_swap_window();
        if let Some(started_at) = self.guest_launch_started_at.take() {
            log!(
                "First guest frame presented {} ms after app selection",
                started_at.elapsed().as_millis()
            );
        }
    }

    /// Consider the emulated device to be rotated to a particular orientation.
    ///
    /// On a PC or laptop, this will make the window be rotated so the app
    /// content appears upright. On a mobile device, this might do something
    /// else, because the user can physically rotate the screen.
    pub fn rotate_device(&mut self, new_orientation: DeviceOrientation) {
        assert!(self.on_main_stack);
        if new_orientation == self.device_orientation {
            return;
        }

        if !self.fullscreen && !Self::rotatable_fullscreen() {
            let (width, height) = if Self::rotatable_fullscreen() {
                set_sdl2_orientation(new_orientation);
                rotate_fullscreen_size(new_orientation, self.window.size())
            } else {
                size_for_orientation(self.device_family, new_orientation, self.scale_hack)
            };

            // macOS quirk: when resizing the window, the new framebuffer's size
            // is apparently max(new_size, old_size) in each dimension, but the
            // viewport is positioned wrong on the y axis for some reason, so we
            // need to apply an offset.
            // Recreating the OpenGL context was an alternative workaround, but
            // that apparently stops other OpenGL contexts drawing to the
            // framebuffer!
            #[cfg(target_os = "macos")]
            {
                let (_old_width, old_height) = self.window.size();
                self.max_height = self.max_height.max(old_height).max(height);
                self.viewport_y_offset = self.max_height - height;
            }

            self.window.set_size(width, height).unwrap();
        }

        if Self::rotatable_fullscreen() {
            set_sdl2_orientation(new_orientation);
            // Hack: from reading SDL2's source code, it seems that SDL2 will
            // only re-do the orientation when changing whether a window is
            // "resizeable" (can be rotated). You can't set the resizeable state
            // on a fullscreen window, so it must be temporarily stop being
            // fulscreen.
            // Apparently, doing this does result in resizing the window.
            self.window
                .set_fullscreen(sdl2::video::FullscreenType::Off)
                .unwrap();
            unsafe {
                let window_raw = self.window.raw();
                sdl2_sys::SDL_SetWindowResizable(window_raw, sdl2_sys::SDL_bool::SDL_FALSE);
                sdl2_sys::SDL_SetWindowResizable(window_raw, sdl2_sys::SDL_bool::SDL_TRUE);
            }
            self.window
                .set_fullscreen(sdl2::video::FullscreenType::True)
                .unwrap();
        }

        self.device_orientation = new_orientation;

        if self.splash_image.is_some() {
            self.display_splash();
        }
    }

    pub fn device_family(&self) -> DeviceFamily {
        self.device_family
    }

    /// Returns the current device orientation
    pub fn current_rotation(&self) -> DeviceOrientation {
        self.device_orientation
    }

    /// Get the size in pixels of the window without rotation or scaling.
    ///
    /// The aspect ratio, scale and orientation reflect the guest app's view of
    /// the world.
    pub fn size_unrotated_unscaled(&self) -> (u32, u32) {
        size_for_orientation(
            self.device_family,
            if self.picker_auto_rotate {
                self.device_orientation
            } else {
                DeviceOrientation::Portrait
            },
            NonZeroU32::new(1).unwrap(),
        )
    }

    pub fn is_picker_auto_rotating(&self) -> bool {
        self.picker_auto_rotate
    }

    pub fn logical_screen_size(&self, orientation: DeviceOrientation) -> (u32, u32) {
        size_for_orientation(self.device_family, orientation, NonZeroU32::new(1).unwrap())
    }

    /// Get the region of the on-screen window (x, y, width, height) used to
    /// display the app content.
    ///
    /// The aspect ratio of this region always reflects the guest app's view of
    /// the world, but the scale and orientation might not.
    pub fn viewport(&self) -> (u32, u32, u32, u32) {
        let (app_width, app_height) =
            size_for_orientation(self.device_family, self.device_orientation, self.scale_hack);
        if !self.fullscreen && !Self::rotatable_fullscreen() {
            return (0, 0, app_width, app_height);
        }

        let (screen_width, screen_height) = self.window.drawable_size();

        let app_aspect = app_width as f32 / app_height as f32;
        let screen_aspect = screen_width as f32 / screen_height as f32;
        let (scaled_width, scaled_height) = if app_aspect < screen_aspect {
            (
                (screen_height as f32 * app_aspect).round() as u32,
                screen_height,
            )
        } else {
            (
                screen_width,
                (screen_width as f32 / app_aspect).round() as u32,
            )
        };
        let x = (screen_width - scaled_width) / 2;
        let y = (screen_height - scaled_height) / 2;
        (x, y, scaled_width, scaled_height)
    }

    /// Special offset to add to y co-ordinates, only when drawing to screen.
    pub fn viewport_y_offset(&self) -> u32 {
        #[cfg(target_os = "macos")]
        return self.viewport_y_offset;
        #[cfg(not(target_os = "macos"))]
        return 0;
    }

    /// Transformation matrix for transforming between the window's co-ordinate
    /// space and the app's original co-ordinate space when rotation is in use
    /// (see [Self::rotate_device]). This returns a matrix appropriate for
    /// rotating texture co-ordinates to display the image in the window; when
    /// rotating input co-ordinates, invert the matrix.
    pub fn rotation_matrix(&self) -> Matrix<2> {
        if self.picker_auto_rotate {
            // The host picker reflows upright views instead of rotating a guest app's canvas.
            return Matrix::identity();
        }
        match self.device_orientation {
            DeviceOrientation::Portrait => Matrix::identity(),
            DeviceOrientation::PortraitUpsideDown => Matrix::z_rotation(PI),
            DeviceOrientation::LandscapeLeft => Matrix::z_rotation(-FRAC_PI_2),
            DeviceOrientation::LandscapeRight => Matrix::z_rotation(FRAC_PI_2),
        }
    }

    pub fn is_screen_saver_enabled(&self) -> bool {
        self.video_ctx.is_screen_saver_enabled()
    }
    pub fn set_screen_saver_enabled(&mut self, enabled: bool) {
        assert!(self.on_main_stack);
        match enabled {
            true => self.video_ctx.enable_screen_saver(),
            false => self.video_ctx.disable_screen_saver(),
        }
    }

    pub fn start_text_input(&self) {
        assert!(self.on_main_stack);
        unsafe {
            sdl2_sys::SDL_StartTextInput();
        }
    }
    pub fn stop_text_input(&self) {
        assert!(self.on_main_stack);
        unsafe {
            sdl2_sys::SDL_StopTextInput();
        }
    }

    pub fn on_main_stack(&self) -> bool {
        self.on_main_stack
    }
}

pub fn open_url(env: &mut Environment, url: &str) -> Result<(), String> {
    env.on_parent_stack_in_coroutine(|_, _| sdl2::url::open_url(url).map_err(|e| e.to_string()))
}

/// Show an SDL messagebox for an error (typically after a panic).
///
/// The window argument allows for passing in the parent window for the
/// messagebox, which is not required but should be done if possible.
pub fn show_error_messagebox(window: Option<&Window>, error_message: &str) {
    assert!(window.is_none_or(|win| win.on_main_stack));
    use sdl2::messagebox;
    let mbox = [
        messagebox::ButtonData {
            flags: messagebox::MessageBoxButtonFlag::NOTHING,
            button_id: 0,
            text: "Open touchHLE directory",
        },
        messagebox::ButtonData {
            flags: messagebox::MessageBoxButtonFlag::NOTHING,
            button_id: 1,
            text: "Close",
        },
    ];

    let Ok(clicked_button) = messagebox::show_message_box(
        messagebox::MessageBoxFlag::ERROR,
        &mbox,
        "touchHLE crashed!",
        &format!("touchHLE crashed with the following error: {error_message}"),
        window.map(|win| &win.window),
        None,
    ) else {
        panic!("Failed to show message box!");
    };

    match clicked_button {
        messagebox::ClickedButton::CloseButton => {}
        messagebox::ClickedButton::CustomButton(button) => {
            match button.button_id {
                // Open data directory (contains log file on android)
                0 => match crate::paths::url_for_opening_user_data_dir() {
                    Ok(url) => {
                        if let Err(e) = sdl2::url::open_url(&url).map_err(|e| e.to_string()) {
                            echo!("Couldn't open file manager at {:?}: {}", url, e);
                        } else {
                            echo!("Opened file manager at {:?}, exiting.", url);
                        }
                    }
                    Err(e) => echo!("Couldn't open file manager: {}", e),
                },
                // Close
                1 => {}
                _ => unreachable!(),
            }
        }
    }
}

#[cfg(not(target_os = "android"))]
pub fn ask_sync_offline_choice(reason: &str) -> Result<bool, String> {
    use sdl2::messagebox;
    let buttons = [
        messagebox::ButtonData {
            flags: messagebox::MessageBoxButtonFlag::NOTHING,
            button_id: 0,
            text: "Retry",
        },
        messagebox::ButtonData {
            flags: messagebox::MessageBoxButtonFlag::NOTHING,
            button_id: 1,
            text: "Continue offline",
        },
    ];
    match messagebox::show_message_box(
        messagebox::MessageBoxFlag::WARNING,
        &buttons,
        "Google Drive sync unavailable",
        &format!("{reason}\n\nRetry the sync, or continue offline for this session?"),
        None,
        None,
    )
    .map_err(|error| error.to_string())?
    {
        messagebox::ClickedButton::CustomButton(button) => match button.button_id {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err("No Google Drive sync action was selected".into()),
        },
        messagebox::ClickedButton::CloseButton => {
            Err("No Google Drive sync action was selected".into())
        }
    }
}

#[cfg(not(target_os = "android"))]
pub fn ask_sync_shutdown_retry(reason: &str) -> Result<bool, String> {
    use sdl2::messagebox;
    let buttons = [
        messagebox::ButtonData {
            flags: messagebox::MessageBoxButtonFlag::NOTHING,
            button_id: 0,
            text: "Retry",
        },
        messagebox::ButtonData {
            flags: messagebox::MessageBoxButtonFlag::NOTHING,
            button_id: 1,
            text: "Exit and keep local data",
        },
    ];
    match messagebox::show_message_box(
        messagebox::MessageBoxFlag::WARNING,
        &buttons,
        "Final Google Drive sync did not finish",
        &format!("{reason}\n\nRetry, or exit with your local data preserved?"),
        None,
        None,
    )
    .map_err(|error| error.to_string())?
    {
        messagebox::ClickedButton::CustomButton(button) => match button.button_id {
            0 => Ok(true),
            1 => Ok(false),
            _ => Err("No shutdown sync action was selected".into()),
        },
        messagebox::ClickedButton::CloseButton => Ok(false),
    }
}

#[cfg(target_os = "android")]
pub fn run_with_sync_progress<T: Send>(
    _title: &str,
    operation: impl FnOnce(&mut dyn FnMut(crate::sync::progress::SyncProgress)) -> T + Send,
) -> T {
    use crate::sync::progress::{SyncProgress, SyncProgressDisplay};

    let initial = SyncProgress::Scanning { files: 0, bytes: 0 }.display();
    crate::sync::auth::android::update_sync_progress(Some(initial));
    let mut last_update = Instant::now();
    let mut last_headline = String::new();
    let mut report = |progress: crate::sync::progress::SyncProgress| {
        let display = progress.display();
        if display.headline != last_headline || last_update.elapsed() >= Duration::from_millis(100)
        {
            crate::sync::auth::android::update_sync_progress(Some(display.clone()));
            last_update = Instant::now();
            last_headline = display.headline.to_owned();
        }
    };
    let result = operation(&mut report);
    crate::sync::auth::android::update_sync_progress(None::<SyncProgressDisplay>);
    result
}

#[cfg(not(target_os = "android"))]
pub fn run_with_sync_progress<T: Send>(
    title: &str,
    operation: impl FnOnce(&mut dyn FnMut(crate::sync::progress::SyncProgress)) -> T + Send,
) -> T {
    use std::sync::mpsc;

    let mut window = match SyncProgressWindow::new(title) {
        Ok(window) => window,
        Err(error) => {
            log!("Could not show sync progress window: {error}");
            let mut ignore = |_| {};
            return operation(&mut ignore);
        }
    };
    window.canvas.window_mut().show();
    window.canvas.window_mut().raise();
    window.draw();
    log!("Sync progress window shown and raised before the operation started");
    let (sender, receiver) = mpsc::channel();
    std::thread::scope(|scope| {
        let worker = scope.spawn(move || {
            let mut report = |progress| {
                let _ = sender.send(progress);
            };
            operation(&mut report)
        });
        let mut restored_focus_after_progress = false;
        while !worker.is_finished() {
            window.poll_events();
            while let Ok(progress) = receiver.try_recv() {
                window.set_progress(progress);
                if !restored_focus_after_progress {
                    window.canvas.window_mut().raise();
                    log!("Sync progress window re-raised after the first sync progress update");
                    restored_focus_after_progress = true;
                }
            }
            window.draw();
            std::thread::sleep(Duration::from_millis(40));
        }
        while let Ok(progress) = receiver.try_recv() {
            window.set_progress(progress);
        }
        window.draw();
        worker.join().expect("cloud sync worker panicked")
    })
}

#[cfg(not(target_os = "android"))]
struct SyncProgressWindow {
    _sdl: sdl2::Sdl,
    canvas: sdl2::render::Canvas<sdl2::video::Window>,
    event_pump: sdl2::EventPump,
    font: rusttype::Font<'static>,
    scale_x: f32,
    scale_y: f32,
    progress: crate::sync::progress::SyncProgress,
    animation_frame: usize,
}

#[cfg(not(target_os = "android"))]
impl SyncProgressWindow {
    fn new(title: &str) -> Result<Self, String> {
        use std::io::Read;

        let sdl = sdl2::init()?;
        let video = sdl.video()?;
        let window = video
            .window(title, 620, 310)
            .position_centered()
            .allow_highdpi()
            .build()
            .map_err(|error| error.to_string())?;
        let canvas = window
            .into_canvas()
            .build()
            .map_err(|error| error.to_string())?;
        let (output_width, output_height) = canvas.output_size()?;
        let scale_x = output_width as f32 / 620.0;
        let scale_y = output_height as f32 / 310.0;
        let mut font_bytes = Vec::new();
        crate::paths::ResourceFile::open(&format!(
            "{}/LiberationSans-Regular.ttf",
            crate::paths::FONTS_DIR
        ))?
        .get()
        .read_to_end(&mut font_bytes)
        .map_err(|error| error.to_string())?;
        let font = rusttype::Font::try_from_vec(font_bytes)
            .ok_or_else(|| "bundled sync progress font could not be loaded".to_owned())?;
        let event_pump = sdl.event_pump()?;
        Ok(Self {
            _sdl: sdl,
            canvas,
            event_pump,
            font,
            scale_x,
            scale_y,
            progress: crate::sync::progress::SyncProgress::Scanning { files: 0, bytes: 0 },
            animation_frame: 0,
        })
    }

    fn set_progress(&mut self, progress: crate::sync::progress::SyncProgress) {
        self.progress = progress;
    }

    fn poll_events(&mut self) {
        for event in self.event_pump.poll_iter() {
            if matches!(event, sdl2::event::Event::Quit { .. }) {
                // Closing this window must not silently bypass the launch gate.
            }
        }
    }

    fn draw(&mut self) {
        use sdl2::pixels::Color;

        let background = Color::RGB(246, 246, 242);
        let foreground = Color::RGB(31, 42, 46);
        let muted = Color::RGB(93, 107, 108);
        let accent = Color::RGB(28, 111, 104);
        let track = Color::RGB(218, 225, 221);
        self.canvas.set_draw_color(background);
        self.canvas.clear();
        self.canvas.set_blend_mode(sdl2::render::BlendMode::Blend);

        let display = self.progress.display();
        self.draw_text("Google Drive Sync", 42, 34, 25.0, foreground);
        self.draw_text(display.headline, 42, 99, 19.0, foreground);
        self.draw_text(&display.detail, 42, 132, 14.0, muted);
        self.draw_text(
            "Local scan",
            42,
            190,
            12.0,
            if display.active_step == 0 {
                accent
            } else {
                muted
            },
        );
        self.draw_text(
            "Drive check",
            174,
            190,
            12.0,
            if display.active_step >= 1 {
                accent
            } else {
                muted
            },
        );
        self.draw_text(
            "Transfer",
            318,
            190,
            12.0,
            if display.active_step >= 2 {
                accent
            } else {
                muted
            },
        );
        self.draw_text(
            "Apply",
            468,
            190,
            12.0,
            if display.active_step >= 3 {
                accent
            } else {
                muted
            },
        );

        let bar_y = 218;
        self.fill_logical_rect(42, bar_y, 526, 5, track);
        self.fill_logical_rect(42, bar_y, display.track_progress.into(), 5, accent);
        let pulse_x = 42 + (self.animation_frame % 28) as i32 * 18;
        self.fill_logical_rect(pulse_x, 246, 18, 3, Color::RGB(255, 255, 255));
        self.fill_logical_rect(42, 246, 504, 1, muted);
        self.draw_text(
            "Please keep touchHLE open until sync finishes.",
            42,
            263,
            12.0,
            muted,
        );
        self.canvas.present();
        self.animation_frame = self.animation_frame.wrapping_add(1);
    }

    fn draw_text(&mut self, text: &str, x: i32, y: i32, size: f32, color: sdl2::pixels::Color) {
        use rusttype::{point, Scale};
        use sdl2::rect::Rect;

        let scale = Scale::uniform(size * self.scale_y);
        let baseline = y as f32 * self.scale_y + self.font.v_metrics(scale).ascent;
        let glyphs: Vec<_> = self
            .font
            .layout(text, scale, point(x as f32 * self.scale_x, baseline))
            .collect();
        let canvas = &mut self.canvas;
        for glyph in glyphs {
            let Some(bounds) = glyph.pixel_bounding_box() else {
                continue;
            };
            glyph.draw(|gx, gy, coverage| {
                if coverage <= 0.0 {
                    return;
                }
                let alpha = (coverage * 255.0) as u8;
                canvas.set_draw_color(sdl2::pixels::Color::RGBA(color.r, color.g, color.b, alpha));
                let _ = canvas.fill_rect(Rect::new(
                    bounds.min.x + gx as i32,
                    bounds.min.y + gy as i32,
                    1,
                    1,
                ));
            });
        }
    }

    fn fill_logical_rect(
        &mut self,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        color: sdl2::pixels::Color,
    ) {
        use sdl2::rect::Rect;

        let x = (x as f32 * self.scale_x).round() as i32;
        let y = (y as f32 * self.scale_y).round() as i32;
        let width = (width as f32 * self.scale_x).round().max(1.0) as u32;
        let height = (height as f32 * self.scale_y).round().max(1.0) as u32;
        self.canvas.set_draw_color(color);
        let _ = self.canvas.fill_rect(Rect::new(x, y, width, height));
    }
}

#[cfg(not(target_os = "android"))]
pub fn resolve_sync_conflicts(plan: &SyncPlan) -> Result<Option<Vec<ConflictChoice>>, String> {
    if plan.conflicts.is_empty() {
        return Ok(Some(Vec::new()));
    }
    Ok(SyncConflictWindow::new(plan.clone())?.run())
}

#[cfg(not(target_os = "android"))]
struct SyncConflictWindow {
    _sdl: sdl2::Sdl,
    canvas: sdl2::render::Canvas<sdl2::video::Window>,
    event_pump: sdl2::EventPump,
    font: rusttype::Font<'static>,
    scale_x: f32,
    scale_y: f32,
    plan: SyncPlan,
    local_root: String,
    conflict_idx: usize,
    selected: Vec<Option<usize>>,
    cloud_versions: Vec<usize>,
    message: String,
}

#[cfg(not(target_os = "android"))]
impl SyncConflictWindow {
    const WIDTH: u32 = 980;
    const HEIGHT: u32 = 720;

    fn new(plan: SyncPlan) -> Result<Self, String> {
        use std::io::Read;

        let sdl = sdl2::init()?;
        let video = sdl.video()?;
        let window = video
            .window(
                "Resolve Google Drive sync conflicts",
                Self::WIDTH,
                Self::HEIGHT,
            )
            .position_centered()
            .allow_highdpi()
            .build()
            .map_err(|error| error.to_string())?;
        let canvas = window
            .into_canvas()
            .build()
            .map_err(|error| error.to_string())?;
        let (output_width, output_height) = canvas.output_size()?;
        let scale_x = output_width as f32 / Self::WIDTH as f32;
        let scale_y = output_height as f32 / Self::HEIGHT as f32;
        let mut font_bytes = Vec::new();
        crate::paths::ResourceFile::open(&format!(
            "{}/LiberationSans-Regular.ttf",
            crate::paths::FONTS_DIR
        ))?
        .get()
        .read_to_end(&mut font_bytes)
        .map_err(|error| error.to_string())?;
        let font = rusttype::Font::try_from_vec(font_bytes)
            .ok_or_else(|| "bundled conflict resolver font could not be loaded".to_owned())?;
        let event_pump = sdl.event_pump()?;
        let count = plan.conflicts.len();
        let local_root = crate::paths::user_data_base_path()
            .canonicalize()
            .unwrap_or_else(|_| crate::paths::user_data_base_path().to_path_buf())
            .display()
            .to_string();
        Ok(Self {
            _sdl: sdl,
            canvas,
            event_pump,
            font,
            scale_x,
            scale_y,
            plan,
            local_root,
            conflict_idx: 0,
            selected: vec![None; count],
            cloud_versions: vec![0; count],
            message: String::new(),
        })
    }

    fn run(mut self) -> Option<Vec<ConflictChoice>> {
        use sdl2::event::Event;
        use sdl2::keyboard::Keycode;

        self.canvas.window_mut().show();
        self.canvas.window_mut().raise();
        loop {
            self.draw();
            let events: Vec<_> = self.event_pump.poll_iter().collect();
            for event in events {
                match event {
                    Event::Quit { .. } => return None,
                    Event::MouseButtonDown {
                        mouse_btn: MouseButton::Left,
                        x,
                        y,
                        ..
                    } => {
                        if let Some(choices) = self.handle_click(x, y) {
                            return choices;
                        }
                    }
                    Event::KeyDown {
                        keycode: Some(key),
                        repeat: false,
                        ..
                    } => match key {
                        Keycode::Escape => return None,
                        Keycode::Left => self.move_file(-1),
                        Keycode::Right => self.move_file(1),
                        Keycode::Up => self.move_cloud_version(-1),
                        Keycode::Down => self.move_cloud_version(1),
                        Keycode::L => self.select_local(),
                        Keycode::C => self.select_cloud(),
                        Keycode::Return | Keycode::KpEnter => {
                            if let Some(choices) = self.try_apply() {
                                return Some(choices);
                            }
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
            std::thread::sleep(Duration::from_millis(16));
        }
    }

    fn handle_click(&mut self, x: i32, y: i32) -> Option<Option<Vec<ConflictChoice>>> {
        match conflict_action_at(x, y) {
            Some(ConflictWindowAction::SelectLocal) => self.select_local(),
            Some(ConflictWindowAction::SelectCloud) => self.select_cloud(),
            Some(ConflictWindowAction::PreviousCloudVersion) => self.move_cloud_version(-1),
            Some(ConflictWindowAction::NextCloudVersion) => self.move_cloud_version(1),
            Some(ConflictWindowAction::PreviousFile) => self.move_file(-1),
            Some(ConflictWindowAction::NextFile) => self.move_file(1),
            Some(ConflictWindowAction::Apply) => return self.try_apply().map(Some),
            Some(ConflictWindowAction::Cancel) => return Some(None),
            None => {}
        }
        None
    }

    fn select_local(&mut self) {
        self.selected[self.conflict_idx] = Some(0);
        self.message = "Keep Local selected for this file.".to_owned();
    }

    fn select_cloud(&mut self) {
        if self.plan.conflicts[self.conflict_idx]
            .remote_candidates
            .is_empty()
        {
            self.message = "No Cloud version is available for this file.".to_owned();
            return;
        }
        self.selected[self.conflict_idx] = Some(self.cloud_versions[self.conflict_idx] + 1);
        self.message = "Use Cloud selected for this file.".to_owned();
    }

    fn move_file(&mut self, delta: isize) {
        self.conflict_idx = (self.conflict_idx as isize + delta)
            .clamp(0, self.plan.conflicts.len() as isize - 1) as usize;
        if let Some(selected) = self.selected[self.conflict_idx] {
            if selected > 0 {
                self.cloud_versions[self.conflict_idx] = selected - 1;
            }
        }
        self.message.clear();
    }

    fn move_cloud_version(&mut self, delta: isize) {
        let count = self.plan.conflicts[self.conflict_idx]
            .remote_candidates
            .len();
        if count == 0 {
            return;
        }
        self.cloud_versions[self.conflict_idx] = (self.cloud_versions[self.conflict_idx] as isize
            + delta)
            .rem_euclid(count as isize) as usize;
        self.message.clear();
    }

    fn try_apply(&mut self) -> Option<Vec<ConflictChoice>> {
        let missing: Vec<_> = self
            .selected
            .iter()
            .enumerate()
            .filter_map(|(index, selected)| selected.is_none().then_some(index + 1))
            .collect();
        if !missing.is_empty() {
            self.message = format!(
                "Choose Keep Local or Use Cloud for conflict{} {}.",
                if missing.len() == 1 { "" } else { "s" },
                missing
                    .iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            return None;
        }
        let choices: Vec<_> = self
            .plan
            .conflicts
            .iter()
            .zip(&self.selected)
            .map(|(conflict, selected)| {
                let selected = selected.expect("missing choices were checked");
                if selected == 0 {
                    ConflictChoice {
                        path: conflict.path.clone(),
                        selected: LocalOrRemote::Local,
                        remote_version_id: None,
                    }
                } else {
                    ConflictChoice {
                        path: conflict.path.clone(),
                        selected: LocalOrRemote::Remote,
                        remote_version_id: Some(
                            conflict.remote_candidates[selected - 1].id.clone(),
                        ),
                    }
                }
            })
            .collect();
        match self.plan.resolved_snapshot(&choices) {
            Ok(_) => Some(choices),
            Err(crate::sync::model::SyncError::UnresolvedConflicts(reason)) => {
                self.message = format!(
                    "These choices conflict: {reason}. Change a choice for one of these paths."
                );
                None
            }
            Err(error) => {
                self.message = format!("Could not apply these choices: {error}");
                None
            }
        }
    }

    fn draw(&mut self) {
        use sdl2::pixels::Color;
        let background = Color::RGB(246, 246, 242);
        let foreground = Color::RGB(31, 42, 46);
        let muted = Color::RGB(93, 107, 108);
        let accent = Color::RGB(28, 111, 104);
        let panel = Color::RGB(255, 255, 255);
        let line = Color::RGB(218, 225, 221);
        let selected_bg = Color::RGB(225, 241, 236);
        self.canvas.set_draw_color(background);
        self.canvas.clear();
        self.canvas.set_blend_mode(sdl2::render::BlendMode::Blend);

        let conflict = self.plan.conflicts[self.conflict_idx].clone();
        let selected_count = self
            .selected
            .iter()
            .filter(|choice| choice.is_some())
            .count();
        self.draw_text(
            "Resolve Google Drive sync conflicts",
            40,
            28,
            27.0,
            foreground,
        );
        self.draw_text(
            &format!(
                "File {} of {}     {} of {} choices made",
                self.conflict_idx + 1,
                self.plan.conflicts.len(),
                selected_count,
                self.plan.conflicts.len()
            ),
            40,
            70,
            16.0,
            muted,
        );
        self.draw_text(
            &format!("Local files are read from: {}", self.local_root),
            40,
            99,
            12.0,
            muted,
        );

        self.fill_logical_rect(40, 130, 900, 94, panel);
        self.draw_logical_rect(40, 130, 900, 94, line);
        self.draw_text("CONFLICTING FILE", 60, 145, 11.0, muted);
        self.draw_wrapped_text(conflict.path.as_str(), 60, 166, 850, 16.0, foreground, 2);

        let local_selected = self.selected[self.conflict_idx] == Some(0);
        let local_missing = !matches!(conflict.local.as_ref(), Some(SnapshotEntry::File { .. }));
        self.draw_choice_card(
            40,
            244,
            435,
            260,
            "This Mac",
            describe_local_conflict_entry(conflict.local.as_ref()),
            local_selected,
            if local_missing {
                "Keep Local (delete Cloud)"
            } else {
                "Keep Local"
            },
            selected_bg,
            panel,
            line,
            foreground,
            muted,
            accent,
        );
        let cloud_index = self.cloud_versions[self.conflict_idx];
        let cloud = conflict.remote_candidates.get(cloud_index);
        let cloud_selected = self.selected[self.conflict_idx] == Some(cloud_index + 1);
        let cloud_missing = cloud.map_or(true, |candidate| {
            !matches!(candidate.entry.as_ref(), Some(SnapshotEntry::File { .. }))
        });
        self.draw_choice_card(
            505,
            244,
            435,
            260,
            "Google Drive",
            cloud.map_or_else(
                || "No Cloud file is available.".to_owned(),
                |candidate| describe_conflict_candidate(candidate),
            ),
            cloud_selected,
            if cloud_missing {
                "Use Cloud (delete Local)"
            } else {
                "Use Cloud"
            },
            selected_bg,
            panel,
            line,
            foreground,
            muted,
            accent,
        );
        if conflict.remote_candidates.len() > 1 {
            self.draw_button(
                862, 252, 32, 32, "<", false, true, panel, line, foreground, accent,
            );
            self.draw_button(
                900, 252, 32, 32, ">", false, true, panel, line, foreground, accent,
            );
            self.draw_text(
                &format!(
                    "Cloud version {} of {}",
                    cloud_index + 1,
                    conflict.remote_candidates.len()
                ),
                700,
                260,
                13.0,
                muted,
            );
        }

        if !self.message.is_empty() {
            self.fill_logical_rect(40, 524, 900, 60, Color::RGB(255, 242, 220));
            self.draw_wrapped_text(&self.message.clone(), 56, 536, 868, 14.0, foreground, 2);
        }
        self.draw_button(
            40,
            606,
            184,
            48,
            "Previous file",
            false,
            self.conflict_idx > 0,
            panel,
            line,
            foreground,
            accent,
        );
        self.draw_button(
            236,
            606,
            184,
            48,
            "Next file",
            false,
            self.conflict_idx + 1 < self.plan.conflicts.len(),
            panel,
            line,
            foreground,
            accent,
        );
        self.draw_button(
            648,
            660,
            190,
            46,
            &format!("Apply ({selected_count}/{})", self.plan.conflicts.len()),
            false,
            selected_count == self.plan.conflicts.len(),
            panel,
            line,
            foreground,
            accent,
        );
        self.draw_button(
            850, 660, 90, 46, "Cancel", false, true, panel, line, foreground, accent,
        );
        self.canvas.present();
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_choice_card(
        &mut self,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        title: &str,
        detail: String,
        selected: bool,
        button: &str,
        selected_bg: sdl2::pixels::Color,
        panel: sdl2::pixels::Color,
        line: sdl2::pixels::Color,
        foreground: sdl2::pixels::Color,
        muted: sdl2::pixels::Color,
        accent: sdl2::pixels::Color,
    ) {
        let fill = if selected { selected_bg } else { panel };
        self.fill_logical_rect(x, y, width, height, fill);
        self.draw_logical_rect(x, y, width, height, if selected { accent } else { line });
        self.draw_text(title, x + 20, y + 18, 19.0, foreground);
        self.draw_wrapped_text(&detail, x + 20, y + 60, width as i32 - 40, 14.0, muted, 5);
        self.draw_button(
            x + 20,
            y + height as i32 - 72,
            width - 40,
            52,
            button,
            selected,
            true,
            fill,
            line,
            foreground,
            accent,
        );
    }

    fn draw_button(
        &mut self,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        label: &str,
        active: bool,
        enabled: bool,
        background: sdl2::pixels::Color,
        border: sdl2::pixels::Color,
        foreground: sdl2::pixels::Color,
        accent: sdl2::pixels::Color,
    ) {
        use rusttype::Scale;
        let fill = if !enabled {
            sdl2::pixels::Color::RGB(231, 233, 230)
        } else if active {
            accent
        } else {
            background
        };
        self.fill_logical_rect(x, y, width, height, fill);
        self.draw_logical_rect(x, y, width, height, if active { accent } else { border });
        let text_color = if active {
            sdl2::pixels::Color::RGB(255, 255, 255)
        } else if enabled {
            foreground
        } else {
            sdl2::pixels::Color::RGB(135, 143, 140)
        };
        let scale = Scale::uniform(15.0 * self.scale_y);
        let text_width: f32 = label
            .chars()
            .map(|character| {
                self.font
                    .glyph(character)
                    .scaled(scale)
                    .h_metrics()
                    .advance_width
            })
            .sum();
        let text_x = x as f32 + (width as f32 - text_width / self.scale_x) / 2.0;
        let text_y = y as f32 + (height as f32 - 18.0) / 2.0;
        self.draw_text(
            label,
            text_x.round() as i32,
            text_y.round() as i32,
            15.0,
            text_color,
        );
    }

    fn draw_text(&mut self, text: &str, x: i32, y: i32, size: f32, color: sdl2::pixels::Color) {
        use rusttype::{point, Scale};
        use sdl2::rect::Rect;

        let scale = Scale::uniform(size * self.scale_y);
        let baseline = y as f32 * self.scale_y + self.font.v_metrics(scale).ascent;
        let glyphs: Vec<_> = self
            .font
            .layout(text, scale, point(x as f32 * self.scale_x, baseline))
            .collect();
        let canvas = &mut self.canvas;
        for glyph in glyphs {
            let Some(bounds) = glyph.pixel_bounding_box() else {
                continue;
            };
            glyph.draw(|gx, gy, coverage| {
                if coverage <= 0.0 {
                    return;
                }
                canvas.set_draw_color(sdl2::pixels::Color::RGBA(
                    color.r,
                    color.g,
                    color.b,
                    (coverage * 255.0) as u8,
                ));
                let _ = canvas.fill_rect(Rect::new(
                    bounds.min.x + gx as i32,
                    bounds.min.y + gy as i32,
                    1,
                    1,
                ));
            });
        }
    }

    fn draw_wrapped_text(
        &mut self,
        text: &str,
        x: i32,
        y: i32,
        max_width: i32,
        size: f32,
        color: sdl2::pixels::Color,
        max_lines: usize,
    ) {
        let scale = rusttype::Scale::uniform(size * self.scale_x);
        let mut lines = Vec::new();
        let mut line = String::new();
        for character in text.chars().chain(std::iter::once('\n')) {
            if character == '\n' {
                lines.push(std::mem::take(&mut line));
                continue;
            }
            let mut candidate = line.clone();
            candidate.push(character);
            let width: f32 = candidate
                .chars()
                .map(|character| {
                    self.font
                        .glyph(character)
                        .scaled(scale)
                        .h_metrics()
                        .advance_width
                })
                .sum();
            if !line.is_empty() && width > max_width as f32 * self.scale_x {
                lines.push(std::mem::take(&mut line));
            }
            line.push(character);
        }
        if !line.is_empty() {
            lines.push(line);
        }
        let clipped = lines.len() > max_lines;
        lines.truncate(max_lines);
        if clipped {
            if let Some(last) = lines.last_mut() {
                last.push_str("...");
            }
        }
        for (index, line) in lines.iter().enumerate() {
            self.draw_text(
                line,
                x,
                y + index as i32 * (size.ceil() as i32 + 7),
                size,
                color,
            );
        }
    }

    fn fill_logical_rect(
        &mut self,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        color: sdl2::pixels::Color,
    ) {
        use sdl2::rect::Rect;
        self.canvas.set_draw_color(color);
        let _ = self.canvas.fill_rect(Rect::new(
            (x as f32 * self.scale_x).round() as i32,
            (y as f32 * self.scale_y).round() as i32,
            (width as f32 * self.scale_x).round().max(1.0) as u32,
            (height as f32 * self.scale_y).round().max(1.0) as u32,
        ));
    }

    fn draw_logical_rect(
        &mut self,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
        color: sdl2::pixels::Color,
    ) {
        use sdl2::rect::Rect;
        self.canvas.set_draw_color(color);
        let _ = self.canvas.draw_rect(Rect::new(
            (x as f32 * self.scale_x).round() as i32,
            (y as f32 * self.scale_y).round() as i32,
            (width as f32 * self.scale_x).round().max(1.0) as u32,
            (height as f32 * self.scale_y).round().max(1.0) as u32,
        ));
    }
}

#[cfg(not(target_os = "android"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConflictWindowAction {
    SelectLocal,
    SelectCloud,
    PreviousCloudVersion,
    NextCloudVersion,
    PreviousFile,
    NextFile,
    Apply,
    Cancel,
}

#[cfg(not(target_os = "android"))]
fn conflict_action_at(x: i32, y: i32) -> Option<ConflictWindowAction> {
    if contains(60, 426, 400, 54, x, y) {
        Some(ConflictWindowAction::SelectLocal)
    } else if contains(520, 426, 400, 54, x, y) {
        Some(ConflictWindowAction::SelectCloud)
    } else if contains(862, 252, 32, 32, x, y) {
        Some(ConflictWindowAction::PreviousCloudVersion)
    } else if contains(900, 252, 32, 32, x, y) {
        Some(ConflictWindowAction::NextCloudVersion)
    } else if contains(40, 606, 184, 48, x, y) {
        Some(ConflictWindowAction::PreviousFile)
    } else if contains(236, 606, 184, 48, x, y) {
        Some(ConflictWindowAction::NextFile)
    } else if contains(648, 660, 190, 46, x, y) {
        Some(ConflictWindowAction::Apply)
    } else if contains(850, 660, 90, 46, x, y) {
        Some(ConflictWindowAction::Cancel)
    } else {
        None
    }
}

#[cfg(not(target_os = "android"))]
fn contains(x: i32, y: i32, width: u32, height: u32, px: i32, py: i32) -> bool {
    px >= x && py >= y && px < x + width as i32 && py < y + height as i32
}

#[cfg(test)]
mod picker_display_tests {
    use super::*;

    #[test]
    fn android_display_orientation_maps_to_guest_content_orientation() {
        assert_eq!(
            device_orientation_from_display(
                sdl2_sys::SDL_DisplayOrientation::SDL_ORIENTATION_PORTRAIT
            ),
            Some(DeviceOrientation::Portrait)
        );
        assert_eq!(
            device_orientation_from_display(
                sdl2_sys::SDL_DisplayOrientation::SDL_ORIENTATION_PORTRAIT_FLIPPED
            ),
            Some(DeviceOrientation::PortraitUpsideDown)
        );
        assert_eq!(
            device_orientation_from_display(
                sdl2_sys::SDL_DisplayOrientation::SDL_ORIENTATION_LANDSCAPE
            ),
            Some(DeviceOrientation::LandscapeLeft)
        );
        assert_eq!(
            device_orientation_from_display(
                sdl2_sys::SDL_DisplayOrientation::SDL_ORIENTATION_LANDSCAPE_FLIPPED
            ),
            Some(DeviceOrientation::LandscapeRight)
        );
        assert_eq!(
            device_orientation_from_display(
                sdl2_sys::SDL_DisplayOrientation::SDL_ORIENTATION_UNKNOWN
            ),
            None
        );
    }

    #[test]
    fn picker_render_scale_tracks_display_and_caps_at_four() {
        assert_eq!(
            picker_scale_hack(
                (1080, 2400),
                DeviceFamily::iPhone,
                DeviceOrientation::Portrait
            )
            .get(),
            3
        );
        assert_eq!(
            picker_scale_hack(
                (2400, 1080),
                DeviceFamily::iPhone,
                DeviceOrientation::LandscapeLeft
            )
            .get(),
            3
        );
        assert_eq!(
            picker_scale_hack(
                (720, 1280),
                DeviceFamily::iPhone,
                DeviceOrientation::Portrait
            )
            .get(),
            2
        );
        assert_eq!(
            picker_scale_hack(
                (4000, 8000),
                DeviceFamily::iPhone,
                DeviceOrientation::Portrait
            )
            .get(),
            4
        );
    }
}

#[cfg(not(target_os = "android"))]
fn describe_local_conflict_entry(entry: Option<&SnapshotEntry>) -> String {
    match entry {
        None | Some(SnapshotEntry::Tombstone) => "Deleted on this Mac".to_owned(),
        Some(SnapshotEntry::File {
            size,
            modified_unix_ms,
            ..
        }) => format!(
            "{size} bytes\nModified (device time) {}\nSource: current touchHLE data folder",
            crate::environment::app_picker::format_unix_ms_local(*modified_unix_ms)
        ),
    }
}

#[cfg(not(target_os = "android"))]
fn describe_conflict_candidate(candidate: &crate::sync::reconcile::RemoteCandidate) -> String {
    match (&candidate.id, &candidate.entry) {
        (RemoteVersionId::NoDriveFile, _)
        | (RemoteVersionId::DriveFile(_), None | Some(SnapshotEntry::Tombstone)) => {
            return "Deleted in Google Drive".to_owned();
        }
        (RemoteVersionId::LegacyCommit(_), None | Some(SnapshotEntry::Tombstone)) => {
            return "Deleted in cloud (legacy revision)".to_owned();
        }
        _ => {}
    }
    let contents = describe_local_conflict_entry(candidate.entry.as_ref());
    let source = match (&candidate.id, &candidate.file) {
        (RemoteVersionId::DriveFile(id), Some(file)) => {
            format!("Drive version {} · file ID {id}", file.version)
        }
        (RemoteVersionId::LegacyCommit(id), _) => format!("Legacy revision {id}"),
        (RemoteVersionId::NoDriveFile, _) => "Deleted in Google Drive".to_owned(),
        _ => "Google Drive version".to_owned(),
    };
    format!("{contents}\n{source}")
}

#[cfg(all(test, not(target_os = "android")))]
mod sync_conflict_display_tests {
    use super::*;

    #[test]
    fn local_deletion_is_attributed_to_this_mac() {
        assert_eq!(describe_local_conflict_entry(None), "Deleted on this Mac");
    }

    #[test]
    fn missing_drive_file_is_attributed_to_google_drive() {
        let candidate = crate::sync::reconcile::RemoteCandidate {
            id: RemoteVersionId::NoDriveFile,
            entry: None,
            file: None,
        };
        assert_eq!(
            describe_conflict_candidate(&candidate),
            "Deleted in Google Drive"
        );
    }

    #[test]
    fn conflict_window_hit_testing_uses_raw_window_coordinates() {
        assert_eq!(
            conflict_action_at(200, 450),
            Some(ConflictWindowAction::SelectLocal)
        );
        assert_eq!(
            conflict_action_at(700, 450),
            Some(ConflictWindowAction::SelectCloud)
        );
        assert_eq!(
            conflict_action_at(132, 630),
            Some(ConflictWindowAction::PreviousFile)
        );
        assert_eq!(
            conflict_action_at(328, 630),
            Some(ConflictWindowAction::NextFile)
        );
        assert_eq!(
            conflict_action_at(878, 268),
            Some(ConflictWindowAction::PreviousCloudVersion)
        );
        assert_eq!(
            conflict_action_at(916, 268),
            Some(ConflictWindowAction::NextCloudVersion)
        );
        assert_eq!(
            conflict_action_at(700, 680),
            Some(ConflictWindowAction::Apply)
        );
        assert_eq!(
            conflict_action_at(890, 680),
            Some(ConflictWindowAction::Cancel)
        );
        assert_eq!(conflict_action_at(950, 710), None);
    }
}

/// Get current battery state from SDL2.
///
/// Returns:
/// - pct: i32 - percentage of battery remaining.
/// - status: [BatteryState] - the current status of the battery
///   (unplugged, charging, full, etc.)
pub fn get_battery_status() -> (i32, BatteryState) {
    if env::consts::OS == "android" {
        log_once!(
            "Warning: get_battery_status on Android, returning fully charged to avoid SDL crash"
        );
        // Android_JNI_GetPowerInfo is crashing with `JNI DETECTED ERROR IN
        // APPLICATION: JNI ERROR (app bug): jobject is an invalid JNI
        // transition frame reference: 0x7b1bc0c7a0 (use of invalid jobject)`
        // TODO: See if updating SDL fixes that
        return (100, BatteryState::Full);
    }
    let mut pct = 0;
    // Unfortunately, Rust-SDL2 does not expose this function yet.
    // iPhoneOS does not measure the battery in seconds remaining,
    // so we discard this argument.
    let status = unsafe { sdl2_sys::SDL_GetPowerInfo(null_mut(), &mut pct) };
    (
        pct,
        match status {
            SDL_PowerState::SDL_POWERSTATE_UNKNOWN => BatteryState::Unknown,
            SDL_PowerState::SDL_POWERSTATE_ON_BATTERY => BatteryState::OnBattery,
            SDL_PowerState::SDL_POWERSTATE_NO_BATTERY => BatteryState::NoBattery,
            SDL_PowerState::SDL_POWERSTATE_CHARGING => BatteryState::Charging,
            SDL_PowerState::SDL_POWERSTATE_CHARGED => BatteryState::Full,
        },
    )
}

pub fn get_preferred_language_codes(env: &mut Environment) -> Vec<String> {
    env.on_parent_stack_in_coroutine(|_, _| {
        sdl2::locale::get_preferred_locales()
            .map(|loc| loc.lang)
            .collect()
    })
}

pub fn get_preferred_country_codes(env: &mut Environment) -> Vec<String> {
    env.on_parent_stack_in_coroutine(|_, _| {
        sdl2::locale::get_preferred_locales()
            .filter_map(|loc| loc.country)
            .collect()
    })
}
