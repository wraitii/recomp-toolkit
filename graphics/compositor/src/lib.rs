#[cfg(feature = "ffi")]
mod ffi;

#[cfg(feature = "gpu")]
pub mod gpu;
pub mod presenter;
#[cfg(feature = "gpu")]
pub mod surface;

pub const OUTPUT_LAYOUT: u32 = 1;
pub const OUTPUT_DRAWS: u32 = 2;

use std::collections::BTreeMap;
use std::sync::Mutex;

#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct Scene {
    pub scale_x: f32,
    pub scale_y: f32,
    pub offset_x: f32,
    pub offset_y: f32,
    pub domain_w: i32,
}
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct Input {
    pub cls: i32,
    pub guest_w: i32,
    pub guest_h: i32,
    pub drawable_w: i32,
    pub drawable_h: i32,
    pub scale_override: i32,
    pub scene: Scene,
    pub legacy: u8,
    pub classic: u8,
    pub narrow: u8,
    pub has_world: u8,
    pub has_overlay: u8,
    pub has_legacy: u8,
    pub has_settings: u8,
}
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct Element {
    pub id: u64,
    pub last_seq: u32,
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub is_cursor: u8,
    pub is_hud: u8,
}
#[repr(u32)]
#[derive(Clone, Copy, Default, Debug)]
pub enum Source {
    #[default]
    World,
    Ui,
    Overlay,
    Legacy,
    Settings,
}
#[repr(u32)]
#[derive(Clone, Copy, Default, Debug)]
pub enum Blend {
    #[default]
    Opaque,
    Straight,
    Premultiplied,
}
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct Draw {
    pub source: Source,
    pub element_index: u32,
    pub blend: Blend,
    pub dst: Rect,
    pub uv: Rect,
}
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct LayoutElement {
    pub element: Element,
    pub drawable: Rect,
}
#[repr(C)]
#[derive(Clone, Copy, Default, Debug)]
pub struct Layout {
    pub scene: Scene,
    pub ui_scale: i32,
}
#[derive(Clone, Default)]
struct State {
    anchors: BTreeMap<u64, (i8, i8)>,
    pointer: Option<(i32, i32)>,
}
#[derive(Default)]
pub struct Compositor {
    state: Mutex<State>,
}
#[derive(Default)]
pub struct ResultData {
    pub layout: Layout,
    pub elements: Vec<LayoutElement>,
    pub draws: Vec<Draw>,
}

pub fn ui_scale(drawable_h: i32, guest_h: i32, override_value: i32) -> i32 {
    (if override_value != 0 {
        override_value
    } else {
        drawable_h / guest_h.max(1)
    })
    .clamp(1, 4)
}
pub fn default_anchor(e: &Element, w: i32, h: i32) -> (i8, i8) {
    fn axis(p: i32, extent: i32, domain: i32) -> i8 {
        let c = p as f64 + extent as f64 / 2.0;
        let d = domain as f64 / 2.0;
        if (c - d).abs() <= 8.0 {
            0
        } else if c <= d {
            -1
        } else {
            1
        }
    }
    (axis(e.x, e.w, w), axis(e.y, e.h, h))
}
fn whole(i: &Input, integer: bool) -> Rect {
    let s = if integer {
        ui_scale(i.drawable_h, i.guest_h, i.scale_override) as f64
    } else {
        (i.drawable_w as f64 / i.guest_w as f64).min(i.drawable_h as f64 / i.guest_h as f64)
    };
    let w = i.guest_w as f64 * s;
    let h = i.guest_h as f64 * s;
    Rect {
        x: (i.drawable_w as f64 - w) / 2.0,
        y: (i.drawable_h as f64 - h) / 2.0,
        w,
        h,
    }
}
fn boxed(i: &Input) -> bool {
    i.narrow != 0
        && i.cls == 2
        && i.legacy == 0
        && i.classic == 0
        && i.drawable_w as i64 * i.guest_h as i64 > (i.drawable_h as i64 + 1) * i.guest_w as i64
}
fn pixel(r: Rect) -> Rect {
    let x = r.x.round();
    let y = r.y.round();
    Rect {
        x,
        y,
        w: (r.x + r.w).round() - x,
        h: (r.y + r.h).round() - y,
    }
}
fn settings(i: &Input) -> Rect {
    let mut p = *i;
    p.guest_w = 640;
    p.guest_h = 480;
    let fits = (i.drawable_w / 640).min(i.drawable_h / 480);
    if fits < 1 {
        pixel(whole(&p, false))
    } else {
        p.scale_override = ui_scale(i.drawable_h, 480, i.scale_override).min(fits);
        pixel(whole(&p, true))
    }
}
fn element_rect(i: &Input, e: &Element, state: &State) -> Rect {
    let scale = ui_scale(i.drawable_h, i.guest_h, i.scale_override) as f64;
    let mapped_whole = i.legacy != 0 || i.classic != 0 || i.cls != 2 || boxed(i);
    if e.is_cursor != 0 {
        if let Some((x, y)) = state.pointer {
            let (sx, sy) = if !mapped_whole {
                (scale, scale)
            } else {
                let r = whole(i, i.legacy == 0 && i.classic == 0 && i.cls == 0);
                (r.w / i.guest_w as f64, r.h / i.guest_h as f64)
            };
            return pixel(Rect {
                x: x as f64,
                y: y as f64,
                w: e.w as f64 * sx,
                h: e.h as f64 * sy,
            });
        }
    }
    if mapped_whole {
        let r = whole(i, i.legacy == 0 && i.classic == 0 && i.cls == 0);
        let sx = r.w / i.guest_w as f64;
        let sy = r.h / i.guest_h as f64;
        return pixel(Rect {
            x: r.x + e.x as f64 * sx,
            y: r.y + e.y as f64 * sy,
            w: e.w as f64 * sx,
            h: e.h as f64 * sy,
        });
    }
    if e.is_cursor != 0 {
        let s = i.scene;
        if s.scale_x.is_finite()
            && s.scale_x > 0.0
            && s.scale_y.is_finite()
            && s.scale_y > 0.0
            && s.offset_x.is_finite()
            && s.offset_y.is_finite()
        {
            return pixel(Rect {
                x: e.x as f64 * s.scale_x as f64 + s.offset_x as f64,
                y: e.y as f64 * s.scale_y as f64 + s.offset_y as f64,
                w: e.w as f64 * scale,
                h: e.h as f64 * scale,
            });
        }
        let r = whole(i, true);
        return pixel(Rect {
            x: r.x + e.x as f64 * scale,
            y: r.y + e.y as f64 * scale,
            w: e.w as f64 * scale,
            h: e.h as f64 * scale,
        });
    }
    let (h, v) = state
        .anchors
        .get(&e.id)
        .copied()
        .unwrap_or_else(|| default_anchor(e, i.guest_w, i.guest_h));
    fn axis(p: i32, domain: i32, drawable: i32, scale: f64, a: i8) -> f64 {
        if a < 0 {
            p as f64 * scale
        } else if a > 0 {
            drawable as f64 - (domain as f64 - p as f64) * scale
        } else {
            (drawable as f64 - domain as f64 * scale) / 2.0 + p as f64 * scale
        }
    }
    pixel(Rect {
        x: axis(e.x, i.guest_w, i.drawable_w, scale, h),
        y: axis(e.y, i.guest_h, i.drawable_h, scale, v),
        w: e.w as f64 * scale,
        h: e.h as f64 * scale,
    })
}
fn subtract(pieces: &mut Vec<Rect>, b: Rect) {
    let mut next = Vec::new();
    for a in pieces.drain(..) {
        let x = a.x.max(b.x);
        let y = a.y.max(b.y);
        let r = (a.x + a.w).min(b.x + b.w);
        let bottom = (a.y + a.h).min(b.y + b.h);
        if x >= r || y >= bottom {
            next.push(a);
            continue;
        }
        if y > a.y {
            next.push(Rect {
                x: a.x,
                y: a.y,
                w: a.w,
                h: y - a.y,
            });
        }
        if bottom < a.y + a.h {
            next.push(Rect {
                x: a.x,
                y: bottom,
                w: a.w,
                h: a.y + a.h - bottom,
            });
        }
        if x > a.x {
            next.push(Rect {
                x: a.x,
                y,
                w: x - a.x,
                h: bottom - y,
            });
        }
        if r < a.x + a.w {
            next.push(Rect {
                x: r,
                y,
                w: a.x + a.w - r,
                h: bottom - y,
            });
        }
    }
    *pieces = next;
}
impl Compositor {
    pub fn set_anchor(&self, id: u64, h: i8, v: i8) {
        self.state
            .lock()
            .unwrap()
            .anchors
            .insert(id, (h.clamp(-1, 1), v.clamp(-1, 1)));
    }
    pub fn clear_anchor(&self, id: u64) {
        self.state.lock().unwrap().anchors.remove(&id);
    }
    pub fn set_pointer(&self, pointer: Option<(i32, i32)>) {
        self.state.lock().unwrap().pointer = pointer;
    }
    pub fn build(&self, i: &Input, elements: &[Element], outputs: u32) -> ResultData {
        assert!(
            outputs != 0 && outputs & !(OUTPUT_LAYOUT | OUTPUT_DRAWS) == 0,
            "compositor: invalid output selection {outputs}"
        );
        assert!(
            (0..=2).contains(&i.cls),
            "compositor: unknown screen class {}",
            i.cls
        );
        assert!(
            u32::try_from(elements.len()).is_ok(),
            "compositor: too many elements"
        );
        let mut out = ResultData::default();
        if i.guest_w <= 0 || i.guest_h <= 0 || i.drawable_w <= 0 || i.drawable_h <= 0 {
            return out;
        }
        let state = self.state.lock().unwrap().clone();
        let mut order: Vec<usize> = (0..elements.len()).collect();
        order.sort_by_key(|&n| elements[n].last_seq);
        if outputs & OUTPUT_LAYOUT != 0 {
            let mut scene = i.scene;
            if i.legacy != 0
                || i.classic != 0
                || i.cls != 2
                || boxed(i)
                || !scene.scale_x.is_finite()
                || scene.scale_x <= 0.0
                || !scene.scale_y.is_finite()
                || scene.scale_y <= 0.0
                || !scene.offset_x.is_finite()
                || !scene.offset_y.is_finite()
                || scene.domain_w <= 0
            {
                let r = whole(i, i.legacy == 0 && i.classic == 0 && i.cls == 0);
                scene = Scene {
                    scale_x: (r.w / i.guest_w as f64) as f32,
                    scale_y: (r.h / i.guest_h as f64) as f32,
                    offset_x: r.x as f32,
                    offset_y: r.y as f32,
                    domain_w: i.guest_w,
                };
            }
            out.layout = Layout {
                scene,
                ui_scale: ui_scale(i.drawable_h, i.guest_h, i.scale_override),
            };
            for &n in &order {
                let e = elements[n];
                if e.w <= 0 || e.h <= 0 {
                    continue;
                }
                let r = element_rect(i, &e, &state);
                if r.w > 0.0
                    && r.h > 0.0
                    && [r.x, r.y, r.w, r.h]
                        .iter()
                        .all(|v| v.is_finite() && *v >= i32::MIN as f64 && *v <= i32::MAX as f64)
                {
                    out.elements.push(LayoutElement {
                        element: e,
                        drawable: r,
                    });
                }
            }
        }
        if outputs & OUTPUT_DRAWS == 0 {
            return out;
        }
        let uv = Rect {
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
        };
        let push = |draws: &mut Vec<Draw>, source, element_index, blend, dst: Rect, uv: Rect| {
            if [dst.x, dst.y, dst.w, dst.h, uv.x, uv.y, uv.w, uv.h]
                .iter()
                .all(|v: &f64| v.is_finite())
                && dst.w > 0.0
                && dst.h > 0.0
                && dst.x < i.drawable_w as f64
                && dst.y < i.drawable_h as f64
                && dst.x + dst.w > 0.0
                && dst.y + dst.h > 0.0
            {
                draws.push(Draw {
                    source,
                    element_index,
                    blend,
                    dst,
                    uv,
                });
            }
        };
        if i.legacy != 0 {
            if i.has_legacy != 0 {
                push(
                    &mut out.draws,
                    Source::Legacy,
                    0,
                    Blend::Opaque,
                    pixel(whole(i, false)),
                    uv,
                );
            }
        } else {
            if i.cls == 2 && i.has_world != 0 {
                let dst = if i.classic != 0 || boxed(i) {
                    pixel(whole(i, false))
                } else {
                    Rect {
                        x: 0.0,
                        y: 0.0,
                        w: i.drawable_w as f64,
                        h: i.drawable_h as f64,
                    }
                };
                push(&mut out.draws, Source::World, 0, Blend::Opaque, dst, uv);
            }
            for &n in &order {
                push(
                    &mut out.draws,
                    Source::Ui,
                    n as u32,
                    Blend::Straight,
                    element_rect(i, &elements[n], &state),
                    uv,
                );
            }
            if i.cls == 2 && i.has_overlay != 0 {
                for (pos, &n) in order.iter().enumerate() {
                    let e = elements[n];
                    if e.is_hud == 0 || e.is_cursor != 0 || e.w <= 0 || e.h <= 0 {
                        continue;
                    }
                    let x = (e.x as f64).max(0.0);
                    let y = (e.y as f64).max(0.0);
                    let r = (e.x as f64 + e.w as f64).min(i.guest_w as f64);
                    let b = (e.y as f64 + e.h as f64).min(i.guest_h as f64);
                    if r <= x || b <= y {
                        continue;
                    }
                    let mut pieces = vec![Rect {
                        x,
                        y,
                        w: r - x,
                        h: b - y,
                    }];
                    for &later in &order[pos + 1..] {
                        let l = elements[later];
                        if l.is_hud != 0 && l.is_cursor == 0 && l.w > 0 && l.h > 0 {
                            subtract(
                                &mut pieces,
                                Rect {
                                    x: l.x as f64,
                                    y: l.y as f64,
                                    w: l.w as f64,
                                    h: l.h as f64,
                                },
                            );
                        }
                    }
                    let dst = element_rect(i, &e, &state);
                    let sx = dst.w / e.w as f64;
                    let sy = dst.h / e.h as f64;
                    for part in pieces {
                        push(
                            &mut out.draws,
                            Source::Overlay,
                            n as u32,
                            Blend::Premultiplied,
                            Rect {
                                x: dst.x + (part.x - e.x as f64) * sx,
                                y: dst.y + (part.y - e.y as f64) * sy,
                                w: part.w * sx,
                                h: part.h * sy,
                            },
                            Rect {
                                x: part.x / i.guest_w as f64,
                                y: part.y / i.guest_h as f64,
                                w: part.w / i.guest_w as f64,
                                h: part.h / i.guest_h as f64,
                            },
                        );
                    }
                }
            }
        }
        if i.has_settings != 0 {
            push(
                &mut out.draws,
                Source::Settings,
                0,
                Blend::Straight,
                settings(i),
                uv,
            );
        }
        out
    }
}

impl Compositor {
    pub fn element_rect(&self, i: &Input, e: &Element) -> Option<Rect> {
        assert!(
            (0..=2).contains(&i.cls),
            "compositor: unknown screen class {}",
            i.cls
        );
        if i.guest_w <= 0
            || i.guest_h <= 0
            || i.drawable_w <= 0
            || i.drawable_h <= 0
            || e.w <= 0
            || e.h <= 0
        {
            return None;
        }
        let r = element_rect(i, e, &self.state.lock().unwrap());
        [r.x, r.y, r.w, r.h]
            .iter()
            .all(|v| v.is_finite() && *v >= i32::MIN as f64 && *v <= i32::MAX as f64)
            .then_some(r)
    }
}
