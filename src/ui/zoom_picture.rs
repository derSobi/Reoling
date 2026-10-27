//! Digital zoom for live and recorded video. Only the displayed picture is
//! transformed; snapshots and recordings still use the original frames.

use gtk4::{gdk, graphene, prelude::*, subclass::prelude::*};
use std::cell::Cell;

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
    fn fit(width: f64, height: f64, aspect: f64, keep_aspect: bool) -> Option<Self> {
        if width <= 0.0 || height <= 0.0 {
            return None;
        }
        let (w, h) = if keep_aspect && aspect > 0.0 {
            let w = width.min(height * aspect);
            (w, w / aspect)
        } else {
            (width, height)
        };
        Some(Self {
            x: (width - w) / 2.0,
            y: (height - h) / 2.0,
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

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct ZoomPicture {
        pub picture: gtk4::Picture,
        pub(super) zoom: Cell<Zoom>,
        pub(super) pointer: Cell<Option<(f64, f64)>>,
        pub(super) drag: Cell<Option<PanDrag>>,
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
            scroll.connect_scroll(move |_, _, dy| {
                if let Some(obj) = weak.upgrade() {
                    if let Some((x, y)) = obj.imp().pointer.get() {
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
                    if obj.update_pan(dx, dy) {
                        // Claim only after movement, leaving a stationary
                        // double-click available for fullscreen.
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
            self.picture.unparent();
        }
    }

    impl WidgetImpl for ZoomPicture {
        fn snapshot(&self, snapshot: &gtk4::Snapshot) {
            let obj = self.obj();
            let Some(rect) = obj.picture_rect() else {
                return;
            };
            let zoom = self.zoom.get();
            snapshot.push_clip(&graphene::Rect::new(
                rect.x as f32,
                rect.y as f32,
                rect.width as f32,
                rect.height as f32,
            ));
            snapshot.save();
            snapshot.translate(&graphene::Point::new(
                (rect.x * (1.0 - zoom.scale) + zoom.x * rect.width) as f32,
                (rect.y * (1.0 - zoom.scale) + zoom.y * rect.height) as f32,
            ));
            snapshot.scale(zoom.scale as f32, zoom.scale as f32);
            obj.snapshot_child(&self.picture, snapshot);
            snapshot.restore();
            snapshot.pop();
        }
    }
}

glib::wrapper! {
    pub struct ZoomPicture(ObjectSubclass<imp::ZoomPicture>)
        @extends gtk4::Widget,
        @implements gtk4::Accessible, gtk4::Buildable, gtk4::ConstraintTarget;
}

impl ZoomPicture {
    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn paintable(&self) -> Option<gdk::Paintable> {
        self.imp().picture.paintable()
    }

    pub fn set_paintable(&self, paintable: Option<&impl IsA<gdk::Paintable>>) {
        self.reset_zoom();
        self.imp().picture.set_paintable(paintable);
    }

    pub fn set_keep_aspect_ratio(&self, keep: bool) {
        self.end_pan();
        self.imp().picture.set_keep_aspect_ratio(keep);
        self.update_cursor();
        self.queue_draw();
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
        self.set_cursor_from_name(if self.imp().drag.get().is_some() {
            Some("grabbing")
        } else if self.imp().zoom.get().scale > 1.0 && over_picture {
            Some("grab")
        } else {
            None
        });
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
    #[test]
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
        let context = glib::MainContext::default();
        for _ in 0..100 {
            while context.pending() {
                context.iteration(false);
            }
            if picture.width() > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!((picture.width(), picture.height()), (400, 300));
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

        // Exercise the viewport operations and rendered pixels directly.
        // Mouse event routing and fullscreen gestures need a manual check.
        assert!(picture.scroll_at(-3.0, 100.0, 100.0));
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
        assert!(picture.scroll_at(-3.0, 100.0, 100.0));
        picture.set_paintable(None::<&gdk::Paintable>);
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
        renderer.unrealize();
        window.close();
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
        let stretched = PictureRect::fit(800.0, 600.0, 2.0, false).unwrap();
        assert_eq!(stretched.position(200.0, 150.0), Some((0.25, 0.25)));
        assert!(PictureRect::fit(0.0, 600.0, 2.0, true).is_none());
    }
}
