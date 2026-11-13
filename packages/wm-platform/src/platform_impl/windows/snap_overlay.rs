use std::{
  cell::RefCell,
  sync::atomic::{AtomicU64, Ordering},
};

use tracing::warn;
use windows::{
  core::{w, PCWSTR},
  Win32::{
    Foundation::{
      GetLastError, BOOL, COLORREF, ERROR_ACCESS_DENIED, ERROR_PIPE_BUSY,
      ERROR_SEM_TIMEOUT, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT,
      SIZE, WPARAM,
    },
    Graphics::{
      Dwm::{
        DwmEnableBlurBehindWindow, DwmIsCompositionEnabled,
        DwmSetWindowAttribute, DWMNCRP_DISABLED, DWMWA_BORDER_COLOR,
        DWMWA_COLOR_NONE, DWMWA_EXCLUDED_FROM_PEEK,
        DWMWA_NCRENDERING_POLICY, DWMWA_TRANSITIONS_FORCEDISABLED,
        DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DWM_BB_ENABLE,
        DWM_BLURBEHIND,
      },
      Gdi::{
        BeginPaint, BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC,
        DeleteObject, EndPaint, GetDC, InvalidateRect, ReleaseDC,
        SelectObject, UpdateWindow, AC_SRC_ALPHA, AC_SRC_OVER,
        BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION,
        DIB_RGB_COLORS, HBITMAP, HGDIOBJ, PAINTSTRUCT, SRCCOPY,
      },
    },
    System::LibraryLoader::GetModuleHandleW,
    UI::WindowsAndMessaging::{
      CreateWindowExW, DefWindowProcW, DestroyWindow, GetWindowRect,
      RegisterClassW, SetWindowPos, ShowWindow, UpdateLayeredWindow,
      CS_HREDRAW, CS_VREDRAW, HTTRANSPARENT, HWND_TOP, MA_NOACTIVATE,
      SWP_NOACTIVATE, SWP_NOCOPYBITS, SWP_NOMOVE, SWP_NOOWNERZORDER,
      SWP_NOSIZE, SWP_SHOWWINDOW, SW_HIDE, ULW_ALPHA, WINDOW_EX_STYLE,
      WM_ERASEBKGND, WM_MOUSEACTIVATE, WM_NCHITTEST, WM_PAINT, WNDCLASSW,
      WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TRANSPARENT,
      WS_POPUP,
    },
  },
};

use crate::Rect;

const OVERLAY_CLASS_NAME: PCWSTR = w!("GlazeWM:SnapOverlay");
const OPENGLASS_HOST_PIPE: PCWSTR =
  w!(r"\\.\pipe\Global\OpenGlassHostPipe");
const NMPWAIT_NOWAIT: u32 = 1;

static SNAP_OVERLAY_GENERATION: AtomicU64 = AtomicU64::new(0);

#[link(name = "kernel32")]
unsafe extern "system" {
  fn WaitNamedPipeW(lp_named_pipe_name: *const u16, timeout: u32) -> i32;
}

#[derive(Default)]
struct OverlayState {
  layered_hwnd: Option<isize>,
  glass_hwnd: Option<isize>,
  class_registered: bool,
  layered_visible: bool,
  glass_visible: bool,
  last_rect: Option<Rect>,
  last_behind_hwnd: Option<isize>,
}

#[derive(Default)]
struct GlassBitmapCache {
  bitmap: Option<HBITMAP>,
  width: i32,
  height: i32,
}

thread_local! {
  static OVERLAY_STATE: RefCell<OverlayState> =
    RefCell::new(OverlayState::default());
  static GLASS_BITMAP_CACHE: RefCell<GlassBitmapCache> =
    RefCell::new(GlassBitmapCache::default());
}

/// Returns `true` when DWM composition is enabled and `OpenGlass` host
/// pipe is active.
pub(crate) fn is_openglass_active() -> bool {
  // SAFETY: `DwmIsCompositionEnabled` takes a valid out pointer.
  let composition_enabled = unsafe { DwmIsCompositionEnabled() }
    .is_ok_and(|enabled| enabled.as_bool());
  if !composition_enabled {
    return false;
  }

  // SAFETY: `OPENGLASS_HOST_PIPE` is a static null-terminated UTF-16
  // string; `NMPWAIT_NOWAIT` performs an immediate non-blocking probe.
  let pipe_ready =
    unsafe { WaitNamedPipeW(OPENGLASS_HOST_PIPE.0, NMPWAIT_NOWAIT) != 0 };
  if pipe_ready {
    return true;
  }

  // Standard user sessions receive `ERROR_ACCESS_DENIED` on the
  // restricted `OpenGlassHostPipe` DACL when `OpenGlass` is running.
  let Err(err) = (unsafe { GetLastError() }) else {
    return false;
  };

  err == ERROR_ACCESS_DENIED.into()
    || err == ERROR_PIPE_BUSY.into()
    || err == ERROR_SEM_TIMEOUT.into()
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

  if rect.width() <= 0 || rect.height() <= 0 {
    hide_snap_overlay_inner();
    return;
  }

  if is_openglass_active() {
    show_glass_overlay(rect, behind_hwnd);
  } else {
    show_layered_overlay(rect, behind_hwnd);
  }
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

/// Destroys the snap preview overlay windows and cached bitmap on event
/// loop shutdown.
pub(crate) fn cleanup_snap_overlay() {
  SNAP_OVERLAY_GENERATION.fetch_add(1, Ordering::AcqRel);

  let (layered_hwnd, glass_hwnd) = OVERLAY_STATE.with(|state| {
    let mut state = state.borrow_mut();
    let layered = state.layered_hwnd.take();
    let glass = state.glass_hwnd.take();
    state.layered_visible = false;
    state.glass_visible = false;
    state.last_rect = None;
    state.last_behind_hwnd = None;
    (layered, glass)
  });

  if let Some(raw_hwnd) = layered_hwnd {
    // SAFETY: `raw_hwnd` was created on this thread.
    let _ = unsafe { DestroyWindow(HWND(raw_hwnd)) };
  }

  if let Some(raw_hwnd) = glass_hwnd {
    // SAFETY: `raw_hwnd` was created on this thread.
    let _ = unsafe { DestroyWindow(HWND(raw_hwnd)) };
  }

  GLASS_BITMAP_CACHE.with(|cache| {
    let mut cache = cache.borrow_mut();
    if let Some(bitmap) = cache.bitmap.take() {
      // SAFETY: `bitmap` is no longer selected into any DC.
      unsafe {
        DeleteObject(bitmap);
      }
    }
    cache.width = 0;
    cache.height = 0;
  });
}

fn hide_snap_overlay_inner() {
  let (layered_to_hide, glass_to_hide) = OVERLAY_STATE.with(|state| {
    let mut state = state.borrow_mut();
    let layered =
      state.layered_visible.then_some(state.layered_hwnd).flatten();
    let glass = state.glass_visible.then_some(state.glass_hwnd).flatten();
    state.layered_visible = false;
    state.glass_visible = false;
    state.last_rect = None;
    state.last_behind_hwnd = None;
    (layered, glass)
  });

  for raw_hwnd in [layered_to_hide, glass_to_hide].into_iter().flatten() {
    // SAFETY: `raw_hwnd` is a valid overlay window on this thread.
    unsafe {
      let _ = ShowWindow(HWND(raw_hwnd), SW_HIDE);
    }
  }
}

fn show_glass_overlay(rect: &Rect, behind_hwnd: Option<isize>) {
  let prep = OVERLAY_STATE.with(|state| {
    let mut state = state.borrow_mut();

    let hide_layered = if state.layered_visible {
      state.layered_visible = false;
      state.layered_hwnd
    } else {
      None
    };

    let glass_hwnd = ensure_glass_overlay_window(&mut state)?;
    let was_visible = state.glass_visible;
    let needs_geometry =
      !was_visible || state.last_rect.as_ref() != Some(rect);
    let needs_zorder =
      needs_geometry || state.last_behind_hwnd != behind_hwnd;

    state.glass_visible = true;
    state.last_rect = Some(rect.clone());
    state.last_behind_hwnd = behind_hwnd;

    Some((
      glass_hwnd,
      hide_layered,
      was_visible,
      needs_geometry,
      needs_zorder,
    ))
  });

  let Some((
    glass_hwnd,
    hide_layered,
    was_visible,
    needs_geometry,
    needs_zorder,
  )) = prep
  else {
    return;
  };

  if let Some(raw_hwnd) = hide_layered {
    // SAFETY: `raw_hwnd` is a valid window owned by this thread.
    unsafe {
      let _ = ShowWindow(HWND(raw_hwnd), SW_HIDE);
    }
  }

  if needs_geometry {
    GLASS_BITMAP_CACHE.with(|cache| {
      let mut cache = cache.borrow_mut();
      let _ = ensure_glass_bitmap(&mut cache, rect.width(), rect.height());
    });

    position_glass_overlay(glass_hwnd, rect, behind_hwnd);

    if !was_visible {
      enable_dwm_glass_behind(glass_hwnd);
    }

    // SAFETY: `glass_hwnd` belongs to this thread; force immediate
    // repaint so the first shown frame displays crisp glass bevels.
    unsafe {
      let _ = InvalidateRect(glass_hwnd, None, false);
      let _ = UpdateWindow(glass_hwnd);
    }
  } else if needs_zorder {
    apply_z_order_and_show(glass_hwnd, behind_hwnd);
  }
}

fn show_layered_overlay(rect: &Rect, behind_hwnd: Option<isize>) {
  let prep = OVERLAY_STATE.with(|state| {
    let mut state = state.borrow_mut();

    let hide_glass = if state.glass_visible {
      state.glass_visible = false;
      state.glass_hwnd
    } else {
      None
    };

    let layered_hwnd = ensure_layered_overlay_window(&mut state)?;
    let needs_redraw =
      !state.layered_visible || state.last_rect.as_ref() != Some(rect);
    let needs_zorder =
      needs_redraw || state.last_behind_hwnd != behind_hwnd;

    state.layered_visible = true;
    state.last_rect = Some(rect.clone());
    state.last_behind_hwnd = behind_hwnd;

    Some((layered_hwnd, hide_glass, needs_redraw, needs_zorder))
  });

  let Some((layered_hwnd, hide_glass, needs_redraw, needs_zorder)) = prep
  else {
    return;
  };

  if let Some(raw_hwnd) = hide_glass {
    // SAFETY: `raw_hwnd` is a valid window owned by this thread.
    unsafe {
      let _ = ShowWindow(HWND(raw_hwnd), SW_HIDE);
    }
  }

  if needs_redraw && !paint_layered_overlay(layered_hwnd, rect) {
    OVERLAY_STATE.with(|state| {
      let mut state = state.borrow_mut();
      state.layered_visible = false;
      state.last_rect = None;
      state.last_behind_hwnd = None;
    });
    return;
  }

  if needs_zorder {
    apply_z_order_and_show(layered_hwnd, behind_hwnd);
  }
}

fn ensure_overlay_class(state: &mut OverlayState) -> HINSTANCE {
  // SAFETY: `GetModuleHandleW(None)` retrieves the current process module.
  let hinstance = unsafe { GetModuleHandleW(None) }
    .map(HINSTANCE::from)
    .unwrap_or_default();

  if !state.class_registered {
    let wnd_class = WNDCLASSW {
      style: CS_HREDRAW | CS_VREDRAW,
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

  hinstance
}

fn create_overlay_popup(
  hinstance: HINSTANCE,
  ex_style: WINDOW_EX_STYLE,
) -> Option<HWND> {
  // SAFETY: Creating a hidden popup overlay on the message thread.
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

  Some(hwnd)
}

fn ensure_layered_overlay_window(
  state: &mut OverlayState,
) -> Option<HWND> {
  if let Some(raw_hwnd) = state.layered_hwnd {
    return Some(HWND(raw_hwnd));
  }

  let hinstance = ensure_overlay_class(state);
  let ex_style = WS_EX_LAYERED
    | WS_EX_TRANSPARENT
    | WS_EX_TOOLWINDOW
    | WS_EX_NOACTIVATE;

  let hwnd = create_overlay_popup(hinstance, ex_style)?;
  state.layered_hwnd = Some(hwnd.0);
  Some(hwnd)
}

fn ensure_glass_overlay_window(state: &mut OverlayState) -> Option<HWND> {
  if let Some(raw_hwnd) = state.glass_hwnd {
    return Some(HWND(raw_hwnd));
  }

  let hinstance = ensure_overlay_class(state);
  let ex_style = WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
  let hwnd = create_overlay_popup(hinstance, ex_style)?;

  #[allow(clippy::cast_possible_truncation)]
  let u32_size = std::mem::size_of::<u32>() as u32;
  let ncrp = DWMNCRP_DISABLED;
  let corner_pref = DWMWCP_DONOTROUND;
  let border_color = DWMWA_COLOR_NONE;

  // SAFETY: `hwnd` is valid; attributes are stack-allocated for the call.
  unsafe {
    let _ = DwmSetWindowAttribute(
      hwnd,
      DWMWA_NCRENDERING_POLICY,
      std::ptr::from_ref(&ncrp).cast(),
      u32_size,
    );
    let _ = DwmSetWindowAttribute(
      hwnd,
      DWMWA_WINDOW_CORNER_PREFERENCE,
      std::ptr::from_ref(&corner_pref).cast(),
      u32_size,
    );
    let _ = DwmSetWindowAttribute(
      hwnd,
      DWMWA_BORDER_COLOR,
      std::ptr::from_ref(&border_color).cast(),
      u32_size,
    );
  }

  enable_dwm_glass_behind(hwnd);
  state.glass_hwnd = Some(hwnd.0);
  Some(hwnd)
}

fn enable_dwm_glass_behind(hwnd: HWND) {
  let blur = DWM_BLURBEHIND {
    dwFlags: DWM_BB_ENABLE,
    fEnable: BOOL(1),
    hRgnBlur: Default::default(),
    fTransitionOnMaximized: BOOL(0),
  };

  // SAFETY: `hwnd` is valid and `blur` lives for the duration of the call.
  let _ = unsafe { DwmEnableBlurBehindWindow(hwnd, &raw const blur) };
}

fn position_glass_overlay(
  hwnd: HWND,
  rect: &Rect,
  behind_hwnd: Option<isize>,
) {
  let flags = SWP_NOACTIVATE
    | SWP_NOCOPYBITS
    | SWP_SHOWWINDOW
    | SWP_NOOWNERZORDER;

  let insert_after = behind_hwnd.map_or(HWND_TOP, HWND);
  // SAFETY: `hwnd` is owned by this thread; fallback to `HWND_TOP` if the
  // target drag window is no longer valid.
  let positioned = unsafe {
    SetWindowPos(
      hwnd,
      insert_after,
      rect.left,
      rect.top,
      rect.width(),
      rect.height(),
      flags,
    )
  };

  if positioned.is_err() && behind_hwnd.is_some() {
    // SAFETY: `hwnd` is valid; placing at top of non-topmost band.
    let _ = unsafe {
      SetWindowPos(
        hwnd,
        HWND_TOP,
        rect.left,
        rect.top,
        rect.width(),
        rect.height(),
        flags,
      )
    };
  }
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
  let positioned =
    unsafe { SetWindowPos(hwnd, insert_after, 0, 0, 0, 0, flags) };

  if positioned.is_err() && behind_hwnd.is_some() {
    // SAFETY: `hwnd` is valid; placing at top of non-topmost band.
    let _ = unsafe { SetWindowPos(hwnd, HWND_TOP, 0, 0, 0, 0, flags) };
  }
}

fn create_32bpp_dib_with<F>(
  width: i32,
  height: i32,
  fill: F,
) -> Option<HBITMAP>
where
  F: FnOnce(*mut std::ffi::c_void, i32, i32),
{
  // SAFETY: `GetDC(None)` acquires the screen DC; released before return.
  let screen_dc = unsafe { GetDC(None) };
  if screen_dc.0 == 0 {
    return None;
  }

  // SAFETY: `CreateCompatibleDC` returns a memory DC matching the screen.
  let mem_dc = unsafe { CreateCompatibleDC(screen_dc) };
  if mem_dc.0 == 0 {
    // SAFETY: `screen_dc` was acquired above via `GetDC(None)`.
    unsafe {
      ReleaseDC(None, screen_dc);
    }
    return None;
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
  let bitmap_res = unsafe {
    CreateDIBSection(
      mem_dc,
      &raw const bmi,
      DIB_RGB_COLORS,
      &raw mut bits,
      None,
      0,
    )
  };

  // SAFETY: Releasing temporary DCs now that `CreateDIBSection` ran.
  unsafe {
    DeleteDC(mem_dc);
    ReleaseDC(None, screen_dc);
  }

  let Ok(bitmap) = bitmap_res else {
    return None;
  };

  if bits.is_null() {
    // SAFETY: Cleaning up empty DIB.
    unsafe {
      DeleteObject(bitmap);
    }
    return None;
  }

  fill(bits, width, height);
  Some(bitmap)
}

fn ensure_glass_bitmap(
  cache: &mut GlassBitmapCache,
  width: i32,
  height: i32,
) -> bool {
  if width <= 0 || height <= 0 {
    return false;
  }

  if cache.width == width
    && cache.height == height
    && cache.bitmap.is_some()
  {
    return true;
  }

  if let Some(old_bitmap) = cache.bitmap.take() {
    // SAFETY: `old_bitmap` is not selected into any DC.
    unsafe {
      DeleteObject(old_bitmap);
    }
  }
  cache.width = 0;
  cache.height = 0;

  let Some(bitmap) =
    create_32bpp_dib_with(width, height, fill_glass_frame_pixels)
  else {
    return false;
  };

  cache.bitmap = Some(bitmap);
  cache.width = width;
  cache.height = height;
  true
}

fn paint_glass_window(hwnd: HWND) {
  let mut ps = PAINTSTRUCT::default();
  // SAFETY: Standard `BeginPaint` / `EndPaint` pair inside `WM_PAINT`.
  let hdc = unsafe { BeginPaint(hwnd, &raw mut ps) };
  if hdc.0 == 0 {
    // SAFETY: `ps` came from `BeginPaint`.
    let _ = unsafe { EndPaint(hwnd, &raw const ps) };
    return;
  }

  let mut window_rect = RECT::default();
  // SAFETY: `hwnd` is a valid overlay window on this thread.
  let has_rect =
    unsafe { GetWindowRect(hwnd, &raw mut window_rect) }.is_ok();
  let (width, height) = if has_rect {
    (
      window_rect.right - window_rect.left,
      window_rect.bottom - window_rect.top,
    )
  } else {
    (
      ps.rcPaint.right - ps.rcPaint.left,
      ps.rcPaint.bottom - ps.rcPaint.top,
    )
  };

  if width > 0 && height > 0 {
    GLASS_BITMAP_CACHE.with(|cache| {
      let mut cache = cache.borrow_mut();
      if !ensure_glass_bitmap(&mut cache, width, height) {
        return;
      }
      let Some(bitmap) = cache.bitmap else {
        return;
      };

      // SAFETY: Creating compatible DC for `BitBlt` into `hdc`.
      let mem_dc = unsafe { CreateCompatibleDC(hdc) };
      if mem_dc.0 == 0 {
        return;
      }

      // SAFETY: Selecting cached 32bpp DIB and copying raw ARGB bytes via
      // `SRCCOPY` so `DwmEnableBlurBehindWindow` blurs alpha=0 interior.
      unsafe {
        let old_obj = SelectObject(mem_dc, bitmap);
        let _ = BitBlt(hdc, 0, 0, width, height, mem_dc, 0, 0, SRCCOPY);
        SelectObject(mem_dc, old_obj);
        DeleteDC(mem_dc);
      }
    });
  }

  // SAFETY: Matches `BeginPaint` above.
  let _ = unsafe { EndPaint(hwnd, &raw const ps) };
}

fn paint_layered_overlay(hwnd: HWND, rect: &Rect) -> bool {
  let width = rect.width();
  let height = rect.height();
  if width <= 0 || height <= 0 {
    return false;
  }

  let Some(bitmap) =
    create_32bpp_dib_with(width, height, fill_aero_glass_pixels)
  else {
    return false;
  };

  // SAFETY: `GetDC(None)` acquires screen DC; released before return.
  let screen_dc = unsafe { GetDC(None) };
  if screen_dc.0 == 0 {
    // SAFETY: `bitmap` is not selected into any DC.
    unsafe {
      DeleteObject(bitmap);
    }
    return false;
  }

  // SAFETY: `CreateCompatibleDC` returns a memory DC matching screen.
  let mem_dc = unsafe { CreateCompatibleDC(screen_dc) };
  if mem_dc.0 == 0 {
    // SAFETY: Releasing screen DC and DIB on failure.
    unsafe {
      ReleaseDC(None, screen_dc);
      DeleteObject(bitmap);
    }
    return false;
  }

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

fn fill_glass_frame_pixels(
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

  for row in 0..height_usize {
    let dist_y = row.min(height_usize - 1 - row);
    let row_offset = row * width_usize;
    for col in 0..width_usize {
      let dist = dist_y.min(col).min(width_usize - 1 - col);
      // Alpha=0 interior allows `DwmEnableBlurBehindWindow` to render
      // real `OpenGlass` Aero glass with a crisp 3px window bevel.
      pixels[row_offset + col] = match dist {
        0 => premultiply_bgra(20, 24, 28, 175),
        1 => premultiply_bgra(255, 255, 255, 220),
        2 => premultiply_bgra(244, 248, 252, 75),
        _ => 0x0000_0000,
      };
    }
  }
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
    WM_PAINT => {
      paint_glass_window(hwnd);
      LRESULT(0)
    }
    _ => DefWindowProcW(hwnd, msg, wparam, lparam),
  }
}
