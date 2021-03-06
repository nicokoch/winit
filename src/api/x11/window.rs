use {Event, MouseCursor};
use CreationError;
use CreationError::OsError;
use libc;
use std::borrow::Borrow;
use std::{mem, ptr};
use std::cell::Cell;
use std::sync::atomic::AtomicBool;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::os::raw::c_long;
use std::thread;
use std::time::Duration;

use CursorState;
use WindowAttributes;

use platform::MonitorId as PlatformMonitorId;

use super::input::XInputEventHandler;
use super::{ffi};
use super::{MonitorId, XConnection};

// XOpenIM doesn't seem to be thread-safe
lazy_static! {      // TODO: use a static mutex when that's possible, and put me back in my function
    static ref GLOBAL_XOPENIM_LOCK: Mutex<()> = Mutex::new(());
}

// TODO: remove me
fn with_c_str<F, T>(s: &str, f: F) -> T where F: FnOnce(*const libc::c_char) -> T {
    use std::ffi::CString;
    let c_str = CString::new(s.as_bytes().to_vec()).unwrap();
    f(c_str.as_ptr())
}

struct WindowProxyData {
    display: Arc<XConnection>,
    window: ffi::Window,
}

unsafe impl Send for WindowProxyData {}

pub struct XWindow {
    display: Arc<XConnection>,
    window: ffi::Window,
    is_fullscreen: bool,
    screen_id: libc::c_int,
    xf86_desk_mode: Option<ffi::XF86VidModeModeInfo>,
    ic: ffi::XIC,
    im: ffi::XIM,
    window_proxy_data: Arc<Mutex<Option<WindowProxyData>>>,
}

unsafe impl Send for XWindow {}
unsafe impl Sync for XWindow {}

unsafe impl Send for Window {}
unsafe impl Sync for Window {}

impl Drop for XWindow {
    fn drop(&mut self) {
        unsafe {
            // Clear out the window proxy data arc, so that any window proxy objects
            // are no longer able to send messages to this window.
            *self.window_proxy_data.lock().unwrap() = None;

            let _lock = GLOBAL_XOPENIM_LOCK.lock().unwrap();

            if self.is_fullscreen {
                if let Some(mut xf86_desk_mode) = self.xf86_desk_mode {
                    (self.display.xf86vmode.XF86VidModeSwitchToMode)(self.display.display, self.screen_id, &mut xf86_desk_mode);
                }
                (self.display.xf86vmode.XF86VidModeSetViewPort)(self.display.display, self.screen_id, 0, 0);
            }

            (self.display.xlib.XDestroyIC)(self.ic);
            (self.display.xlib.XCloseIM)(self.im);
            (self.display.xlib.XDestroyWindow)(self.display.display, self.window);
        }
    }
}

#[derive(Clone)]
pub struct WindowProxy {
    data: Arc<Mutex<Option<WindowProxyData>>>,
}

impl WindowProxy {
    pub fn wakeup_event_loop(&self) {
        let window_proxy_data = self.data.lock().unwrap();

        if let Some(ref data) = *window_proxy_data {
            let mut xev = ffi::XClientMessageEvent {
                type_: ffi::ClientMessage,
                window: data.window,
                format: 32,
                message_type: 0,
                serial: 0,
                send_event: 0,
                display: data.display.display,
                data: unsafe { mem::zeroed() },
            };

            unsafe {
                (data.display.xlib.XSendEvent)(data.display.display, data.window, 0, 0, mem::transmute(&mut xev));
                (data.display.xlib.XFlush)(data.display.display);
                data.display.check_errors().expect("Failed to call XSendEvent after wakeup");
            }
        }
    }
}

// XEvents of type GenericEvent store their actual data
// in an XGenericEventCookie data structure. This is a wrapper
// to extract the cookie from a GenericEvent XEvent and release
// the cookie data once it has been processed
struct GenericEventCookie<'a> {
    display: &'a XConnection,
    cookie: ffi::XGenericEventCookie
}

impl<'a> GenericEventCookie<'a> {
    fn from_event<'b>(display: &'b XConnection, event: ffi::XEvent) -> Option<GenericEventCookie<'b>> {
        unsafe {
            let mut cookie: ffi::XGenericEventCookie = From::from(event);
            if (display.xlib.XGetEventData)(display.display, &mut cookie) == ffi::True {
                Some(GenericEventCookie{display: display, cookie: cookie})
            } else {
                None
            }
        }
    }
}

impl<'a> Drop for GenericEventCookie<'a> {
    fn drop(&mut self) {
        unsafe {
            let xlib = &self.display.xlib;
            (xlib.XFreeEventData)(self.display.display, &mut self.cookie);
        }
    }
}

pub struct PollEventsIterator<'a> {
    window: &'a Window
}

impl<'a> Iterator for PollEventsIterator<'a> {
    type Item = Event;

    fn next(&mut self) -> Option<Event> {
        let xlib = &self.window.x.display.xlib;

        loop {
            if let Some(ev) = self.window.pending_events.lock().unwrap().pop_front() {
                return Some(ev);
            }

            let mut xev = unsafe { mem::uninitialized() };
            let res = unsafe { (xlib.XCheckMaskEvent)(self.window.x.display.display, -1, &mut xev) };

            if res == 0 {
                let res = unsafe { (xlib.XCheckTypedEvent)(self.window.x.display.display, ffi::ClientMessage, &mut xev) };

                if res == 0 {
                    let res = unsafe { (xlib.XCheckTypedEvent)(self.window.x.display.display, ffi::GenericEvent, &mut xev) };
                    if res == 0 {
                        return None;
                    }
                }
            }

            match xev.get_type() {
                ffi::MappingNotify => {
                    unsafe { (xlib.XRefreshKeyboardMapping)(mem::transmute(&xev)); }
                    self.window.x.display.check_errors().expect("Failed to call XRefreshKeyboardMapping");
                },

                ffi::ClientMessage => {
                    use events::Event::{Closed, Awakened};
                    use std::sync::atomic::Ordering::Relaxed;

                    let client_msg: &ffi::XClientMessageEvent = unsafe { mem::transmute(&xev) };

                    if client_msg.data.get_long(0) == self.window.wm_delete_window as libc::c_long {
                        self.window.is_closed.store(true, Relaxed);
                        return Some(Closed);
                    } else {
                        return Some(Awakened);
                    }
                },

                ffi::ConfigureNotify => {
                    use events::Event::Resized;
                    let cfg_event: &ffi::XConfigureEvent = unsafe { mem::transmute(&xev) };
                    let (current_width, current_height) = self.window.current_size.get();
                    if current_width != cfg_event.width || current_height != cfg_event.height {
                        self.window.current_size.set((cfg_event.width, cfg_event.height));
                        return Some(Resized(cfg_event.width as u32, cfg_event.height as u32));
                    }
                },

                ffi::Expose => {
                    use events::Event::Refresh;
                    return Some(Refresh);
                },

                ffi::KeyPress | ffi::KeyRelease => {
                    let mut event: &mut ffi::XKeyEvent = unsafe { mem::transmute(&mut xev) };
                    let events = self.window.input_handler.lock().unwrap().translate_key_event(&mut event);
                    for event in events {
                        self.window.pending_events.lock().unwrap().push_back(event);
                    }
                },

                ffi::GenericEvent => {
                    if let Some(cookie) = GenericEventCookie::from_event(self.window.x.display.borrow(), xev) {
                        match cookie.cookie.evtype {
                            ffi::XI_DeviceChanged...ffi::XI_LASTEVENT => {
                                match self.window.input_handler.lock() {
                                    Ok(mut handler) => {
                                        match handler.translate_event(&cookie.cookie) {
                                            Some(event) => self.window.pending_events.lock().unwrap().push_back(event),
                                            None => {}
                                        }
                                    },
                                    Err(_) => {}
                                }
                            },
                            _ => {}
                        }
                    }
                }

                _ => {}
            };
        }
    }
}

pub struct WaitEventsIterator<'a> {
    window: &'a Window,
}

impl<'a> Iterator for WaitEventsIterator<'a> {
    type Item = Event;

    fn next(&mut self) -> Option<Event> {
        use std::sync::atomic::Ordering::Relaxed;
        use std::mem;

        while !self.window.is_closed.load(Relaxed) {
            if let Some(ev) = self.window.pending_events.lock().unwrap().pop_front() {
                return Some(ev);
            }

            // this will block until an event arrives, but doesn't remove
            // it from the queue
            let mut xev = unsafe { mem::uninitialized() };
            unsafe { (self.window.x.display.xlib.XPeekEvent)(self.window.x.display.display, &mut xev) };
            self.window.x.display.check_errors().expect("Failed to call XPeekEvent");

            // calling poll_events()
            if let Some(ev) = self.window.poll_events().next() {
                return Some(ev);
            }
        }

        None
    }
}

pub struct Window {
    pub x: Arc<XWindow>,
    is_closed: AtomicBool,
    wm_delete_window: ffi::Atom,
    current_size: Cell<(libc::c_int, libc::c_int)>,
    /// Events that have been retreived with XLib but not dispatched with iterators yet
    pending_events: Mutex<VecDeque<Event>>,
    cursor_state: Mutex<CursorState>,
    input_handler: Mutex<XInputEventHandler>
}

impl Window {
    pub fn new(display: &Arc<XConnection>, window_attrs: &WindowAttributes)
               -> Result<Window, CreationError>
    {
        let dimensions = window_attrs.dimensions.unwrap_or((800, 600));

        // not implemented
        assert!(window_attrs.min_dimensions.is_none());
        assert!(window_attrs.max_dimensions.is_none());

        let screen_id = match window_attrs.monitor {
            Some(PlatformMonitorId::X(MonitorId(_, monitor))) => monitor as i32,
            _ => unsafe { (display.xlib.XDefaultScreen)(display.display) },
        };

        // finding the mode to switch to if necessary
        let (mode_to_switch_to, xf86_desk_mode) = unsafe {
            let mut mode_num: libc::c_int = mem::uninitialized();
            let mut modes: *mut *mut ffi::XF86VidModeModeInfo = mem::uninitialized();
            if (display.xf86vmode.XF86VidModeGetAllModeLines)(display.display, screen_id, &mut mode_num, &mut modes) == 0 {
                (None, None)
            } else {
                let xf86_desk_mode: ffi::XF86VidModeModeInfo = ptr::read(*modes.offset(0));
                let mode_to_switch_to = if window_attrs.monitor.is_some() {
                    let matching_mode = (0 .. mode_num).map(|i| {
                        let m: ffi::XF86VidModeModeInfo = ptr::read(*modes.offset(i as isize) as *const _); m
                    }).find(|m| m.hdisplay == dimensions.0 as u16 && m.vdisplay == dimensions.1 as u16);
                    if let Some(matching_mode) = matching_mode {
                        Some(matching_mode)
                    } else {
                        let m = (0 .. mode_num).map(|i| {
                            let m: ffi::XF86VidModeModeInfo = ptr::read(*modes.offset(i as isize) as *const _); m
                        }).find(|m| m.hdisplay >= dimensions.0 as u16 && m.vdisplay >= dimensions.1 as u16);

                        match m {
                            Some(m) => Some(m),
                            None => return Err(OsError(format!("Could not find a suitable graphics mode")))
                        }
                    }
                } else {
                    None
                };
                (display.xlib.XFree)(modes as *mut _);
                (mode_to_switch_to, Some(xf86_desk_mode))
            }
        };

        // getting the root window
        let root = unsafe { (display.xlib.XDefaultRootWindow)(display.display) };
        display.check_errors().expect("Failed to get root window");

        // creating
        let mut set_win_attr = {
            let mut swa: ffi::XSetWindowAttributes = unsafe { mem::zeroed() };
            swa.colormap = 0;
            swa.event_mask = ffi::ExposureMask | ffi::StructureNotifyMask |
                ffi::VisibilityChangeMask | ffi::KeyPressMask | ffi::PointerMotionMask |
                ffi::KeyReleaseMask | ffi::ButtonPressMask |
                ffi::ButtonReleaseMask | ffi::KeymapStateMask;
            swa.border_pixel = 0;
            if window_attrs.transparent {
                swa.background_pixel = 0;
            }
            swa.override_redirect = 0;
            swa
        };

        let mut window_attributes = ffi::CWBorderPixel | ffi::CWColormap | ffi::CWEventMask;

        if window_attrs.transparent {
            window_attributes |= ffi::CWBackPixel;
        }

        // finally creating the window
        let window = unsafe {
            let win = (display.xlib.XCreateWindow)(display.display, root, 0, 0, dimensions.0 as libc::c_uint,
                dimensions.1 as libc::c_uint, 0, ffi::CopyFromParent, ffi::InputOutput as libc::c_uint,
                ffi::CopyFromParent as *mut _, window_attributes,
                &mut set_win_attr);
            display.check_errors().expect("Failed to call XCreateWindow");
            win
        };

        // set visibility
        if window_attrs.visible {
            unsafe {
                (display.xlib.XMapRaised)(display.display, window);
                (display.xlib.XFlush)(display.display);
            }

            display.check_errors().expect("Failed to set window visibility");
        }

        // creating window, step 2
        let wm_delete_window = unsafe {
            let mut wm_delete_window = with_c_str("WM_DELETE_WINDOW", |delete_window|
                (display.xlib.XInternAtom)(display.display, delete_window, 0)
            );
            display.check_errors().expect("Failed to call XInternAtom");
            (display.xlib.XSetWMProtocols)(display.display, window, &mut wm_delete_window, 1);
            display.check_errors().expect("Failed to call XSetWMProtocols");
            (display.xlib.XFlush)(display.display);
            display.check_errors().expect("Failed to call XFlush");

            wm_delete_window
        };

        // creating IM
        let im = unsafe {
            let _lock = GLOBAL_XOPENIM_LOCK.lock().unwrap();

            let im = (display.xlib.XOpenIM)(display.display, ptr::null_mut(), ptr::null_mut(), ptr::null_mut());
            if im.is_null() {
                return Err(OsError(format!("XOpenIM failed")));
            }
            im
        };

        // creating input context
        let ic = unsafe {
            let ic = with_c_str("inputStyle", |input_style|
                with_c_str("clientWindow", |client_window|
                    (display.xlib.XCreateIC)(
                        im, input_style,
                        ffi::XIMPreeditNothing | ffi::XIMStatusNothing, client_window,
                        window, ptr::null::<()>()
                    )
                )
            );
            if ic.is_null() {
                return Err(OsError(format!("XCreateIC failed")));
            }
            (display.xlib.XSetICFocus)(ic);
            display.check_errors().expect("Failed to call XSetICFocus");
            ic
        };

        // Attempt to make keyboard input repeat detectable
        unsafe {
            let mut supported_ptr = ffi::False;
            (display.xlib.XkbSetDetectableAutoRepeat)(display.display, ffi::True, &mut supported_ptr);
            if supported_ptr == ffi::False {
                return Err(OsError(format!("XkbSetDetectableAutoRepeat failed")));
            }
        }

        // Set ICCCM WM_CLASS property based on initial window title
        unsafe {
            with_c_str(&*window_attrs.title, |c_name| {
                let hint = (display.xlib.XAllocClassHint)();
                (*hint).res_name = c_name as *mut libc::c_char;
                (*hint).res_class = c_name as *mut libc::c_char;
                (display.xlib.XSetClassHint)(display.display, window, hint);
                display.check_errors().expect("Failed to call XSetClassHint");
                (display.xlib.XFree)(hint as *mut _);
            });
        }

        let is_fullscreen = window_attrs.monitor.is_some();

        if is_fullscreen {
            let state_atom = unsafe {
                with_c_str("_NET_WM_STATE", |state|
                    (display.xlib.XInternAtom)(display.display, state, 0)
                )
            };
            display.check_errors().expect("Failed to call XInternAtom");
            let fullscreen_atom = unsafe {
                with_c_str("_NET_WM_STATE_FULLSCREEN", |state_fullscreen|
                    (display.xlib.XInternAtom)(display.display, state_fullscreen, 0)
                )
            };
            display.check_errors().expect("Failed to call XInternAtom");

            let client_message_event = ffi::XClientMessageEvent {
                type_: ffi::ClientMessage,
                serial: 0,
                send_event: 1,            // true because we are sending this through `XSendEvent`
                display: display.display,
                window: window,
                message_type: state_atom, // the _NET_WM_STATE atom is sent to change the state of a window
                format: 32,               // view `data` as `c_long`s
                data: {
                    let mut data = ffi::ClientMessageData::new();
                    // This first `long` is the action; `1` means add/set following property.
                    data.set_long(0, 1);
                    // This second `long` is the property to set (fullscreen)
                    data.set_long(1, fullscreen_atom as c_long);
                    data
                }
            };
            let mut x_event = ffi::XEvent::from(client_message_event);

            unsafe {
                (display.xlib.XSendEvent)(
                    display.display,
                    root,
                    0,
                    ffi::SubstructureRedirectMask | ffi::SubstructureNotifyMask,
                    &mut x_event as *mut _
                );
                display.check_errors().expect("Failed to call XSendEvent");
            }

            if let Some(mut mode_to_switch_to) = mode_to_switch_to {
                unsafe {
                    (display.xf86vmode.XF86VidModeSwitchToMode)(
                        display.display,
                        screen_id,
                        &mut mode_to_switch_to
                    );
                    display.check_errors().expect("Failed to call XF86VidModeSwitchToMode");
                }
            }
            else {
                println!("[glutin] Unexpected state: `mode` is None creating fullscreen window");
            }
            unsafe {
                (display.xf86vmode.XF86VidModeSetViewPort)(display.display, screen_id, 0, 0);
                display.check_errors().expect("Failed to call XF86VidModeSetViewPort");
            }
        }

        // creating the window object
        let window_proxy_data = WindowProxyData {
            display: display.clone(),
            window: window,
        };
        let window_proxy_data = Arc::new(Mutex::new(Some(window_proxy_data)));

        let window = Window {
            x: Arc::new(XWindow {
                display: display.clone(),
                window: window,
                im: im,
                ic: ic,
                screen_id: screen_id,
                is_fullscreen: is_fullscreen,
                xf86_desk_mode: xf86_desk_mode,
                window_proxy_data: window_proxy_data,
            }),
            is_closed: AtomicBool::new(false),
            wm_delete_window: wm_delete_window,
            current_size: Cell::new((0, 0)),
            pending_events: Mutex::new(VecDeque::new()),
            cursor_state: Mutex::new(CursorState::Normal),
            input_handler: Mutex::new(XInputEventHandler::new(display, window, ic, window_attrs))
        };

        window.set_title(&window_attrs.title);

        if window_attrs.visible {
            unsafe {
                let ref x_window: &XWindow = window.x.borrow();

                // XSetInputFocus generates an error if the window is not visible,
                // therefore we wait until it's the case.
                loop {
                    let mut window_attributes = mem::uninitialized();
                    (display.xlib.XGetWindowAttributes)(display.display, x_window.window, &mut window_attributes);
                    display.check_errors().expect("Failed to call XGetWindowAttributes");

                    if window_attributes.map_state == ffi::IsViewable {
                        (display.xlib.XSetInputFocus)(
                            display.display,
                            x_window.window,
                            ffi::RevertToParent,
                            ffi::CurrentTime
                        );
                        display.check_errors().expect("Failed to call XSetInputFocus");
                        break;
                    }

                    // Wait about a frame to avoid too-busy waiting
                    thread::sleep(Duration::from_millis(16));
                }
            }
        }

        // returning
        Ok(window)
    }

    pub fn set_title(&self, title: &str) {
        let wm_name = unsafe {
            (self.x.display.xlib.XInternAtom)(self.x.display.display, b"_NET_WM_NAME\0".as_ptr() as *const _, 0)
        };
        self.x.display.check_errors().expect("Failed to call XInternAtom");

        let wm_utf8_string = unsafe {
            (self.x.display.xlib.XInternAtom)(self.x.display.display, b"UTF8_STRING\0".as_ptr() as *const _, 0)
        };
        self.x.display.check_errors().expect("Failed to call XInternAtom");

        with_c_str(title, |c_title| unsafe {
            (self.x.display.xlib.XStoreName)(self.x.display.display, self.x.window, c_title);

            let len = title.as_bytes().len();
            (self.x.display.xlib.XChangeProperty)(self.x.display.display, self.x.window,
                                            wm_name, wm_utf8_string, 8, ffi::PropModeReplace,
                                            c_title as *const u8, len as libc::c_int);
            (self.x.display.xlib.XFlush)(self.x.display.display);
        });
        self.x.display.check_errors().expect("Failed to set window title");

    }

    pub fn show(&self) {
        unsafe {
            (self.x.display.xlib.XMapRaised)(self.x.display.display, self.x.window);
            (self.x.display.xlib.XFlush)(self.x.display.display);
            self.x.display.check_errors().expect("Failed to call XMapRaised");
        }
    }

    pub fn hide(&self) {
        unsafe {
            (self.x.display.xlib.XUnmapWindow)(self.x.display.display, self.x.window);
            (self.x.display.xlib.XFlush)(self.x.display.display);
            self.x.display.check_errors().expect("Failed to call XUnmapWindow");
        }
    }

    fn get_geometry(&self) -> Option<(i32, i32, u32, u32, u32)> {
        unsafe {
            use std::mem;

            let mut root: ffi::Window = mem::uninitialized();
            let mut x: libc::c_int = mem::uninitialized();
            let mut y: libc::c_int = mem::uninitialized();
            let mut width: libc::c_uint = mem::uninitialized();
            let mut height: libc::c_uint = mem::uninitialized();
            let mut border: libc::c_uint = mem::uninitialized();
            let mut depth: libc::c_uint = mem::uninitialized();

            if (self.x.display.xlib.XGetGeometry)(self.x.display.display, self.x.window,
                &mut root, &mut x, &mut y, &mut width, &mut height,
                &mut border, &mut depth) == 0
            {
                return None;
            }

            Some((x as i32, y as i32, width as u32, height as u32, border as u32))
        }
    }

    #[inline]
    pub fn get_position(&self) -> Option<(i32, i32)> {
        self.get_geometry().map(|(x, y, _, _, _)| (x, y))
    }

    pub fn set_position(&self, x: i32, y: i32) {
        unsafe { (self.x.display.xlib.XMoveWindow)(self.x.display.display, self.x.window, x as libc::c_int, y as libc::c_int); }
        self.x.display.check_errors().expect("Failed to call XMoveWindow");
    }

    #[inline]
    pub fn get_inner_size(&self) -> Option<(u32, u32)> {
        self.get_geometry().map(|(_, _, w, h, _)| (w, h))
    }

    #[inline]
    pub fn get_outer_size(&self) -> Option<(u32, u32)> {
        self.get_geometry().map(|(_, _, w, h, b)| (w + b, h + b))       // TODO: is this really outside?
    }

    #[inline]
    pub fn set_inner_size(&self, x: u32, y: u32) {
        unsafe { (self.x.display.xlib.XResizeWindow)(self.x.display.display, self.x.window, x as libc::c_uint, y as libc::c_uint); }
        self.x.display.check_errors().expect("Failed to call XResizeWindow");
    }

    #[inline]
    pub fn create_window_proxy(&self) -> WindowProxy {
        WindowProxy {
            data: self.x.window_proxy_data.clone()
        }
    }

    #[inline]
    pub fn poll_events(&self) -> PollEventsIterator {
        PollEventsIterator {
            window: self
        }
    }

    #[inline]
    pub fn wait_events(&self) -> WaitEventsIterator {
        WaitEventsIterator {
            window: self
        }
    }

    #[inline]
    pub fn get_xlib_display(&self) -> *mut libc::c_void {
        self.x.display.display as *mut libc::c_void
    }

    #[inline]
    pub fn platform_display(&self) -> *mut libc::c_void {
        self.x.display.display as *mut libc::c_void
    }

    #[inline]
    pub fn get_xlib_window(&self) -> *mut libc::c_void {
        self.x.window as *mut libc::c_void
    }

    #[inline]
    pub fn platform_window(&self) -> *mut libc::c_void {
        self.x.window as *mut libc::c_void
    }

    #[inline]
    pub fn set_window_resize_callback(&mut self, _: Option<fn(u32, u32)>) {
    }

    pub fn set_cursor(&self, cursor: MouseCursor) {
        unsafe {
            use std::ffi::CString;
            let cursor_name = match cursor {
                MouseCursor::Alias => "link",
                MouseCursor::Arrow => "arrow",
                MouseCursor::Cell => "plus",
                MouseCursor::Copy => "copy",
                MouseCursor::Crosshair => "crosshair",
                MouseCursor::Default => "left_ptr",
                MouseCursor::Grabbing => "grabbing",
                MouseCursor::Hand | MouseCursor::Grab => "hand",
                MouseCursor::Help => "question_arrow",
                MouseCursor::Move => "move",
                MouseCursor::NoDrop => "circle",
                MouseCursor::NotAllowed => "crossed_circle",
                MouseCursor::Progress => "left_ptr_watch",

                /// Resize cursors
                MouseCursor::EResize => "right_side",
                MouseCursor::NResize => "top_side",
                MouseCursor::NeResize => "top_right_corner",
                MouseCursor::NwResize => "top_left_corner",
                MouseCursor::SResize => "bottom_side",
                MouseCursor::SeResize => "bottom_right_corner",
                MouseCursor::SwResize => "bottom_left_corner",
                MouseCursor::WResize => "left_side",
                MouseCursor::EwResize | MouseCursor::ColResize => "h_double_arrow",
                MouseCursor::NsResize | MouseCursor::RowResize => "v_double_arrow",
                MouseCursor::NwseResize => "bd_double_arrow",
                MouseCursor::NeswResize => "fd_double_arrow",

                MouseCursor::Text | MouseCursor::VerticalText => "xterm",
                MouseCursor::Wait => "watch",

                /// TODO: Find matching X11 cursors
                MouseCursor::ContextMenu | MouseCursor::NoneCursor |
                MouseCursor::AllScroll | MouseCursor::ZoomIn |
                MouseCursor::ZoomOut => "left_ptr",
            };
            let c_string = CString::new(cursor_name.as_bytes().to_vec()).unwrap();
            let xcursor = (self.x.display.xcursor.XcursorLibraryLoadCursor)(self.x.display.display, c_string.as_ptr());
            self.x.display.check_errors().expect("Failed to call XcursorLibraryLoadCursor");
            (self.x.display.xlib.XDefineCursor)(self.x.display.display, self.x.window, xcursor);
            (self.x.display.xlib.XFlush)(self.x.display.display);
            (self.x.display.xlib.XFreeCursor)(self.x.display.display, xcursor);
            self.x.display.check_errors().expect("Failed to call XDefineCursor");
        }
    }

    pub fn set_cursor_state(&self, state: CursorState) -> Result<(), String> {
        use CursorState::{ Grab, Normal, Hide };

        let mut cursor_state = self.cursor_state.lock().unwrap();
        match (state, *cursor_state) {
            (Normal, Normal) | (Hide, Hide) | (Grab, Grab) => return Ok(()),
            _ => {},
        }

        match *cursor_state {
            Grab => {
                unsafe {
                    (self.x.display.xlib.XUngrabPointer)(self.x.display.display, ffi::CurrentTime);
                    self.x.display.check_errors().expect("Failed to call XUngrabPointer");
                }
            },
            Normal => {},
            Hide => {
                unsafe {
                    let xcursor = (self.x.display.xlib.XCreateFontCursor)(self.x.display.display, 68/*XC_left_ptr*/);
                    self.x.display.check_errors().expect("Failed to call XCreateFontCursor");
                    (self.x.display.xlib.XDefineCursor)(self.x.display.display, self.x.window, xcursor);
                    self.x.display.check_errors().expect("Failed to call XDefineCursor");
                    (self.x.display.xlib.XFlush)(self.x.display.display);
                    (self.x.display.xlib.XFreeCursor)(self.x.display.display, xcursor);
                }
            },
        }

        *cursor_state = state;
        match state {
            Normal => Ok(()),
            Hide => {
                let data = &[0, 0, 0, 0, 0, 0, 0, 0];
                unsafe {
                    let mut black = ffi::XColor {
                        red: 0, green: 0, blue: 0,
                        pad: 0, pixel: 0, flags: 0,
                    };
                    let bitmap = (self.x.display.xlib.XCreateBitmapFromData)(self.x.display.display, self.x.window, data.as_ptr(), 8, 8);
                    let cursor = (self.x.display.xlib.XCreatePixmapCursor)(self.x.display.display, bitmap, bitmap, &mut black, &mut black, 0, 0);
                    (self.x.display.xlib.XDefineCursor)(self.x.display.display, self.x.window, cursor);
                    self.x.display.check_errors().expect("Failed to call XDefineCursor");
                    (self.x.display.xlib.XFreeCursor)(self.x.display.display, cursor);
                    (self.x.display.xlib.XFreePixmap)(self.x.display.display, bitmap);
                }
                Ok(())
            },
            Grab => {
                unsafe {
                    match (self.x.display.xlib.XGrabPointer)(
                        self.x.display.display, self.x.window, ffi::False,
                        (ffi::ButtonPressMask | ffi::ButtonReleaseMask | ffi::EnterWindowMask |
                        ffi::LeaveWindowMask | ffi::PointerMotionMask | ffi::PointerMotionHintMask |
                        ffi::Button1MotionMask | ffi::Button2MotionMask | ffi::Button3MotionMask |
                        ffi::Button4MotionMask | ffi::Button5MotionMask | ffi::ButtonMotionMask |
                        ffi::KeymapStateMask) as libc::c_uint,
                        ffi::GrabModeAsync, ffi::GrabModeAsync,
                        self.x.window, 0, ffi::CurrentTime
                    ) {
                        ffi::GrabSuccess => Ok(()),
                        ffi::AlreadyGrabbed | ffi::GrabInvalidTime |
                        ffi::GrabNotViewable | ffi::GrabFrozen
                            => Err("cursor could not be grabbed".to_string()),
                        _ => unreachable!(),
                    }
                }
            },
        }
    }

    #[inline]
    pub fn hidpi_factor(&self) -> f32 {
        1.0
    }

    pub fn set_cursor_position(&self, x: i32, y: i32) -> Result<(), ()> {
        unsafe {
            (self.x.display.xlib.XWarpPointer)(self.x.display.display, 0, self.x.window, 0, 0, 0, 0, x, y);
            self.x.display.check_errors().map_err(|_| ())
        }
    }
}
