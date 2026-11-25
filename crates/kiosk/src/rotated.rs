//! A container that shows its one child turned clockwise by 0/90/180/270 degrees. GTK maps touch, clicks and
//! WebKit's coordinates back through the transform, so cage, the screen and the touch mapping stay untouched.
//! Known limit: native popovers (a page's <select> list) open unrotated.

use gtk::subclass::prelude::*;
use gtk::{glib, graphene, gsk, prelude::*};

mod imp {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(Default)]
    pub struct Rotated {
        pub degrees: Cell<u16>,
        pub child: RefCell<Option<gtk::Widget>>,
        /// set when the transform did not fit and the child is shown unrotated
        pub failed: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Rotated {
        const NAME: &'static str = "WtsRotated";
        type Type = super::Rotated;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for Rotated {
        fn dispose(&self) {
            if let Some(child) = self.child.take() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for Rotated {
        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let Some(child) = self.child.borrow().clone() else { return (0, 0, -1, -1) };
            let o = match (super::swaps(self.degrees.get()), orientation) {
                (true, gtk::Orientation::Horizontal) => gtk::Orientation::Vertical,
                (true, _) => gtk::Orientation::Horizontal,
                (false, o) => o,
            };
            let (min, nat, _, _) = child.measure(o, -1);
            (min, nat, -1, -1)
        }

        fn size_allocate(&self, width: i32, height: i32, _baseline: i32) {
            let Some(child) = self.child.borrow().clone() else { return };
            let d = self.degrees.get();
            let (w, h) = if super::swaps(d) { (height, width) } else { (width, height) };
            let transform = super::transform(d, w, h).filter(|t| {
                // the turned child must cover exactly the screen, else something in GSK differs from the math
                let b = t.transform_bounds(&graphene::Rect::new(0.0, 0.0, w as f32, h as f32));
                let ok = [b.x(), b.y(), b.width(), b.height()].map(|v| v.round() as i32) == [0, 0, width, height];
                if !ok && !self.failed.replace(true) {
                    println!("Drehung {d}°: Transformation passt nicht, zeige ungedreht");
                }
                ok
            });
            match transform {
                Some(t) => child.allocate(w, h, -1, Some(t)),
                None => child.allocate(width, height, -1, None),
            }
        }
    }
}

glib::wrapper! {
    pub struct Rotated(ObjectSubclass<imp::Rotated>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl Rotated {
    pub fn new(child: &impl IsA<gtk::Widget>) -> Rotated {
        let r: Rotated = glib::Object::new();
        child.set_parent(&r);
        r.imp().child.replace(Some(child.clone().upcast()));
        r
    }

    /// 0, 90, 180 or 270; anything else counts as 0.
    pub fn set_degrees(&self, degrees: u16) {
        let d = if [90, 180, 270].contains(&degrees) { degrees } else { 0 };
        if self.imp().degrees.replace(d) != d {
            self.imp().failed.set(false);
            self.queue_resize(); // not queue_allocate: width and height swap
        }
    }

    pub fn failed(&self) -> bool {
        self.imp().failed.get()
    }
}

fn swaps(d: u16) -> bool {
    d == 90 || d == 270
}

/// Child of size w x h turned by d degrees clockwise around its top-left corner, then moved back on screen.
fn transform(d: u16, w: i32, h: i32) -> Option<gsk::Transform> {
    let (x, y) = match d {
        90 => (h, 0),
        180 => (w, h),
        270 => (0, w),
        _ => return None,
    };
    Some(gsk::Transform::new().translate(&graphene::Point::new(x as f32, y as f32)).rotate(f32::from(d)))
}
