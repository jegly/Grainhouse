//! The Masks panel: mask layers made of brush strokes, gradients, ranges and AI selections, with 17 adjustments
//! each, plus editing on the photo (paint with the brush, drag the pins of gradients).

use super::*;
use grainhouse::masks::{self, Component, MaskLayer, Stroke};

/// Which number of a component a slider edits, and how it is shown: label, min, max, scale to the stored value.
#[derive(Clone, Copy)]
enum Target {
    A(usize),
    P(usize),
}

struct Spec {
    label: &'static str,
    target: Target,
    min: f64,
    max: f64,
    scale: f64,
}

fn specs(kind: usize) -> Vec<Spec> {
    let s = |label, target, min, max, scale| Spec { label, target, min, max, scale };
    match kind {
        masks::BRUSH => vec![s("Density", Target::P(0), 0.0, 100.0, 100.0)],
        masks::LINEAR => vec![
            s("Start across", Target::A(0), 0.0, 100.0, 100.0),
            s("Start down", Target::A(1), 0.0, 100.0, 100.0),
            s("End across", Target::A(2), 0.0, 100.0, 100.0),
            s("End down", Target::A(3), 0.0, 100.0, 100.0),
            s("Feather", Target::P(0), 0.0, 100.0, 100.0),
        ],
        masks::RADIAL => vec![
            s("Centre across", Target::A(0), 0.0, 100.0, 100.0),
            s("Centre down", Target::A(1), 0.0, 100.0, 100.0),
            s("Width", Target::A(2), 1.0, 100.0, 100.0),
            s("Height", Target::A(3), 1.0, 100.0, 100.0),
            s("Feather", Target::P(0), 0.0, 100.0, 100.0),
            s("Rotation", Target::P(1), 0.0, 360.0, 360.0),
        ],
        masks::LUMINANCE => vec![
            s("Darkest", Target::P(0), 0.0, 100.0, 100.0),
            s("Brightest", Target::P(1), 0.0, 100.0, 100.0),
            s("Smoothness", Target::P(2), 0.0, 100.0, 100.0),
        ],
        masks::COLOUR => vec![
            s("Hue", Target::P(0), 0.0, 360.0, 1.0),
            s("Range", Target::P(1), 0.0, 100.0, 100.0),
            s("Minimum colour", Target::P(2), 0.0, 100.0, 100.0),
        ],
        masks::SKY => vec![s("Reach down", Target::P(0), 0.0, 100.0, 100.0)],
        _ => Vec::new(),
    }
}

pub struct MaskUi {
    pub group: adw::PreferencesGroup,
    pub layer: adw::ComboRow,
    pub add: gtk::Button,
    pub del: gtk::Button,
    pub visible: adw::SwitchRow,
    pub amount: Slider,
    pub comp: adw::ComboRow,
    pub add_comp: gtk::MenuButton,
    pub add_kind_buttons: Vec<gtk::Button>,
    pub del_comp: gtk::Button,
    pub mode: adw::ComboRow,
    pub invert: adw::SwitchRow,
    pub params: Vec<Slider>,
    pub brush_size: Slider,
    pub brush_feather: Slider,
    pub brush_flow: Slider,
    pub brush_erase: adw::SwitchRow,
    pub brush_clear: gtk::Button,
    pub brush_clear_row: adw::ActionRow,
    pub edit: adw::SwitchRow,
    pub adjust_row: adw::ExpanderRow,
    pub adjust: Vec<Slider>,
    /// draws the red selection and the pins over the photo, and takes the drags while editing
    pub area: gtk::DrawingArea,
}

pub fn build() -> MaskUi {
    let group = adw::PreferencesGroup::builder()
        .title("Masks")
        .description("Change just part of the photo: paint with a brush, drop gradients, or select by brightness, colour, sky or subject")
        .build();
    let layer = combo("Mask", &["No masks yet"], 0);
    let add = gtk::Button::builder().icon_name("list-add-symbolic").tooltip_text("New mask").valign(gtk::Align::Center).css_classes(["flat"]).build();
    let del = gtk::Button::builder().icon_name("user-trash-symbolic").tooltip_text("Delete this mask").valign(gtk::Align::Center).css_classes(["flat"]).build();
    layer.add_suffix(&add);
    layer.add_suffix(&del);
    let visible = switch("Show this mask's effect", "Turn off to compare without it");
    let amount = slider("Amount", 0.0, 100.0, 1.0, 100.0, 0);
    amount.set_subtitle("Overall strength of this mask");
    let edit = switch("Edit on the photo", "Shows the mask in red. Drag to paint or move pins");

    let comp = combo("Part", &["-"], 0);
    let add_comp = gtk::MenuButton::builder().icon_name("list-add-symbolic").tooltip_text("Add a part to this mask").valign(gtk::Align::Center).css_classes(["flat"]).build();
    let pop = gtk::Popover::new();
    let kinds_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    let mut add_kind_buttons = Vec::new();
    for name in masks::KINDS.iter() {
        let b = gtk::Button::builder().label(*name).css_classes(["flat"]).build();
        kinds_box.append(&b);
        add_kind_buttons.push(b);
    }
    pop.set_child(Some(&kinds_box));
    add_comp.set_popover(Some(&pop));
    let del_comp = gtk::Button::builder().icon_name("user-trash-symbolic").tooltip_text("Remove this part").valign(gtk::Align::Center).css_classes(["flat"]).build();
    comp.add_suffix(&add_comp);
    comp.add_suffix(&del_comp);
    let mode = combo("Combine", &masks::MODES, 0);
    mode.set_subtitle("How this part joins the parts before it");
    let invert = switch("Invert", "Select everything except this part");
    let mut params = Vec::new();
    for _ in 0..6 {
        params.push(slider("-", 0.0, 100.0, 1.0, 0.0, 0));
    }
    let brush_size = slider("Brush size", 1.0, 40.0, 1.0, 8.0, 0);
    let brush_feather = slider("Brush feather", 0.0, 100.0, 1.0, 50.0, 0);
    let brush_flow = slider("Brush flow", 5.0, 100.0, 1.0, 70.0, 0);
    let brush_erase = switch("Erase", "Paint away from the mask");
    let brush_clear = gtk::Button::builder().label("Clear strokes").valign(gtk::Align::Center).build();
    let clear_row = adw::ActionRow::builder().title("Brush strokes").build();
    clear_row.add_suffix(&brush_clear);

    let adjust_row = adw::ExpanderRow::builder().title("Adjustments").subtitle("What this mask changes").build();
    let mut adjust = Vec::new();
    for name in masks::ADJUSTMENTS.iter() {
        let sl = slider(name, -100.0, 100.0, 1.0, 0.0, 0);
        adjust_row.add_row(&sl.row);
        adjust.push(sl);
    }

    group.add(&layer);
    group.add(&visible);
    group.add(&amount.row);
    group.add(&edit);
    group.add(&comp);
    group.add(&mode);
    group.add(&invert);
    for p in &params {
        group.add(&p.row);
    }
    for r in [&brush_size.row, &brush_feather.row, &brush_flow.row] {
        group.add(r);
    }
    group.add(&brush_erase);
    group.add(&clear_row);
    group.add(&adjust_row);

    let area = gtk::DrawingArea::builder().hexpand(true).vexpand(true).can_target(false).build();
    MaskUi {
        group, layer, add, del, visible, amount, comp, add_comp, add_kind_buttons, del_comp, mode, invert, params, brush_size, brush_feather, brush_flow,
        brush_erase, brush_clear, brush_clear_row: clear_row.clone(), edit, adjust_row, adjust, area,
    }
}

impl App {
    fn with_layer<R>(&self, f: impl FnOnce(&mut MaskLayer) -> R) -> Option<R> {
        let i = self.mask_sel.get();
        self.masks.borrow_mut().get_mut(i).map(f)
    }

    /// For the screenshot tour.
    pub fn with_comp_tour(&self, f: impl FnOnce(&mut Component)) {
        self.with_comp(f);
        self.mask_refresh();
        self.request_render();
    }

    fn with_comp<R>(&self, f: impl FnOnce(&mut Component) -> R) -> Option<R> {
        let c = self.comp_sel.get();
        self.with_layer(|l| l.comps.get_mut(c).map(f)).flatten()
    }

    pub fn mask_changed(&self) {
        self.ui.mui.area.queue_draw();
        self.request_render();
    }

    /// Shows the selected mask and part in the panel.
    pub fn mask_refresh(&self) {
        let m = &self.ui.mui;
        let was = self.updating.replace(true);
        let masks = self.masks.borrow().clone();
        let names: Vec<String> = if masks.is_empty() { vec!["No masks yet".into()] } else { masks.iter().map(|l| l.name.clone()).collect() };
        let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        m.layer.set_model(Some(&gtk::StringList::new(&refs)));
        let sel = self.mask_sel.get().min(masks.len().saturating_sub(1));
        self.mask_sel.set(sel);
        m.layer.set_selected(sel as u32);
        let have = !masks.is_empty();
        m.visible.set_visible(have);
        m.amount.row.set_visible(have);
        m.edit.set_visible(have);
        m.comp.set_visible(have);
        m.adjust_row.set_visible(have);
        m.del.set_sensitive(have);
        if let Some(l) = masks.get(sel) {
            m.visible.set_active(l.visible);
            m.amount.set_value(l.amount as f64 * 100.0);
            for (i, sl) in m.adjust.iter().enumerate() {
                sl.set_value(l.adj.values[i] as f64 * 100.0);
            }
            let cn: Vec<String> = if l.comps.is_empty() {
                vec!["No parts yet: press +".into()]
            } else {
                l.comps.iter().enumerate().map(|(i, c)| format!("{}  {}", i + 1, masks::KINDS[c.kind.min(7)])).collect()
            };
            let cr: Vec<&str> = cn.iter().map(|s| s.as_str()).collect();
            m.comp.set_model(Some(&gtk::StringList::new(&cr)));
            let cs = self.comp_sel.get().min(l.comps.len().saturating_sub(1));
            self.comp_sel.set(cs);
            m.comp.set_selected(cs as u32);
        }
        self.updating.set(was);
        self.comp_refresh();
    }

    /// Shows the controls of the selected part.
    fn comp_refresh(&self) {
        let m = &self.ui.mui;
        let was = self.updating.replace(true);
        let comp = {
            let layers = self.masks.borrow();
            layers.get(self.mask_sel.get()).and_then(|l| l.comps.get(self.comp_sel.get()).cloned())
        };
        m.del_comp.set_sensitive(comp.is_some());
        m.mode.set_visible(comp.as_ref().is_some() && self.comp_sel.get() > 0);
        m.invert.set_visible(comp.is_some());
        m.edit.set_visible(self.mask_sel.get() < self.masks.borrow().len());
        let kind = comp.as_ref().map(|c| c.kind);
        let sp = kind.map(specs).unwrap_or_default();
        for (i, sl) in m.params.iter().enumerate() {
            match (sp.get(i), comp.as_ref()) {
                (Some(s), Some(c)) => {
                    sl.row.set_visible(true);
                    sl.row.set_title(s.label);
                    sl.scale.set_range(s.min, s.max);
                    let raw = match s.target {
                        Target::A(k) => c.a[k],
                        Target::P(k) => c.p[k],
                    };
                    sl.set_value(raw as f64 * s.scale);
                }
                _ => sl.row.set_visible(false),
            }
        }
        let brush = kind == Some(masks::BRUSH);
        m.brush_size.row.set_visible(brush);
        m.brush_feather.row.set_visible(brush);
        m.brush_flow.row.set_visible(brush);
        m.brush_erase.set_visible(brush);
        m.brush_clear_row.set_visible(brush);
        if let Some(c) = &comp {
            m.mode.set_selected(c.mode as u32);
            m.invert.set_active(c.invert);
        }
        self.updating.set(was);
        m.area.queue_draw();
    }

    fn mask_add(&self) {
        let n = self.masks.borrow().len() + 1;
        self.masks.borrow_mut().push(MaskLayer { name: format!("Mask {n}"), ..Default::default() });
        self.mask_sel.set(n - 1);
        self.comp_sel.set(0);
        self.mask_refresh();
        self.request_render();
    }

    pub fn comp_add(&self, kind: usize) {
        if self.masks.borrow().is_empty() {
            self.mask_add();
        }
        let c = Component::new(kind);
        let i = self.with_layer(|l| {
            let mut c = c;
            if !l.comps.is_empty() && matches!(c.kind, masks::BRUSH) {
                c.mode = 0;
            }
            l.comps.push(c);
            l.comps.len() - 1
        });
        if let Some(i) = i {
            self.comp_sel.set(i);
        }
        self.mask_refresh();
        self.request_render();
    }

    /// Where the photo sits in the overlay, as (x, y, w, h), when the whole photo is in view.
    fn mask_rect(&self) -> Option<(f64, f64, f64, f64)> {
        self.shown_rect()
    }

    fn draw_mask_overlay(&self, cr: &gtk::cairo::Context) {
        let m = &self.ui.mui;
        if !m.edit.is_active() {
            return;
        }
        let Some((ox, oy, dw, dh)) = self.mask_rect() else { return };
        let layer = self.masks.borrow().get(self.mask_sel.get()).cloned();
        let Some(layer) = layer else { return };
        // the selection in red
        if let Some(sample) = self.sample.borrow().as_ref() {
            let (pw, ph) = (160usize, ((160.0 * sample.h as f32 / sample.w as f32) as usize).max(2));
            let a = masks::preview(&layer, sample, None, pw, ph);
            if let Ok(mut surf) = gtk::cairo::ImageSurface::create(gtk::cairo::Format::ARgb32, pw as i32, ph as i32) {
                let stride = surf.stride() as usize;
                if let Ok(mut data) = surf.data() {
                    for y in 0..ph {
                        for x in 0..pw {
                            let al = (a[y * pw + x] as u32 * 130) / 255;
                            let px = (al << 24) | (al << 16);
                            data[y * stride + x * 4..y * stride + x * 4 + 4].copy_from_slice(&px.to_le_bytes());
                        }
                    }
                }
                surf.mark_dirty();
                let _ = cr.save();
                cr.translate(ox, oy);
                cr.scale(dw / pw as f64, dh / ph as f64);
                let pattern = gtk::cairo::SurfacePattern::create(&surf);
                pattern.set_filter(gtk::cairo::Filter::Bilinear);
                let _ = cr.set_source(&pattern);
                let _ = cr.paint();
                let _ = cr.restore();
            }
        }
        // pins of the selected part
        let Some(comp) = layer.comps.get(self.comp_sel.get()) else { return };
        let pin = |cr: &gtk::cairo::Context, fx: f32, fy: f32| {
            let (x, y) = (ox + fx as f64 * dw, oy + fy as f64 * dh);
            cr.set_line_width(3.0);
            cr.set_source_rgba(0.0, 0.0, 0.0, 0.7);
            cr.arc(x, y, 7.0, 0.0, std::f64::consts::TAU);
            let _ = cr.stroke();
            cr.set_line_width(1.5);
            cr.set_source_rgb(1.0, 1.0, 1.0);
            cr.arc(x, y, 7.0, 0.0, std::f64::consts::TAU);
            let _ = cr.stroke();
        };
        match comp.kind {
            masks::LINEAR => {
                cr.set_line_width(1.5);
                cr.set_source_rgba(1.0, 1.0, 1.0, 0.9);
                cr.move_to(ox + comp.a[0] as f64 * dw, oy + comp.a[1] as f64 * dh);
                cr.line_to(ox + comp.a[2] as f64 * dw, oy + comp.a[3] as f64 * dh);
                let _ = cr.stroke();
                pin(cr, comp.a[0], comp.a[1]);
                pin(cr, comp.a[2], comp.a[3]);
            }
            masks::RADIAL => {
                pin(cr, comp.a[0], comp.a[1]);
                pin(cr, comp.a[0] + comp.a[2], comp.a[1]);
            }
            masks::BRUSH => {
                if let Some((hx, hy)) = self.brush_hover.get() {
                    let r = m.brush_size.value() / 100.0 * dw.max(dh);
                    cr.set_line_width(1.5);
                    cr.set_source_rgba(1.0, 1.0, 1.0, 0.9);
                    cr.arc(hx, hy, r.max(2.0), 0.0, std::f64::consts::TAU);
                    let _ = cr.stroke();
                }
            }
            _ => {}
        }
    }

    /// A press, drag or release on the photo while editing a mask. (x, y) are in the overlay's pixels.
    fn mask_pointer(&self, phase: u8, x: f64, y: f64) {
        let Some((ox, oy, dw, dh)) = self.mask_rect() else { return };
        let (fx, fy) = (((x - ox) / dw) as f32, ((y - oy) / dh) as f32);
        let kind = self.masks.borrow().get(self.mask_sel.get()).and_then(|l| l.comps.get(self.comp_sel.get()).map(|c| c.kind));
        let Some(kind) = kind else { return };
        let m = &self.ui.mui;
        let long = dw.max(dh);
        match kind {
            masks::BRUSH => {
                match phase {
                    0 => {
                        let stroke = Stroke {
                            pts: vec![[fx, fy]],
                            size: (m.brush_size.value() / 100.0) as f32,
                            feather: (m.brush_feather.value() / 100.0) as f32,
                            flow: (m.brush_flow.value() / 100.0) as f32,
                            erase: m.brush_erase.is_active(),
                        };
                        self.with_comp(|c| c.strokes.push(stroke));
                    }
                    1 => {
                        self.with_comp(|c| {
                            if let Some(s) = c.strokes.last_mut() {
                                let moved = s.pts.last().is_none_or(|p| (((p[0] - fx) as f64 * dw).powi(2) + ((p[1] - fy) as f64 * dh).powi(2)).sqrt() > long * 0.004);
                                if moved {
                                    s.pts.push([fx, fy]);
                                }
                            }
                        });
                    }
                    _ => {}
                }
                if phase == 2 {
                    self.mask_changed();
                } else {
                    m.area.queue_draw();
                }
            }
            masks::LINEAR | masks::RADIAL => {
                let (fx, fy) = (fx.clamp(0.0, 1.0), fy.clamp(0.0, 1.0));
                if phase == 0 {
                    let pins: Vec<(f32, f32)> = self
                        .masks
                        .borrow()
                        .get(self.mask_sel.get())
                        .and_then(|l| {
                            l.comps.get(self.comp_sel.get()).map(|c| {
                                if kind == masks::LINEAR { vec![(c.a[0], c.a[1]), (c.a[2], c.a[3])] } else { vec![(c.a[0], c.a[1]), (c.a[0] + c.a[2], c.a[1])] }
                            })
                        })
                        .unwrap_or_default();
                    let near = pins.iter().position(|p| ((p.0 - fx) as f64 * dw).hypot((p.1 - fy) as f64 * dh) < 18.0);
                    self.pin_drag.set(match near {
                        Some(i) => Some(i),
                        None => {
                            // drawing a new one: the press is the start (or the centre)
                            self.with_comp(|c| {
                                if kind == masks::LINEAR {
                                    c.a = [fx, fy, fx, fy];
                                } else {
                                    c.a = [fx, fy, 0.02, 0.02];
                                }
                            });
                            Some(if kind == masks::LINEAR { 1 } else { 2 })
                        }
                    });
                }
                if phase <= 1 {
                    if let Some(i) = self.pin_drag.get() {
                        let aspect = (dw / dh) as f32;
                        self.with_comp(|c| {
                            if kind == masks::LINEAR {
                                c.a[i * 2] = fx;
                                c.a[i * 2 + 1] = fy;
                            } else if i == 0 {
                                c.a[0] = fx;
                                c.a[1] = fy;
                            } else if i == 1 {
                                c.a[2] = (fx - c.a[0]).abs().max(0.01);
                            } else {
                                // a new radial gradient grows as a circle
                                let r = (fx - c.a[0]).hypot((fy - c.a[1]) / aspect).max(0.01);
                                c.a[2] = r;
                                c.a[3] = (r * aspect).min(1.5);
                            }
                        });
                    }
                }
                if phase == 2 {
                    self.pin_drag.set(None);
                    self.comp_refresh();
                    self.mask_changed();
                } else {
                    m.area.queue_draw();
                    self.comp_refresh();
                }
            }
            _ => {}
        }
    }
}

/// Wires the panel's controls to the mask data.
pub fn connect(app: &Rc<App>) {
    let m = &app.ui.mui;
    let weak = || Rc::downgrade(app);
    {
        let a = weak();
        m.add.connect_clicked(move |_| {
            if let Some(a) = a.upgrade() {
                a.mask_add();
            }
        });
    }
    {
        let a = weak();
        m.del.connect_clicked(move |_| {
            if let Some(a) = a.upgrade() {
                let i = a.mask_sel.get();
                if i < a.masks.borrow().len() {
                    a.masks.borrow_mut().remove(i);
                }
                a.mask_sel.set(i.saturating_sub(1));
                a.comp_sel.set(0);
                a.mask_refresh();
                a.request_render();
            }
        });
    }
    {
        let a = weak();
        m.layer.connect_selected_notify(move |r| {
            if let Some(a) = a.upgrade() {
                if !a.updating.get() {
                    a.mask_sel.set(r.selected() as usize);
                    a.comp_sel.set(0);
                    a.mask_refresh();
                }
            }
        });
    }
    {
        let a = weak();
        m.comp.connect_selected_notify(move |r| {
            if let Some(a) = a.upgrade() {
                if !a.updating.get() {
                    a.comp_sel.set(r.selected() as usize);
                    a.comp_refresh();
                }
            }
        });
    }
    {
        let a = weak();
        m.visible.connect_active_notify(move |r| {
            if let Some(a) = a.upgrade() {
                if !a.updating.get() {
                    a.with_layer(|l| l.visible = r.is_active());
                    a.mask_changed();
                }
            }
        });
    }
    {
        let a = weak();
        m.amount.connect_value_notify(move |r| {
            if let Some(a) = a.upgrade() {
                if !a.updating.get() {
                    a.with_layer(|l| l.amount = (r.value() / 100.0) as f32);
                    a.mask_changed();
                }
            }
        });
    }
    for (kind, b) in m.add_kind_buttons.iter().enumerate() {
        let a = weak();
        let pop = m.add_comp.popover();
        b.connect_clicked(move |_| {
            if let Some(p) = &pop {
                p.popdown();
            }
            if let Some(a) = a.upgrade() {
                a.comp_add(kind);
            }
        });
    }
    {
        let a = weak();
        m.del_comp.connect_clicked(move |_| {
            if let Some(a) = a.upgrade() {
                let c = a.comp_sel.get();
                a.with_layer(|l| {
                    if c < l.comps.len() {
                        l.comps.remove(c);
                    }
                });
                a.comp_sel.set(c.saturating_sub(1));
                a.mask_refresh();
                a.request_render();
            }
        });
    }
    {
        let a = weak();
        m.mode.connect_selected_notify(move |r| {
            if let Some(a) = a.upgrade() {
                if !a.updating.get() {
                    a.with_comp(|c| c.mode = r.selected() as usize);
                    a.mask_changed();
                }
            }
        });
    }
    {
        let a = weak();
        m.invert.connect_active_notify(move |r| {
            if let Some(a) = a.upgrade() {
                if !a.updating.get() {
                    a.with_comp(|c| c.invert = r.is_active());
                    a.mask_changed();
                }
            }
        });
    }
    for (i, sl) in m.params.iter().enumerate() {
        let a = weak();
        sl.connect_value_notify(move |r| {
            let Some(a) = a.upgrade() else { return };
            if a.updating.get() {
                return;
            }
            let kind = a.masks.borrow().get(a.mask_sel.get()).and_then(|l| l.comps.get(a.comp_sel.get()).map(|c| c.kind));
            let Some(kind) = kind else { return };
            let sp = specs(kind);
            if let Some(s) = sp.get(i) {
                let v = (r.value() / s.scale) as f32;
                a.with_comp(|c| match s.target {
                    Target::A(k) => c.a[k] = v,
                    Target::P(k) => c.p[k] = v,
                });
                a.mask_changed();
            }
        });
    }
    for (i, sl) in m.adjust.iter().enumerate() {
        let a = weak();
        sl.connect_value_notify(move |r| {
            let Some(a) = a.upgrade() else { return };
            if a.updating.get() {
                return;
            }
            a.with_layer(|l| l.adj.values[i] = (r.value() / 100.0) as f32);
            a.mask_changed();
        });
    }
    {
        let a = weak();
        m.brush_clear.connect_clicked(move |_| {
            if let Some(a) = a.upgrade() {
                a.with_comp(|c| c.strokes.clear());
                a.mask_changed();
            }
        });
    }
    // editing on the photo
    {
        let a = weak();
        m.edit.connect_active_notify(move |r| {
            let Some(a) = a.upgrade() else { return };
            if r.is_active() {
                a.ui.rui.on.set_active(false);
                a.set_zoom(1.0);
                if a.ui.crop_button.is_active() {
                    a.ui.crop_button.set_active(false);
                }
            }
            a.ui.mui.area.set_can_target(r.is_active());
            a.ui.mui.area.queue_draw();
        });
    }
    {
        let a = weak();
        m.area.set_draw_func(move |_, cr, _, _| {
            if let Some(a) = a.upgrade() {
                a.draw_mask_overlay(cr);
            }
        });
        let drag = gtk::GestureDrag::new();
        let start = Rc::new(Cell::new((0.0f64, 0.0f64)));
        {
            let (a, start) = (weak(), start.clone());
            drag.connect_drag_begin(move |_, x, y| {
                start.set((x, y));
                if let Some(a) = a.upgrade() {
                    a.mask_pointer(0, x, y);
                }
            });
        }
        {
            let (a, start) = (weak(), start.clone());
            drag.connect_drag_update(move |_, dx, dy| {
                let s = start.get();
                if let Some(a) = a.upgrade() {
                    a.mask_pointer(1, s.0 + dx, s.1 + dy);
                }
            });
        }
        {
            let (a, start) = (weak(), start.clone());
            drag.connect_drag_end(move |_, dx, dy| {
                let s = start.get();
                if let Some(a) = a.upgrade() {
                    a.mask_pointer(2, s.0 + dx, s.1 + dy);
                }
            });
        }
        m.area.add_controller(drag);
        let motion = gtk::EventControllerMotion::new();
        let a = weak();
        motion.connect_motion(move |_, x, y| {
            if let Some(a) = a.upgrade() {
                a.brush_hover.set(Some((x, y)));
                a.ui.mui.area.queue_draw();
            }
        });
        let a = weak();
        motion.connect_leave(move |_| {
            if let Some(a) = a.upgrade() {
                a.brush_hover.set(None);
                a.ui.mui.area.queue_draw();
            }
        });
        m.area.add_controller(motion);
    }
}
