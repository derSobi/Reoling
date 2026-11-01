//! Digital zoom for live and recorded video. Only the displayed picture is
//! transformed; snapshots and recordings still use the original frames.

use gtk4::{gdk, graphene, prelude::*, subclass::prelude::*};
use std::cell::{Cell, RefCell};

const MAX_ZOOM: f64 = 16.0;
const ZOOM_STEP: f64 = 1.2;

#[derive(Clone, Copy, Debug, PartialEq)]
struct Zoom {
    scale: f64,
    // Translation in fractions of the unzoomed picture, independent of
    // widget size so resizing and entering fullscreen preserve the crop.
    x: f64,
    y: f64,
}

impl Default for Zoom {
    fn default() -> Self {
        Self {
            scale: 1.0,
            x: 0.0,
            y: 0.0,
        }
    }
}

impl Zoom {
    fn pan(&mut self, dx: f64, dy: f64) {
        if !dx.is_finite() || !dy.is_finite() {
            return;
        }
        self.x = (self.x + dx).clamp(1.0 - self.scale, 0.0);
        self.y = (self.y + dy).clamp(1.0 - self.scale, 0.0);
    }

    fn scroll(&mut self, dy: f64, x: f64, y: f64) {
        if !dy.is_finite() || !x.is_finite() || !y.is_finite() {
            return;
        }
        let scale = (self.scale * ZOOM_STEP.powf(-dy)).clamp(1.0, MAX_ZOOM);
        if scale == self.scale {
            return;
        }
        if scale <= 1.0 + 1e-9 {
            *self = Self::default();
            return;
        }
        let ratio = scale / self.scale;
        // Keep the source point under the pointer stationary. Clamp when
        // zooming out near an edge so no empty area enters the picture.
        self.x = (x - (x - self.x) * ratio).clamp(1.0 - scale, 0.0);
        self.y = (y - (y - self.y) * ratio).clamp(1.0 - scale, 0.0);
        self.scale = scale;
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct PictureRect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

#[derive(Clone, Copy)]
struct PanDrag {
    zoom: Zoom,
    rect: PictureRect,
}

impl PictureRect {
    /// The picture's place in a `width` × `height` widget, on whole pixels
    /// as GtkPicture 4.14 and newer places it unzoomed: the size rounded up,
    /// the offset truncated. Zoomed at a fraction of a pixel, the outer rows
    /// or columns would be partly covered and show blended with the
    /// background.
    fn fit(width: f64, height: f64, aspect: f64, keep_aspect: bool) -> Option<Self> {
        if width <= 0.0 || height <= 0.0 {
            return None;
        }
        let (w, h) = if !keep_aspect || aspect <= 0.0 {
            (width, height)
        } else if aspect > width / height {
            (width, (width / aspect).ceil())
        } else {
            ((height * aspect).ceil(), height)
        };
        Some(Self {
            x: ((width - w) / 2.0).trunc(),
            y: ((height - h) / 2.0).trunc(),
            width: w,
            height: h,
        })
    }

    fn position(self, x: f64, y: f64) -> Option<(f64, f64)> {
        let x = (x - self.x) / self.width;
        let y = (y - self.y) / self.height;
        ((0.0..=1.0).contains(&x) && (0.0..=1.0).contains(&y)).then_some((x, y))
    }
}

/// The point a wheel step zooms about: where the pointer is at the scroll
/// event, else the last motion position. The latter is stale after a layout
/// change without pointer motion (entering fullscreen by double-click).
fn scroll_anchor(at_event: Option<(f64, f64)>, last_motion: Option<(f64, f64)>) -> Option<(f64, f64)> {
    at_event.or(last_motion)
}

/// The pointer's position on a Wayland surface, asked of the compositor. GDK
/// answers a position query from the last pointer event, and none follows when
/// the surface moves under a still pointer: fullscreen entered or left by
/// double-click, a window maximized or moved without the pointer. A second
/// `wl_pointer` on GDK's own connection is sent an `enter` at once, which on
/// GNOME Shell 50.1 carries the pointer's current position (measured); what
/// other compositors put there is not measured. The pointer is released as soon
/// as the `enter` has arrived, on a queue of its own, so GDK never sees its
/// events.
mod wayland {
    use gtk4::{gdk, glib::translate::ToGlibPtr, prelude::*};
    use std::ffi::{c_int, c_void};
    use std::ptr;

    #[repr(C)]
    struct Interface {
        _opaque: [u8; 0],
    }

    /// libwayland's `union wl_argument`, as far as `enter` needs it.
    #[repr(C)]
    union Argument {
        fixed: i32,
        object: *mut c_void,
    }

    type Dispatcher = unsafe extern "C" fn(*const c_void, *mut c_void, u32, *const c_void, *mut Argument) -> c_int;

    #[link(name = "wayland-client")]
    extern "C" {
        static wl_pointer_interface: Interface;
        fn wl_display_create_queue(display: *mut c_void) -> *mut c_void;
        fn wl_event_queue_destroy(queue: *mut c_void);
        fn wl_display_roundtrip_queue(display: *mut c_void, queue: *mut c_void) -> c_int;
        fn wl_proxy_create_wrapper(proxy: *mut c_void) -> *mut c_void;
        fn wl_proxy_wrapper_destroy(wrapper: *mut c_void);
        fn wl_proxy_set_queue(proxy: *mut c_void, queue: *mut c_void);
        fn wl_proxy_get_version(proxy: *mut c_void) -> u32;
        fn wl_proxy_get_user_data(proxy: *mut c_void) -> *mut c_void;
        fn wl_proxy_add_dispatcher(proxy: *mut c_void, dispatcher: Dispatcher, implementation: *const c_void,
            data: *mut c_void) -> c_int;
        fn wl_proxy_marshal_flags(proxy: *mut c_void, opcode: u32, interface: *const Interface, version: u32,
            flags: u32, ...) -> *mut c_void;
    }

    // GTK's Wayland backend, part of the GTK library.
    extern "C" {
        fn gdk_wayland_display_get_wl_display(display: *mut gdk::ffi::GdkDisplay) -> *mut c_void;
        fn gdk_wayland_seat_get_wl_seat(seat: *mut gdk::ffi::GdkSeat) -> *mut c_void;
        fn gdk_wayland_surface_get_wl_surface(surface: *mut gdk::ffi::GdkSurface) -> *mut c_void;
    }

    const SEAT_GET_POINTER: u32 = 0;
    const POINTER_ENTER: u32 = 0;
    const POINTER_RELEASE: u32 = 1;
    const POINTER_RELEASE_SINCE: u32 = 3;
    const MARSHAL_FLAG_DESTROY: u32 = 1;

    struct Enter {
        surface: *mut c_void,
        x: f64,
        y: f64,
    }

    /// Notes the pointer's `enter(serial, surface, x, y)`; every other event
    /// is ignored, including those of protocol versions newer than this code.
    unsafe extern "C" fn dispatch(
        _: *const c_void,
        proxy: *mut c_void,
        opcode: u32,
        _: *const c_void,
        args: *mut Argument,
    ) -> c_int {
        if opcode == POINTER_ENTER {
            let enter = &mut *wl_proxy_get_user_data(proxy).cast::<Enter>();
            enter.surface = (*args.add(1)).object;
            enter.x = f64::from((*args.add(2)).fixed) / 256.0;
            enter.y = f64::from((*args.add(3)).fixed) / 256.0;
        }
        0
    }

    /// The pointer's position on `surface` in its coordinates, or None: not on
    /// Wayland, no pointer on `seat` (asking for one would be a protocol
    /// error), the pointer over another surface or none of this client's.
    pub(super) fn pointer_position(surface: &gdk::Surface, seat: &gdk::Seat) -> Option<(f64, f64)> {
        if surface.display().type_().name() != "GdkWaylandDisplay"
            || !seat.capabilities().contains(gdk::SeatCapabilities::POINTER)
        {
            return None;
        }
        // SAFETY: the objects are GDK's own, alive for the duration of this
        // call; the new pointer and its queue are this function's alone, and
        // `enter` outlives the pointer, which is destroyed before returning.
        unsafe {
            let display = gdk_wayland_display_get_wl_display(surface.display().to_glib_none().0);
            let wl_seat = gdk_wayland_seat_get_wl_seat(seat.to_glib_none().0);
            let wl_surface = gdk_wayland_surface_get_wl_surface(surface.to_glib_none().0);
            if display.is_null() || wl_seat.is_null() || wl_surface.is_null() {
                return None;
            }
            let version = wl_proxy_get_version(wl_seat);
            if version < POINTER_RELEASE_SINCE {
                return None;
            }
            let queue = wl_display_create_queue(display);
            if queue.is_null() {
                return None;
            }
            let mut enter = Enter { surface: ptr::null_mut(), x: 0.0, y: 0.0 };
            let wrapper = wl_proxy_create_wrapper(wl_seat);
            if !wrapper.is_null() {
                wl_proxy_set_queue(wrapper, queue);
                let pointer = wl_proxy_marshal_flags(
                    wrapper,
                    SEAT_GET_POINTER,
                    &wl_pointer_interface,
                    version,
                    0,
                    ptr::null_mut::<c_void>(),
                );
                wl_proxy_wrapper_destroy(wrapper);
                if !pointer.is_null() {
                    if wl_proxy_add_dispatcher(pointer, dispatch, ptr::null(), ptr::addr_of_mut!(enter).cast()) == 0 {
                        wl_display_roundtrip_queue(display, queue);
                    }
                    wl_proxy_marshal_flags(pointer, POINTER_RELEASE, ptr::null(), version, MARSHAL_FLAG_DESTROY);
                }
            }
            wl_event_queue_destroy(queue);
            (enter.surface == wl_surface).then_some((enter.x, enter.y))
        }
    }
}

/// The cursor over the viewport: a closed hand while panning, an open one
/// over a zoomed picture.
fn cursor_name(dragging: bool, zoomed: bool, over_picture: bool) -> Option<&'static str> {
    if dragging {
        Some("grabbing")
    } else if zoomed && over_picture {
        Some("grab")
    } else {
        None
    }
}

/// Notes `name` as the cursor shown; true only when it differs from the one
/// noted before.
fn cursor_changes(shown: &Cell<Option<&'static str>>, name: Option<&'static str>) -> bool {
    shown.replace(name) != name
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct ZoomPicture {
        pub picture: gtk4::Picture,
        pub(super) zoom: Cell<Zoom>,
        pub(super) pointer: Cell<Option<(f64, f64)>>,
        pub(super) drag: Cell<Option<PanDrag>>,
        // The cursor last set, so motion events do not set it again.
        pub(super) cursor: Cell<Option<&'static str>>,
        // The paintable whose new frames redraw this widget while zoomed,
        // with its invalidate-contents and invalidate-size handlers.
        pub(super) watched: RefCell<Option<(gdk::Paintable, [glib::SignalHandlerId; 2])>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for ZoomPicture {
        const NAME: &'static str = "ReolingZoomPicture";
        type Type = super::ZoomPicture;
        type ParentType = gtk4::Widget;
    }

    impl ObjectImpl for ZoomPicture {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.set_layout_manager(Some(gtk4::BinLayout::new()));
            self.picture.set_parent(&*obj);
            // Controllers belong to the viewport, whose coordinates do
            // not change with the render transform.
            self.picture.set_can_target(false);

            let motion = gtk4::EventControllerMotion::new();
            let weak = obj.downgrade();
            motion.connect_enter(move |_, x, y| {
                if let Some(obj) = weak.upgrade() {
                    obj.imp().pointer.set(Some((x, y)));
                    obj.update_cursor();
                }
            });
            let weak = obj.downgrade();
            motion.connect_motion(move |_, x, y| {
                if let Some(obj) = weak.upgrade() {
                    obj.imp().pointer.set(Some((x, y)));
                    obj.update_cursor();
                }
            });
            let weak = obj.downgrade();
            motion.connect_leave(move |_| {
                if let Some(obj) = weak.upgrade() {
                    obj.imp().pointer.set(None);
                    obj.update_cursor();
                }
            });
            obj.add_controller(motion);

            // Discrete steps work on GTK 4.6 too, without the newer API
            // for distinguishing wheel units from touchpad pixel deltas.
            let scroll = gtk4::EventControllerScroll::new(
                gtk4::EventControllerScrollFlags::VERTICAL
                    | gtk4::EventControllerScrollFlags::DISCRETE,
            );
            let weak = obj.downgrade();
            scroll.connect_scroll(move |controller, _, dy| {
                if let Some(obj) = weak.upgrade() {
                    if let Some((x, y)) = scroll_anchor(obj.pointer_position(controller), obj.imp().pointer.get()) {
                        obj.imp().pointer.set(Some((x, y)));
                        if obj.scroll_at(dy, x, y) {
                            return glib::Propagation::Stop;
                        }
                    }
                }
                glib::Propagation::Proceed
            });
            obj.add_controller(scroll);

            let drag = gtk4::GestureDrag::new();
            drag.set_button(1);
            let weak = obj.downgrade();
            drag.connect_drag_begin(move |gesture, x, y| {
                if let Some(obj) = weak.upgrade() {
                    if !obj.begin_pan(x, y) {
                        gesture.set_state(gtk4::EventSequenceState::Denied);
                    }
                }
            });
            let weak = obj.downgrade();
            drag.connect_drag_update(move |gesture, dx, dy| {
                if let Some(obj) = weak.upgrade() {
                    // Claim only beyond the drag threshold: a double-click
                    // (fullscreen) still counts when the pointer moves a
                    // pixel or two between press and release.
                    if obj.update_pan(dx, dy) && dx.hypot(dy) >= f64::from(obj.settings().gtk_dnd_drag_threshold()) {
                        gesture.set_state(gtk4::EventSequenceState::Claimed);
                    }
                }
            });
            let weak = obj.downgrade();
            drag.connect_drag_end(move |_, dx, dy| {
                if let Some(obj) = weak.upgrade() {
                    obj.update_pan(dx, dy);
                    obj.end_pan();
                }
            });
            let weak = obj.downgrade();
            drag.connect_cancel(move |_, _| {
                if let Some(obj) = weak.upgrade() {
                    obj.end_pan();
                }
            });
            obj.add_controller(drag);

            let reset = gtk4::GestureClick::new();
            reset.set_button(2);
            let weak = obj.downgrade();
            reset.connect_pressed(move |gesture, _, _, _| {
                if let Some(obj) = weak.upgrade() {
                    obj.reset_zoom();
                    gesture.set_state(gtk4::EventSequenceState::Claimed);
                }
            });
            obj.add_controller(reset);
        }

        fn dispose(&self) {
            self.obj().watch(None);
            self.picture.unparent();
        }
    }

    impl WidgetImpl for ZoomPicture {
        fn snapshot(&self, snapshot: &gtk4::Snapshot) {
            let obj = self.obj();
            let zoom = self.zoom.get();
            // Unzoomed, the inner picture draws itself, pixel for pixel as a
            // plain GtkPicture would.
            if zoom == Zoom::default() {
                obj.snapshot_child(&self.picture, snapshot);
                return;
            }
            let (Some(rect), Some(paintable)) = (obj.picture_rect(), self.picture.paintable()) else {
                return;
            };
            // Clip only the sides the zoomed picture overhangs. At a pan
            // limit its own edge lies on the rectangle's, and a clip there as
            // well would blend that edge twice at a fractional display scale.
            let near = |a: f64, b: f64| (a - b).abs() < 1e-6;
            let open = |at_limit: bool| if at_limit { 1.0 } else { 0.0 };
            let (left, top) = (open(near(zoom.x, 0.0)), open(near(zoom.y, 0.0)));
            let (right, bottom) = (open(near(zoom.x + zoom.scale, 1.0)), open(near(zoom.y + zoom.scale, 1.0)));
            snapshot.push_clip(&graphene::Rect::new(
                (rect.x - left) as f32,
                (rect.y - top) as f32,
                (rect.width + left + right) as f32,
                (rect.height + top + bottom) as f32,
            ));
            snapshot.save();
            snapshot.translate(&graphene::Point::new(
                (rect.x * (1.0 - zoom.scale) + zoom.x * rect.width) as f32,
                (rect.y * (1.0 - zoom.scale) + zoom.y * rect.height) as f32,
            ));
            snapshot.scale(zoom.scale as f32, zoom.scale as f32);
            // Zoomed, the paintable is drawn here at the rectangle the clip
            // and the pan limits use, which is GtkPicture's own placement
            // (`PictureRect::fit`): no empty strip, and the edges look as
            // they do unzoomed. `watch` redraws this widget on new frames.
            snapshot.translate(&graphene::Point::new(rect.x as f32, rect.y as f32));
            paintable.snapshot(snapshot.upcast_ref::<gdk::Snapshot>(), rect.width, rect.height);
            snapshot.restore();
            snapshot.pop();
        }
    }
}

glib::wrapper! {
    /// A picture for the video view with digital zoom: the wheel zooms about
    /// the pointer (up to 16×), a left drag pans while zoomed, a middle click
    /// shows the whole picture again. Unzoomed it draws like a
    /// `gtk4::Picture`.
    pub struct ZoomPicture(ObjectSubclass<imp::ZoomPicture>)
        @extends gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl ZoomPicture {
    /// An empty, unzoomed picture.
    pub fn new() -> Self {
        glib::Object::new()
    }

    /// What the picture shows, if anything.
    pub fn paintable(&self) -> Option<gdk::Paintable> {
        self.imp().picture.paintable()
    }

    /// Shows `paintable` (a new stream, or nothing), unzoomed.
    pub fn set_paintable(&self, paintable: Option<&impl IsA<gdk::Paintable>>) {
        self.reset_zoom();
        self.imp().picture.set_paintable(paintable);
        self.watch(paintable.map(|p| p.as_ref()));
    }

    /// Letterboxes the picture to its aspect ratio (true) or stretches it
    /// over the whole widget (false), as
    /// `gtk4::Picture::set_keep_aspect_ratio`. A pan under way ends.
    pub fn set_keep_aspect_ratio(&self, keep: bool) {
        let changed = self.imp().picture.is_keep_aspect_ratio() != keep;
        self.end_pan();
        self.imp().picture.set_keep_aspect_ratio(keep);
        self.update_cursor();
        self.queue_draw();
        // The inner picture only redraws; what is placed by `picture_bounds`
        // (the fullscreen controls) is laid out again when it changes, not on
        // every other setting saved with it.
        if changed {
            self.queue_resize();
        }
    }

    /// Where the picture shows in this widget, in whole pixels: letterboxed
    /// or stretched as `set_keep_aspect_ratio` says, and the same zoomed or
    /// not (zoom crops inside it). None without a picture, before its first
    /// frame (a new stream's size is not known yet) or without a size.
    pub fn picture_bounds(&self) -> Option<gdk::Rectangle> {
        let aspect = self.paintable()?.intrinsic_aspect_ratio();
        if !(aspect.is_finite() && aspect > 0.0) {
            return None;
        }
        let rect = self.picture_rect()?;
        let finite = [rect.x, rect.y, rect.width, rect.height].iter().all(|v| v.is_finite());
        (finite && rect.width >= 1.0 && rect.height >= 1.0)
            .then(|| gdk::Rectangle::new(rect.x as i32, rect.y as i32, rect.width as i32, rect.height as i32))
    }

    /// While zoomed the paintable is drawn by this widget, not by the inner
    /// picture, whose own redraw requests then stop at it: a new frame (or
    /// size) of `paintable` has to redraw this widget itself. The handlers
    /// hold this widget weakly and go with the next paintable or on dispose.
    fn watch(&self, paintable: Option<&gdk::Paintable>) {
        if let Some((old, handlers)) = self.imp().watched.take() {
            for handler in handlers {
                old.disconnect(handler);
            }
        }
        let Some(paintable) = paintable else { return };
        let redraw = |weak: glib::WeakRef<Self>| {
            move |_: &gdk::Paintable| {
                // Unzoomed, the inner picture redraws on its own.
                if let Some(obj) = weak.upgrade().filter(|obj| obj.imp().zoom.get() != Zoom::default()) {
                    obj.queue_draw();
                }
            }
        };
        let contents = paintable.connect_invalidate_contents(redraw(self.downgrade()));
        let size = paintable.connect_invalidate_size(redraw(self.downgrade()));
        *self.imp().watched.borrow_mut() = Some((paintable.clone(), [contents, size]));
    }

    fn reset_zoom(&self) {
        self.imp().drag.set(None);
        self.imp().zoom.set(Zoom::default());
        self.update_cursor();
        self.queue_draw();
    }

    fn begin_pan(&self, x: f64, y: f64) -> bool {
        self.imp().drag.set(None);
        let zoom = self.imp().zoom.get();
        let rect = self.picture_rect().filter(|r| r.position(x, y).is_some());
        if zoom.scale > 1.0 {
            if let Some(rect) = rect {
                self.imp().drag.set(Some(PanDrag { zoom, rect }));
            }
        }
        self.update_cursor();
        self.imp().drag.get().is_some()
    }

    fn update_pan(&self, dx: f64, dy: f64) -> bool {
        let Some(drag) = self.imp().drag.get() else {
            return false;
        };
        if self.picture_rect() != Some(drag.rect) {
            self.end_pan();
            return false;
        }
        if !dx.is_finite() || !dy.is_finite() {
            return false;
        }
        // GTK reports total displacement from the press, not a delta from
        // the previous update. Always start from the original crop.
        let mut zoom = drag.zoom;
        zoom.pan(dx / drag.rect.width, dy / drag.rect.height);
        self.imp().zoom.set(zoom);
        self.queue_draw();
        dx != 0.0 || dy != 0.0
    }

    fn end_pan(&self) {
        self.imp().drag.set(None);
        self.update_cursor();
    }

    fn update_cursor(&self) {
        let over_picture = self
            .imp()
            .pointer
            .get()
            .is_some_and(|(x, y)| self.picture_rect().and_then(|r| r.position(x, y)).is_some());
        let name = cursor_name(self.imp().drag.get().is_some(), self.imp().zoom.get().scale > 1.0, over_picture);
        // Called on every motion event: a cursor only when it changes.
        if cursor_changes(&self.imp().cursor, name) {
            self.set_cursor_from_name(name);
        }
    }

    /// Where the pointer is for the event `controller` is handling, in this
    /// widget's coordinates. A GTK 4 scroll event carries no position of its
    /// own, so the pointer is asked where it is now: the live position on X11,
    /// on Wayland the compositor's answer (`wayland::pointer_position`), else
    /// that of the last pointer event. All are relative to the surface, which
    /// can extend beyond the native widget (client-side decorations).
    fn pointer_position(&self, controller: &impl IsA<gtk4::EventController>) -> Option<(f64, f64)> {
        let event = controller.as_ref().current_event()?;
        let native = self.native()?;
        let (x, y) = match event.position() {
            Some(position) => position,
            None => {
                let (surface, seat) = (native.surface()?, event.seat()?);
                match wayland::pointer_position(&surface, &seat) {
                    Some(position) => position,
                    None => {
                        // The seat's logical pointer: X11 refuses a position
                        // query for the physical device an event can name.
                        let (x, y, _) = surface.device_position(&seat.pointer()?)?;
                        (x, y)
                    }
                }
            }
        };
        let (dx, dy) = native.surface_transform();
        native.upcast_ref::<gtk4::Widget>().translate_coordinates(self, x - dx, y - dy)
    }

    fn picture_rect(&self) -> Option<PictureRect> {
        let paintable = self.paintable()?;
        PictureRect::fit(
            f64::from(self.width()),
            f64::from(self.height()),
            paintable.intrinsic_aspect_ratio(),
            self.imp().picture.is_keep_aspect_ratio(),
        )
    }

    fn scroll_at(&self, dy: f64, x: f64, y: f64) -> bool {
        let Some((x, y)) = self.picture_rect().and_then(|r| r.position(x, y)) else {
            return false;
        };
        let mut zoom = self.imp().zoom.get();
        zoom.scroll(dy, x, y);
        self.imp().drag.set(None);
        self.imp().zoom.set(zoom);
        self.update_cursor();
        self.queue_draw();
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Run separately on a display (or with GDK_BACKEND=broadway and a
    // connected gtk4-broadwayd): cargo test --bin reoling
    // zoom_picture::tests::widget_renders_and_resets -- --ignored
    // `gtk4::test` runs every GTK test on one thread, the only one GTK can be
    // initialised on, so it can run together with the other window tests.
    #[gtk4::test]
    #[ignore = "requires a GTK display"]
    fn widget_renders_and_resets() {
        gtk4::init().unwrap();
        let picture = ZoomPicture::new();
        // A coloured gradient makes both the anchored point and the crop
        // observable in the rendered pixels, without a camera connection.
        let pixels: Vec<u8> = (0..100)
            .flat_map(|y| (0..200).flat_map(move |x| [x as u8, y as u8, 0, 255]))
            .collect();
        let texture = gdk::MemoryTexture::new(
            200,
            100,
            gdk::MemoryFormat::R8g8b8a8,
            &glib::Bytes::from_owned(pixels),
            800,
        );
        picture.set_paintable(Some(&texture));
        let window = gtk4::Window::builder()
            .decorated(false)
            .default_width(400)
            .default_height(300)
            .child(&picture)
            .build();
        window.present();
        settle(&picture, 400, 300);
        let renderer = gtk4::gsk::CairoRenderer::new();
        renderer.realize(None::<&gdk::Surface>).unwrap();
        let render = || {
            let snapshot = gtk4::Snapshot::new();
            picture.imp().snapshot(&snapshot);
            let node = snapshot.to_node().unwrap();
            let texture =
                renderer.render_texture(&node, Some(&graphene::Rect::new(0.0, 0.0, 400.0, 300.0)));
            let mut pixels = vec![0; 400 * 300 * 4];
            texture.download(&mut pixels, 400 * 4);
            pixels
        };
        let original = render();
        assert_eq!(original[(10 * 400 + 100) * 4 + 3], 0, "letterbox is empty");
        assert_eq!(original[(100 * 400 + 100) * 4 + 3], 255, "picture is drawn");

        // The picture's place, as the fullscreen controls take it: the same
        // zoomed or not (checked right below).
        let bounds = Some(gdk::Rectangle::new(0, 50, 400, 200));
        assert_eq!(picture.picture_bounds(), bounds);

        // Exercise the viewport operations and rendered pixels directly.
        // Mouse event routing and fullscreen gestures need a manual check.
        assert!(picture.scroll_at(-3.0, 100.0, 100.0));
        assert_eq!(picture.picture_bounds(), bounds, "zoom crops inside the picture");
        let zoomed = render();
        assert_ne!(original, zoomed);
        let anchor = (100 * 400 + 100) * 4;
        for i in 0..4 {
            assert!(original[anchor + i].abs_diff(zoomed[anchor + i]) <= 1);
        }
        assert_eq!(
            zoomed[(10 * 400 + 100) * 4 + 3],
            0,
            "zoom stays inside the picture"
        );
        assert!(!picture.scroll_at(-1.0, 100.0, 10.0));
        assert!(!picture.begin_pan(100.0, 10.0));
        assert_eq!(render(), zoomed, "letterbox input does nothing");

        assert!(picture.begin_pan(100.0, 100.0));
        assert!(picture.update_pan(40.0, 20.0));
        let panned = render();
        assert_ne!(panned, zoomed);
        let moved_anchor = (120 * 400 + 140) * 4;
        for i in 0..4 {
            assert!(panned[moved_anchor + i].abs_diff(zoomed[anchor + i]) <= 1);
        }
        picture.update_pan(40.0, 20.0);
        assert_eq!(render(), panned, "offsets are relative to the press");
        picture.update_pan(0.0, 0.0);
        assert_eq!(render(), zoomed, "returning to the press restores the crop");
        picture.update_pan(40.0, 20.0);
        picture.end_pan();
        assert!(!picture.update_pan(80.0, 40.0));
        assert_eq!(render(), panned, "release keeps the final crop");

        picture.begin_pan(100.0, 100.0);
        picture.reset_zoom();
        assert!(!picture.update_pan(80.0, 40.0));
        assert_eq!(render(), original, "reset cancels an active drag");
        assert!(!picture.begin_pan(100.0, 100.0));
        assert!(picture.scroll_at(-3.0, 100.0, 100.0));
        picture.set_paintable(Some(&texture));
        assert_eq!(render(), original, "a new stream resets zoom");
        // Zoomed, new frames redraw the widget through handlers on the
        // paintable; each new paintable takes them over.
        let invalidate =
            glib::subclass::SignalId::lookup("invalidate-contents", gdk::Paintable::static_type()).unwrap();
        let watched = |paintable: &gdk::MemoryTexture| {
            glib::signal::signal_has_handler_pending(paintable, invalidate, None, true)
        };
        assert!(watched(&texture));
        assert!(picture.scroll_at(-3.0, 100.0, 100.0));
        picture.set_paintable(None::<&gdk::Paintable>);
        assert!(!watched(&texture), "no handler stays on a previous paintable");
        assert_eq!(picture.imp().zoom.get(), Zoom::default());
        assert!(!picture.scroll_at(-1.0, 100.0, 100.0));
        picture.set_paintable(Some(&texture));
        picture.set_keep_aspect_ratio(false);
        // Process the allocation queued by the changed picture fit.
        picture.allocate(400, 300, -1, None);
        assert_eq!(
            render()[(10 * 400 + 100) * 4 + 3],
            255,
            "stretch fills the viewport"
        );

        // Motion over a zoomed picture keeps the cursor it has; only a change
        // of cursor sets a new one.
        assert!(picture.scroll_at(-3.0, 100.0, 100.0));
        picture.imp().pointer.set(Some((100.0, 100.0)));
        picture.update_cursor();
        let grab = picture.cursor();
        assert_eq!(grab.as_ref().and_then(|cursor| cursor.name()).as_deref(), Some("grab"));
        picture.imp().pointer.set(Some((120.0, 110.0)));
        picture.update_cursor();
        assert_eq!(picture.cursor(), grab, "the same cursor object");
        assert!(picture.begin_pan(120.0, 110.0));
        assert_eq!(picture.cursor().and_then(|cursor| cursor.name()).as_deref(), Some("grabbing"));
        picture.end_pan();
        picture.reset_zoom();
        assert_eq!(picture.cursor(), None);

        // Unzoomed, the widget draws exactly like a plain GtkPicture, also
        // where the letterbox or the pillarbox falls between pixels. Zoomed
        // one step about the centre and at 16× panned to either corner, the
        // picture covers every pixel row and column of its rectangle wholly
        // and none outside it, as the plain picture does from GTK 4.14 on: no
        // empty strip and no edge blended with the background. From either
        // pan limit a drag pans back, also from a press near the opposite
        // corner.
        for (width, height) in [(160, 90), (90, 160)] {
            let pixels: Vec<u8> = (0..height)
                .flat_map(|y| (0..width).flat_map(move |x| [(x * 7) as u8, (y * 5) as u8, 128, 255]))
                .collect();
            let pattern = gdk::MemoryTexture::new(
                width,
                height,
                gdk::MemoryFormat::R8g8b8a8,
                &glib::Bytes::from_owned(pixels),
                width as usize * 4,
            );
            for keep in [true, false] {
                let plain = gtk4::Picture::new();
                plain.set_paintable(Some(&pattern));
                plain.set_keep_aspect_ratio(keep);
                let zoomed = ZoomPicture::new();
                zoomed.set_paintable(Some(&pattern));
                zoomed.set_keep_aspect_ratio(keep);
                let windows = [plain.upcast_ref::<gtk4::Widget>(), zoomed.upcast_ref()].map(|child| {
                    let window =
                        gtk4::Window::builder().decorated(false).default_width(401).default_height(301).build();
                    window.set_child(Some(child));
                    window.present();
                    window
                });
                settle(&plain, 401, 301);
                settle(&zoomed, 401, 301);
                let case = format!("{width}x{height} picture, keep aspect ratio {keep}");
                let unzoomed = pixels_of(&zoomed, &renderer);
                let plain_pixels = pixels_of(&plain, &renderer);
                assert!(plain_pixels == unzoomed, "1× like a plain picture: {case}");

                let rect = zoomed.picture_rect().unwrap();
                let (cx, cy) = (rect.x + rect.width / 2.0, rect.y + rect.height / 2.0);
                let (right, bottom) = (rect.x + rect.width - 2.0, rect.y + rect.height - 2.0);
                assert!(zoomed.scroll_at(-1.0, cx, cy));
                let step = pixels_of(&zoomed, &renderer);
                zoomed.reset_zoom();
                assert!(zoomed.scroll_at(-100.0, rect.x, rect.y));
                assert_eq!(zoomed.imp().zoom.get(), Zoom { scale: MAX_ZOOM, x: 0.0, y: 0.0 });
                let top_left = pixels_of(&zoomed, &renderer);
                assert!(zoomed.begin_pan(right, bottom));
                assert!(zoomed.update_pan(-30.0, -20.0));
                let zoom = zoomed.imp().zoom.get();
                assert!(zoom.x < 0.0 && zoom.y < 0.0, "pans back from the top-left limit: {case}");
                zoomed.update_pan(-1e6, -1e6);
                zoomed.end_pan();
                assert_eq!(zoomed.imp().zoom.get(), Zoom { scale: MAX_ZOOM, x: 1.0 - MAX_ZOOM, y: 1.0 - MAX_ZOOM });
                let bottom_right = pixels_of(&zoomed, &renderer);
                assert!(zoomed.begin_pan(rect.x + 2.0, rect.y + 2.0));
                assert!(zoomed.update_pan(30.0, 20.0));
                let zoom = zoomed.imp().zoom.get();
                assert!(
                    zoom.x > 1.0 - MAX_ZOOM && zoom.y > 1.0 - MAX_ZOOM,
                    "pans back from the bottom-right limit: {case}"
                );
                zoomed.end_pan();

                let alpha = |pixels: &[u8], x: usize, y: usize| pixels[(y * 401 + x) * 4 + 3];
                let inside = |i: usize, start: f64, length: f64| (start..start + length).contains(&(i as f64));
                let renders = [
                    ("1× plain", &plain_pixels),
                    ("1.2×", &step),
                    ("16× top-left", &top_left),
                    ("16× bottom-right", &bottom_right),
                ];
                for (label, pixels) in renders {
                    // `PictureRect::fit` matches GtkPicture 4.14 and newer.
                    if label == "1× plain" && gtk4::check_version(4, 14, 0).is_some() {
                        continue;
                    }
                    for y in 0..301 {
                        let expected = if inside(y, rect.y, rect.height) { 255 } else { 0 };
                        assert_eq!(alpha(pixels, cx as usize, y), expected, "row {y} at {label}: {case}");
                    }
                    for x in 0..401 {
                        let expected = if inside(x, rect.x, rect.width) { 255 } else { 0 };
                        assert_eq!(alpha(pixels, x, cy as usize), expected, "column {x} at {label}: {case}");
                    }
                }
                for window in windows {
                    window.close();
                }
            }
        }
        renderer.unrealize();

        // Nothing but its window keeps the widget alive (the handlers hold it
        // weakly), and its handlers go with it.
        picture.set_paintable(Some(&texture));
        assert!(watched(&texture));
        let weak = picture.downgrade();
        drop(picture);
        window.close();
        drop(window);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while weak.upgrade().is_some() && std::time::Instant::now() < deadline {
            pump();
        }
        assert!(weak.upgrade().is_none(), "the widget is freed with its window");
        assert!(!watched(&texture), "its handlers are gone");
    }

    /// Runs what the main loop has pending, then waits 10 ms.
    fn pump() {
        let context = glib::MainContext::default();
        while context.pending() {
            context.iteration(false);
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    /// Runs the main loop until `widget` has its size, for up to 5 s: a
    /// release build under Xvfb can take a while to map a first window.
    fn settle(widget: &impl IsA<gtk4::Widget>, width: i32, height: i32) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while (widget.width(), widget.height()) != (width, height) && std::time::Instant::now() < deadline {
            pump();
        }
        pump();
        assert_eq!((widget.width(), widget.height()), (width, height));
    }

    /// The pixels of `widget` as its window draws it, at once rather than on
    /// the next frame.
    fn pixels_of(widget: &impl IsA<gtk4::Widget>, renderer: &gtk4::gsk::CairoRenderer) -> Vec<u8> {
        let window = widget.parent().unwrap();
        let bounds = widget.compute_bounds(&window).unwrap();
        let snapshot = gtk4::Snapshot::new();
        window.snapshot_child(widget, &snapshot);
        let texture = renderer.render_texture(&snapshot.to_node().unwrap(), Some(&bounds));
        let (width, height) = (texture.width() as usize, texture.height() as usize);
        let mut pixels = vec![0; width * height * 4];
        texture.download(&mut pixels, width * 4);
        pixels
    }

    fn near(a: f64, b: f64) {
        assert!((a - b).abs() < 1e-9, "{a} != {b}");
    }

    #[test]
    fn panning_moves_with_the_pointer_and_clamps_to_each_edge() {
        let mut zoom = Zoom {
            scale: 3.0,
            x: -1.0,
            y: -1.0,
        };
        zoom.pan(0.25, -0.5);
        assert_eq!(
            zoom,
            Zoom {
                scale: 3.0,
                x: -0.75,
                y: -1.5
            }
        );
        zoom.pan(10.0, -10.0);
        assert_eq!((zoom.x, zoom.y), (0.0, -2.0));
        zoom.pan(-10.0, 10.0);
        assert_eq!((zoom.x, zoom.y), (-2.0, 0.0));
        let mut full = Zoom::default();
        full.pan(0.5, -0.5);
        assert_eq!(full, Zoom::default());
    }

    #[test]
    fn zoom_keeps_the_source_point_under_each_new_pointer_position() {
        let mut zoom = Zoom::default();
        for (dy, x, y) in [(-3.0, 0.2, 0.7), (-2.0, 0.8, 0.1), (1.0, 0.5, 0.5)] {
            let previous_scale = zoom.scale;
            let source_x = (x - zoom.x) / zoom.scale;
            let source_y = (y - zoom.y) / zoom.scale;
            zoom.scroll(dy, x, y);
            if dy < 0.0 {
                assert!(zoom.scale > previous_scale, "wheel up zooms in");
            } else {
                assert!(zoom.scale < previous_scale, "wheel down zooms out");
            }
            near(source_x * zoom.scale + zoom.x, x);
            near(source_y * zoom.scale + zoom.y, y);
        }
    }

    #[test]
    fn zoom_limits_and_zooming_out_restore_the_whole_picture() {
        let mut zoom = Zoom::default();
        zoom.scroll(-1000.0, 0.25, 0.75);
        assert_eq!(zoom.scale, MAX_ZOOM);
        let at_limit = zoom;
        zoom.scroll(-1.0, 0.9, 0.1);
        assert_eq!(zoom, at_limit);
        zoom.scroll(1000.0, 0.9, 0.1);
        assert_eq!(zoom, Zoom::default());
        zoom.scroll(1.0, 0.9, 0.1);
        assert_eq!(zoom, Zoom::default());
        zoom.scroll(-1.0, 0.4, 0.6);
        zoom.scroll(1.0, 0.4, 0.6);
        assert_eq!(zoom, Zoom::default());
    }

    #[test]
    fn zooming_out_at_another_edge_never_exposes_empty_space() {
        for (x, y) in [(0.0, 0.0), (1.0, 1.0), (0.0, 1.0), (1.0, 0.0)] {
            let mut zoom = Zoom::default();
            zoom.scroll(-10.0, x, y);
            zoom.scroll(5.0, 1.0 - x, 1.0 - y);
            assert!(zoom.x <= 0.0 && zoom.x + zoom.scale >= 1.0);
            assert!(zoom.y <= 0.0 && zoom.y + zoom.scale >= 1.0);
        }
    }

    #[test]
    fn pointer_coordinates_account_for_letterboxing_and_stretch() {
        let wide = PictureRect::fit(800.0, 600.0, 2.0, true).unwrap();
        assert_eq!(wide.position(400.0, 300.0), Some((0.5, 0.5)));
        assert_eq!(wide.position(200.0, 200.0), Some((0.25, 0.25)));
        assert_eq!(wide.position(400.0, 99.0), None);
        let tall = PictureRect::fit(800.0, 600.0, 0.5, true).unwrap();
        assert_eq!(tall.position(250.0, 0.0), Some((0.0, 0.0)));
        assert_eq!(tall.position(249.0, 300.0), None);
        // Fits at fractions of a pixel round as GtkPicture's do: the size
        // up, the offset down.
        let fit = |width, height, aspect| {
            let rect = PictureRect::fit(width, height, aspect, true).unwrap();
            (rect.x, rect.y, rect.width, rect.height)
        };
        assert_eq!(fit(934.0, 560.0, 16.0 / 9.0), (0.0, 17.0, 934.0, 526.0));
        assert_eq!(fit(401.0, 301.0, 16.0 / 9.0), (0.0, 37.0, 401.0, 226.0));
        assert_eq!(fit(1001.0, 600.0, 9.0 / 16.0), (331.0, 0.0, 338.0, 600.0));
        let stretched = PictureRect::fit(800.0, 600.0, 2.0, false).unwrap();
        assert_eq!(stretched.position(200.0, 150.0), Some((0.25, 0.25)));
        assert!(PictureRect::fit(0.0, 600.0, 2.0, true).is_none());
    }

    #[test]
    fn a_wheel_step_zooms_about_where_the_event_happened() {
        // The last motion position is stale after fullscreen came in under a
        // still pointer. Only the fallback order is tested here: the lookup
        // in `pointer_position` needs a pointer on a display and is checked
        // outside the test suite.
        assert_eq!(scroll_anchor(Some((520.0, 230.0)), Some((259.0, 184.0))), Some((520.0, 230.0)));
        assert_eq!(scroll_anchor(None, Some((259.0, 184.0))), Some((259.0, 184.0)));
        assert_eq!(scroll_anchor(Some((520.0, 230.0)), None), Some((520.0, 230.0)));
        assert_eq!(scroll_anchor(None, None), None);
    }

    #[test]
    fn the_cursor_is_set_only_when_it_changes() {
        assert_eq!(cursor_name(false, false, true), None);
        assert_eq!(cursor_name(false, true, false), None);
        assert_eq!(cursor_name(false, true, true), Some("grab"));
        assert_eq!(cursor_name(true, true, false), Some("grabbing"));
        assert_eq!(cursor_name(true, true, true), Some("grabbing"));
        // A widget starts with no cursor of its own.
        let shown = Cell::new(None);
        let names = [None, Some("grab"), Some("grab"), Some("grabbing"), Some("grabbing"), Some("grab"), None, None];
        let changes: Vec<bool> = names.into_iter().map(|name| cursor_changes(&shown, name)).collect();
        assert_eq!(changes, [false, true, false, true, false, true, true, false]);
        assert_eq!(shown.get(), None);
    }
}
