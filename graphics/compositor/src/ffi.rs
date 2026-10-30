use super::*;
use std::panic::{catch_unwind, AssertUnwindSafe};

pub(crate) fn boundary<T>(f: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or_else(|_| {
        eprintln!("compositor: panic across C boundary");
        std::process::abort()
    })
}
#[no_mangle]
pub extern "C" fn rc_compositor_new() -> *mut Compositor {
    boundary(|| Box::into_raw(Box::new(Compositor::default())))
}
#[no_mangle]
pub unsafe extern "C" fn rc_compositor_free(c: *mut Compositor) {
    boundary(|| {
        if !c.is_null() {
            drop(Box::from_raw(c));
        }
    });
}
#[no_mangle]
pub unsafe extern "C" fn rc_set_anchor(c: *mut Compositor, id: u64, h: i8, v: i8) {
    boundary(|| {
        if let Some(c) = c.as_ref() {
            c.set_anchor(id, h, v);
        }
    });
}
#[no_mangle]
pub unsafe extern "C" fn rc_clear_anchor(c: *mut Compositor, id: u64) {
    boundary(|| {
        if let Some(c) = c.as_ref() {
            c.clear_anchor(id);
        }
    });
}
#[no_mangle]
pub unsafe extern "C" fn rc_set_pointer(c: *mut Compositor, valid: u8, x: i32, y: i32) {
    boundary(|| {
        if let Some(c) = c.as_ref() {
            c.set_pointer((valid != 0).then_some((x, y)));
        }
    });
}
#[no_mangle]
pub extern "C" fn rc_ui_scale(h: i32, guest: i32, value: i32) -> i32 {
    boundary(|| ui_scale(h, guest, value))
}
#[no_mangle]
pub unsafe extern "C" fn rc_default_anchor(
    e: *const Element,
    w: i32,
    h: i32,
    out_h: *mut i8,
    out_v: *mut i8,
) {
    boundary(|| {
        let a = e
            .as_ref()
            .map(|e| default_anchor(e, w, h))
            .unwrap_or((-1, -1));
        if let Some(p) = out_h.as_mut() {
            *p = a.0;
        }
        if let Some(p) = out_v.as_mut() {
            *p = a.1;
        }
    });
}
#[no_mangle]
pub unsafe extern "C" fn rc_element_rect(
    c: *mut Compositor,
    i: *const Input,
    e: *const Element,
    out: *mut Rect,
) -> u8 {
    boundary(|| {
        if let (Some(c), Some(i), Some(e), Some(out)) =
            (c.as_ref(), i.as_ref(), e.as_ref(), out.as_mut())
        {
            if let Some(r) = c.element_rect(i, e) {
                *out = r;
                return 1;
            }
        }
        0
    })
}
#[no_mangle]
pub unsafe extern "C" fn rc_build(
    c: *mut Compositor,
    i: *const Input,
    elements: *const Element,
    count: usize,
    outputs: u32,
) -> *mut ResultData {
    boundary(|| {
        let (Some(c), Some(i)) = (c.as_ref(), i.as_ref()) else {
            return std::ptr::null_mut();
        };
        if count > isize::MAX as usize / std::mem::size_of::<Element>()
            || (count != 0 && elements.is_null())
            || count > u32::MAX as usize
        {
            return std::ptr::null_mut();
        }
        let elements = if count == 0 {
            &[]
        } else {
            std::slice::from_raw_parts(elements, count)
        };
        Box::into_raw(Box::new(c.build(i, elements, outputs)))
    })
}
#[no_mangle]
pub unsafe extern "C" fn rc_result_free(r: *mut ResultData) {
    boundary(|| {
        if !r.is_null() {
            drop(Box::from_raw(r));
        }
    });
}
#[no_mangle]
pub unsafe extern "C" fn rc_result_layout(r: *const ResultData) -> Layout {
    boundary(|| r.as_ref().map(|r| r.layout).unwrap_or_default())
}
#[no_mangle]
pub unsafe extern "C" fn rc_result_elements(
    r: *const ResultData,
    count: *mut usize,
) -> *const LayoutElement {
    boundary(|| {
        if let Some(r) = r.as_ref() {
            if let Some(count) = count.as_mut() {
                *count = r.elements.len();
            }
            r.elements.as_ptr()
        } else {
            if let Some(count) = count.as_mut() {
                *count = 0;
            }
            std::ptr::null()
        }
    })
}
#[no_mangle]
pub unsafe extern "C" fn rc_result_draws(r: *const ResultData, count: *mut usize) -> *const Draw {
    boundary(|| {
        if let Some(r) = r.as_ref() {
            if let Some(count) = count.as_mut() {
                *count = r.draws.len();
            }
            r.draws.as_ptr()
        } else {
            if let Some(count) = count.as_mut() {
                *count = 0;
            }
            std::ptr::null()
        }
    })
}
