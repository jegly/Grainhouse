//! The Retouch panel: click a spot on the photo to heal, clone or remove it.

use super::*;
use grainhouse::retouch::{self, Spot};

pub struct RetouchUi {
    pub group: adw::PreferencesGroup,
    pub mode: adw::ComboRow,
    pub size: Slider,
    pub feather: Slider,
    pub opacity: Slider,
    pub on: adw::SwitchRow,
    pub undo: gtk::Button,
    pub clear: gtk::Button,
    pub area: gtk::DrawingArea,
}

pub fn build() -> RetouchUi {
    let group = adw::PreferencesGroup::builder().title("Retouch").description("Click a spot on the photo to cover it with a matching patch from nearby").build();
    let mode = combo("Tool", &retouch::MODES, 0);
    mode.set_subtitle("Heal matches the light, Clone copies exactly, Remove picks the best patch");
    let size = slider("Size", 0.5, 12.0, 0.1, 2.0, 1);
    size.set_subtitle("Radius, as a percent of the picture's long edge");
    let feather = slider("Feather", 0.0, 100.0, 1.0, 50.0, 0);
    let opacity = slider("Opacity", 5.0, 100.0, 1.0, 100.0, 0);
    let on = switch("Retouch on the photo", "Click to place a spot");
    let undo = gtk::Button::builder().label("Undo last spot").valign(gtk::Align::Center).build();
    let clear = gtk::Button::builder().label("Clear all").valign(gtk::Align::Center).build();
    let row = adw::ActionRow::builder().title("Spots").build();
    row.add_suffix(&undo);
    row.add_suffix(&clear);
    group.add(&mode);
    group.add(&size.row);
    group.add(&feather.row);
    group.add(&opacity.row);
    group.add(&on);
    group.add(&row);
    let area = gtk::DrawingArea::builder().hexpand(true).vexpand(true).can_target(false).build();
    RetouchUi { group, mode, size, feather, opacity, on, undo, clear, area }
}

impl App {
    pub fn spots_changed(&self) {
        self.ui.rui.area.queue_draw();
        self.request_render();
    }

    fn draw_retouch(&self, cr: &gtk::cairo::Context) {
        let Some((ox, oy, dw, dh)) = self.shown_rect() else { return };
        if !self.ui.rui.on.is_active() {
            return;
        }
        cr.set_line_width(1.5);
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.85);
        for s in self.spots.borrow().iter() {
            cr.arc(ox + s.x as f64 * dw, oy + s.y as f64 * dh, s.radius as f64 * dw.max(dh), 0.0, std::f64::consts::TAU);
            let _ = cr.stroke();
        }
        if let Some((hx, hy)) = self.brush_hover.get() {
            cr.set_source_rgba(1.0, 0.9, 0.2, 0.95);
            cr.arc(hx, hy, self.ui.rui.size.value() / 100.0 * dw.max(dh), 0.0, std::f64::consts::TAU);
            let _ = cr.stroke();
        }
    }
}

pub fn connect(app: &Rc<App>) {
    let r = &app.ui.rui;
    {
        let a = Rc::downgrade(app);
        r.area.set_draw_func(move |_, cr, _, _| {
            if let Some(a) = a.upgrade() {
                a.draw_retouch(cr);
            }
        });
        let a = Rc::downgrade(app);
        r.on.connect_active_notify(move |sw| {
            let Some(a) = a.upgrade() else { return };
            if sw.is_active() {
                a.set_zoom(1.0);
                a.ui.mui.edit.set_active(false);
                if a.ui.crop_button.is_active() {
                    a.ui.crop_button.set_active(false);
                }
            }
            a.ui.rui.area.set_can_target(sw.is_active());
            a.ui.rui.area.queue_draw();
        });
        let click = gtk::GestureClick::new();
        let a = Rc::downgrade(app);
        click.connect_pressed(move |_, _, x, y| {
            let Some(a) = a.upgrade() else { return };
            let Some((ox, oy, dw, dh)) = a.shown_rect() else { return };
            let (fx, fy) = (((x - ox) / dw) as f32, ((y - oy) / dh) as f32);
            if !(0.0..=1.0).contains(&fx) || !(0.0..=1.0).contains(&fy) {
                return;
            }
            let u = &a.ui.rui;
            let spot = Spot {
                x: fx,
                y: fy,
                radius: (u.size.value() / 100.0) as f32,
                feather: (u.feather.value() / 100.0) as f32,
                opacity: (u.opacity.value() / 100.0) as f32,
                mode: u.mode.selected() as usize,
                ..Default::default()
            };
            a.spots.borrow_mut().push(spot);
            a.spots_changed();
        });
        r.area.add_controller(click);
        let motion = gtk::EventControllerMotion::new();
        let a = Rc::downgrade(app);
        motion.connect_motion(move |_, x, y| {
            if let Some(a) = a.upgrade() {
                a.brush_hover.set(Some((x, y)));
                a.ui.rui.area.queue_draw();
            }
        });
        r.area.add_controller(motion);
    }
    {
        let a = Rc::downgrade(app);
        r.undo.connect_clicked(move |_| {
            if let Some(a) = a.upgrade() {
                a.spots.borrow_mut().pop();
                a.spots_changed();
            }
        });
        let a = Rc::downgrade(app);
        r.clear.connect_clicked(move |_| {
            if let Some(a) = a.upgrade() {
                a.spots.borrow_mut().clear();
                a.spots_changed();
            }
        });
    }
}
