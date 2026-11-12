use std::{
  cell::RefCell,
  sync::atomic::{AtomicU64, Ordering},
};

use tracing::warn;
use windows::{
  core::w,
  Win32::{
    Foundation::{
      BOOL, COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, SIZE,
      WPARAM,
    },
    Graphics::{
      Dwm::{
        DwmSetWindowAttribute, DWMWA_EXCLUDED_FROM_PEEK,
        DWMWA_TRANSITIONS_FORCEDISABLED,
      },
      Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject,
        GetDC, ReleaseDC, SelectObject, AC_SRC_ALPHA, AC_SRC_OVER,
        BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION,
        DIB_RGB_COLORS, HGDIOBJ,
      },
    },
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::{
      CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW,
      SetWindowPos, ShowWindow, UpdateLayeredWindow, HTTRANSPARENT,
      HWND_TOP, MA_NOACTIVATE, SWP_NOACTIVATE, SWP_NOMOVE,
      SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_SHOWWINDOW, SW_HIDE, ULW_ALPHA,
      WM_ERASEBKGND, WM_MOUSEACTIVATE, WM_NCHITTEST, WNDCLASSW,
      WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
      WS_EX_TRANSPARENT, WS_POPUP,
    },
  },
};

use crate::Rect;

const OVERLAY_CLASS_NAME: windows::core::PCWSTR =
  w!("GlazeWM:SnapOverlay");

static SNAP_OVERLAY_GENERATION: AtomicU64 = AtomicU64::new(0);

#[derive(Default)]
struct OverlayState {
  hwnd: Option<isize>,
  class_registered: bool,
  is_visible: bool,
  last_rect: Option<Rect>,
  last_behind_hwnd: Option<isize>,
}

thread_local! {
  static OVERLAY_STATE: RefCell<OverlayState> =
    RefCell::new(OverlayState::default());
}

/// Increments and returns the generation counter for snap overlay updates.
pub(crate) fn next_snap_overlay_generation() -> u64 {
  SNAP_OVERLAY_GENERATION.fetch_add(1, Ordering::AcqRel) + 1
}

/// Shows or updates the Aero glass snap preview overlay behind
/// `behind_hwnd`.
///
/// Must be called on the Windows event loop thread.
pub(crate) fn show_snap_overlay(
  rect: &Rect,
  behind_hwnd: Option<isize>,
  generation: u64,
) {
  if generation != SNAP_OVERLAY_GENERATION.load(Ordering::Acquire) {
    return;
  }

  let width = rect.width();
  let height = rect.height();
  if width <= 0 || height <= 0 {
    hide_snap_overlay_inner();
    return;
  }

  OVERLAY_STATE.with(|state| {
    let mut state = state.borrow_mut();

    let Some(hwnd) = ensure_overlay_window(&mut state) else {
      return;
    };

    let needs_redraw = !state.is_visible
      || state.last_rect.as_ref() != Some(rect);

    if needs_redraw && !paint_layered_overlay(hwnd, rect) {
      return;
    }

    if needs_redraw || state.last_behind_hwnd != behind_hwnd {
      apply_z_order_and_show(hwnd, behind_hwnd);
    }

    state.is_visible = true;
    state.last_rect = Some(rect.clone());
    state.last_behind_hwnd = behind_hwnd;
  });
}

/// Hides the Aero snap preview overlay if `generation` is still current.
///
/// Must be called on the Windows event loop thread.
pub(crate) fn hide_snap_overlay(generation: u64) {
  if generation != SNAP_OVERLAY_GENERATION.load(Ordering::Acquire) {
    return;
  }

  hide_snap_overlay_inner();
}

/// Destroys the snap preview overlay window on event loop shutdown.
pub(crate) fn cleanup_snap_overlay() {
  SNAP_OVERLAY_GENERATION.fetch_add(1, Ordering::AcqRel);

  OVERLAY_STATE.with(|state| {
    let mut state = state.borrow_mut();
    if let Some(raw_hwnd) = state.hwnd.take() {
      // SAFETY: `raw_hwnd` was created on this thread.
      let _ = unsafe { DestroyWindow(HWND(raw_hwnd)) };
    }
    state.is_visible = false;
    state.last_rect = None;
    state.last_behind_hwnd = None;
  });
}

fn hide_snap_overlay_inner() {
  OVERLAY_STATE.with(|state| {
    let mut state = state.borrow_mut();
    if let (Some(raw_hwnd), true) = (state.hwnd, state.is_visible) {
      // SAFETY: `raw_hwnd` is a valid overlay window on this thread.
      unsafe {
        let _ = ShowWindow(HWND(raw_hwnd), SW_HIDE);
      }
    }
    state.is_visible = false;
    state.last_rect = None;
    state.last_behind_hwnd = None;
  });
}

fn ensure_overlay_window(state: &mut OverlayState) -> Option<HWND> {
  if let Some(raw_hwnd) = state.hwnd {
    return Some(HWND(raw_hwnd));
  }

  // SAFETY: `GetModuleHandleW(None)` retrieves the current process module.
  let hinstance = unsafe { GetModuleHandleW(None) }
    .map(HINSTANCE::from)
    .unwrap_or_default();

  if !state.class_registered {
    let wnd_class = WNDCLASSW {
      lpszClassName: OVERLAY_CLASS_NAME,
      lpfnWndProc: Some(overlay_wnd_proc),
      hInstance: hinstance,
      ..Default::default()
    };

    // SAFETY: `wnd_class` uses a static class name and valid wndproc.
    unsafe {
      RegisterClassW(&raw const wnd_class);
    }
    state.class_registered = true;
  }

  let ex_style = WS_EX_LAYERED
    | WS_EX_TRANSPARENT
    | WS_EX_TOOLWINDOW
    | WS_EX_NOACTIVATE;

  // SAFETY: Creating a hidden layered popup overlay on the message thread.
  let hwnd = unsafe {
    CreateWindowExW(
      ex_style,
      OVERLAY_CLASS_NAME,
      w!("GlazeWM Snap Overlay"),
      WS_POPUP,
      0,
      0,
      1,
      1,
      None,
      None,
      hinstance,
      None,
    )
  };

  if hwnd.0 == 0 {
    warn!("Failed to create snap preview overlay window.");
    return None;
  }

  let enabled = BOOL(1);
  #[allow(clippy::cast_possible_truncation)]
  let bool_size = std::mem::size_of::<BOOL>() as u32;
  // SAFETY: `hwnd` is a valid window and `enabled` lives across the calls.
  unsafe {
    let _ = DwmSetWindowAttribute(
      hwnd,
      DWMWA_EXCLUDED_FROM_PEEK,
      std::ptr::from_ref(&enabled).cast(),
      bool_size,
    );
    let _ = DwmSetWindowAttribute(
      hwnd,
      DWMWA_TRANSITIONS_FORCEDISABLED,
      std::ptr::from_ref(&enabled).cast(),
      bool_size,
    );
  }

  state.hwnd = Some(hwnd.0);
  Some(hwnd)
}

fn apply_z_order_and_show(hwnd: HWND, behind_hwnd: Option<isize>) {
  let flags = SWP_NOMOVE
    | SWP_NOSIZE
    | SWP_NOACTIVATE
    | SWP_SHOWWINDOW
    | SWP_NOOWNERZORDER;

  let insert_after = behind_hwnd.map_or(HWND_TOP, HWND);
  // SAFETY: `hwnd` is owned by this thread; fallback to `HWND_TOP` if the
  // target drag window is no longer valid.
  let positioned = unsafe {
    SetWindowPos(hwnd, insert_after, 0, 0, 0, 0, flags)
  };

  if positioned.is_err() && behind_hwnd.is_some() {
    // SAFETY: `hwnd` is valid; placing at top of non-topmost band.
    let _ = unsafe { SetWindowPos(hwnd, HWND_TOP, 0, 0, 0, 0, flags) };
  }
}

#[allow(clippy::too_many_lines)]
fn paint_layered_overlay(hwnd: HWND, rect: &Rect) -> bool {
  let width = rect.width();
  let height = rect.height();
  if width <= 0 || height <= 0 {
    return false;
  }

  // SAFETY: `GetDC(None)` acquires the screen DC; released before return.
  let screen_dc = unsafe { GetDC(None) };
  if screen_dc.0 == 0 {
    return false;
  }

  // SAFETY: `CreateCompatibleDC` returns a memory DC matching the screen.
  let mem_dc = unsafe { CreateCompatibleDC(screen_dc) };
  if mem_dc.0 == 0 {
    // SAFETY: `screen_dc` was acquired above via `GetDC(None)`.
    unsafe {
      ReleaseDC(None, screen_dc);
    }
    return false;
  }

  #[allow(clippy::cast_possible_truncation)]
  let bmi_header_size = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
  let mut bmi = BITMAPINFO::default();
  bmi.bmiHeader = BITMAPINFOHEADER {
    biSize: bmi_header_size,
    biWidth: width,
    biHeight: -height,
    biPlanes: 1,
    biBitCount: 32,
    biCompression: BI_RGB.0,
    ..Default::default()
  };

  let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
  // SAFETY: `mem_dc` and `bmi` are initialized; `bits` receives the DIB
  // pixel buffer pointer on success.
  let Ok(bitmap) = (unsafe {
    CreateDIBSection(
      mem_dc,
      &raw const bmi,
      DIB_RGB_COLORS,
      &raw mut bits,
      None,
      0,
    )
  }) else {
    // SAFETY: Releasing acquired DCs on failure.
    unsafe {
      DeleteDC(mem_dc);
      ReleaseDC(None, screen_dc);
    }
    return false;
  };

  if bits.is_null() {
    // SAFETY: Cleaning up DIB and DCs.
    unsafe {
      DeleteObject(bitmap);
      DeleteDC(mem_dc);
      ReleaseDC(None, screen_dc);
    }
    return false;
  }

  fill_aero_glass_pixels(bits, width, height);

  // SAFETY: Selecting `bitmap` into `mem_dc` before `UpdateLayeredWindow`.
  let old_obj: HGDIOBJ = unsafe { SelectObject(mem_dc, bitmap) };
  let pt_dst = POINT {
    x: rect.left,
    y: rect.top,
  };
  let size = SIZE {
    cx: width,
    cy: height,
  };
  let pt_src = POINT { x: 0, y: 0 };

  #[allow(clippy::cast_possible_truncation)]
  let blend = BLENDFUNCTION {
    BlendOp: AC_SRC_OVER as u8,
    BlendFlags: 0,
    SourceConstantAlpha: u8::MAX,
    AlphaFormat: AC_SRC_ALPHA as u8,
  };

  // SAFETY: All handles and pointers stay valid for the Win32 call.
  let updated = unsafe {
    UpdateLayeredWindow(
      hwnd,
      screen_dc,
      Some(&raw const pt_dst),
      Some(&raw const size),
      mem_dc,
      Some(&raw const pt_src),
      COLORREF(0),
      Some(&raw const blend),
      ULW_ALPHA,
    )
  };

  // SAFETY: Restore previous GDI object and free temporary DIB & DCs.
  unsafe {
    SelectObject(mem_dc, old_obj);
    DeleteObject(bitmap);
    DeleteDC(mem_dc);
    ReleaseDC(None, screen_dc);
  }

  if let Err(err) = updated {
    warn!("Failed to update snap preview overlay: {err}");
    return false;
  }

  true
}

fn fill_aero_glass_pixels(
  bits: *mut std::ffi::c_void,
  width: i32,
  height: i32,
) {
  let Ok(width_usize) = usize::try_from(width) else {
    return;
  };
  let Ok(height_usize) = usize::try_from(height) else {
    return;
  };
  let Some(pixel_count) = width_usize.checked_mul(height_usize) else {
    return;
  };

  // SAFETY: `CreateDIBSection` allocated `width * height` 32-bit pixels.
  let pixels = unsafe {
    std::slice::from_raw_parts_mut(bits.cast::<u32>(), pixel_count)
  };

  let denom = height_usize.saturating_sub(1).max(1);
  for row in 0..height_usize {
    let dist_y = row.min(height_usize - 1 - row);
    #[allow(clippy::cast_possible_truncation)]
    // Colorless clear Aero glass sheen: slightly stronger near top edge.
    let sheen_alpha = (52usize - (22usize * row) / denom) as u8;
    let fill_pixel = premultiply_bgra(246, 248, 250, sheen_alpha);

    let row_offset = row * width_usize;
    for col in 0..width_usize {
      let dist = dist_y.min(col).min(width_usize - 1 - col);
      pixels[row_offset + col] = match dist {
        0 => premultiply_bgra(16, 18, 20, 120),
        1 => premultiply_bgra(255, 255, 255, 220),
        2 => premultiply_bgra(236, 240, 244, 105),
        _ => fill_pixel,
      };
    }
  }
}

#[inline]
fn premultiply_bgra(red: u8, green: u8, blue: u8, alpha: u8) -> u32 {
  let alpha_u32 = u32::from(alpha);
  #[allow(clippy::cast_possible_truncation)]
  let premul_red = ((u32::from(red) * alpha_u32 + 127) / 255) as u8;
  #[allow(clippy::cast_possible_truncation)]
  let premul_green = ((u32::from(green) * alpha_u32 + 127) / 255) as u8;
  #[allow(clippy::cast_possible_truncation)]
  let premul_blue = ((u32::from(blue) * alpha_u32 + 127) / 255) as u8;

  u32::from_le_bytes([premul_blue, premul_green, premul_red, alpha])
}

unsafe extern "system" fn overlay_wnd_proc(
  hwnd: HWND,
  msg: u32,
  wparam: WPARAM,
  lparam: LPARAM,
) -> LRESULT {
  match msg {
    WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
    WM_MOUSEACTIVATE => {
      #[allow(clippy::cast_possible_wrap)]
      LRESULT(MA_NOACTIVATE as isize)
    }
    WM_ERASEBKGND => LRESULT(1),
    _ => DefWindowProcW(hwnd, msg, wparam, lparam),
  }
}
