#[cfg(target_os = "windows")]
use std::time::{Duration, Instant};

use anyhow::Context;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use wm_common::try_warn;
#[cfg(target_os = "windows")]
use wm_common::{
  ActiveDrag, ActiveDragOperation, FloatingStateConfig, WindowState,
};
#[cfg(target_os = "windows")]
use wm_platform::{NativeWindowWindowsExt, Point, Rect, WindowHitTest};
use wm_platform::{MouseButton, MouseEvent};

#[cfg(target_os = "macos")]
use crate::events::handle_window_moved_or_resized_end;
#[cfg(target_os = "windows")]
use crate::{
  commands::window::{
    finalize_move_drag, update_drag_snap_preview, update_window_state,
  },
  events::update_floating_window_position,
  models::WindowContainer,
  wm_state::{LeftButtonDownHit, ManualWindowDrag, PendingMaximizeDrag},
};
use crate::{
  commands::container::set_focused_descendant,
  traits::{CommonGetters, WindowGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

pub fn handle_mouse_move(
  event: &MouseEvent,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  // Ignore mouse move events if the WM is paused. The mouse listener
  // should anyways be disabled when the WM is paused, but this is just in
  // case any events slipped through while disabling.
  if state.is_paused {
    return Ok(());
  }

  #[cfg(target_os = "windows")]
  if handle_windows_mouse_button(event, state, config)? {
    return Ok(());
  }

  // On macOS, detect when a window drag operation has ended by listening
  // to the release of left click.
  //
  // This cannot be used for Windows, since it leads to race conditions
  // where the mouse event comes in before the `MovedOrResized` event with
  // `is_interactive_end`. For example, if the user drags to maximize a
  // window, the WS_MAXIMIZED state is sometimes set after the mouse event.
  #[cfg(target_os = "macos")]
  if let MouseEvent::ButtonUp { button, .. } = event {
    if *button == MouseButton::Left {
      let active_drag_windows = state
        .windows()
        .into_iter()
        .filter(|window| window.active_drag().is_some());

      // Only one window should ever be actively dragged at a time, but
      // just in case, iterate over all active drag windows.
      for window in active_drag_windows {
        let new_rect = try_warn!(window.native().frame());

        window.update_native_properties(|properties| {
          properties.frame = new_rect;
        });

        handle_window_moved_or_resized_end(&window, state, config)?;
      }
    }

    return Ok(());
  }

  if let MouseEvent::Move {
    pressed_buttons,
    // LINT: `window_below_cursor` is only used on macOS.
    #[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
    window_below_cursor,
    position,
    ..
  } = event
  {
    #[cfg(target_os = "windows")]
    if handle_drag_move(position, pressed_buttons, state, config)? {
      return Ok(());
    }

    // Ignore event if left/right-click is down. Otherwise, this causes
    // focus to jitter when a window is being resized by its drag
    // handles. Also ignore if the OS focused window isn't the same as
    // the WM's focused window.
    if pressed_buttons.contains(&MouseButton::Left)
      || pressed_buttons.contains(&MouseButton::Right)
      || !state.is_focus_synced
      || !config.value.general.focus_follows_cursor
    {
      return Ok(());
    }

    let window_under_cursor = {
      #[cfg(target_os = "macos")]
      {
        window_below_cursor.and_then(|window_id| {
          state
            .windows()
            .into_iter()
            .find(|w| w.native().id() == window_id)
        })
      }
      #[cfg(target_os = "windows")]
      {
        resolve_managed_window_at_point(position, state)?
      }
    };

    // Set focus to whichever window is currently under the cursor.
    if let Some(window) = window_under_cursor {
      let focused_container =
        state.focused_container().context("No focused container.")?;

      if focused_container.id() != window.id() {
        set_focused_descendant(&window.as_container(), None);
        state.pending_sync.queue_focus_change();
      }
    } else {
      // Focus the monitor if no window is under the cursor.
      let cursor_monitor = state
        .monitor_at_point(position)
        .context("No monitor under cursor.")?;

      let focused_monitor = state
        .focused_container()
        .context("No focused container.")?
        .monitor()
        .context("Focused container has no monitor.")?;

      // Avoid setting focus to the same monitor.
      if cursor_monitor.id() != focused_monitor.id() {
        set_focused_descendant(&cursor_monitor.as_container(), None);
        state.pending_sync.queue_focus_change();
      }
    }
  }

  Ok(())
}

#[cfg(target_os = "windows")]
fn handle_windows_mouse_button(
  event: &MouseEvent,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<bool> {
  match event {
    MouseEvent::ButtonDown {
      button: MouseButton::Left,
      position,
      ..
    } => {
      handle_windows_left_down(position, state, config)?;
      Ok(true)
    }
    MouseEvent::ButtonUp {
      button: MouseButton::Left,
      position,
      ..
    } => {
      state.last_left_button_down = None;
      state.pending_maximize_drag = None;
      if state.active_manual_drag.is_some() {
        finish_manual_drag(Some(position), state, config)?;
      }
      Ok(true)
    }
    _ => Ok(false),
  }
}

#[cfg(target_os = "windows")]
fn resolve_managed_window_at_point(
  position: &Point,
  state: &WmState,
) -> anyhow::Result<Option<WindowContainer>> {
  const MAX_OWNER_HOPS: usize = 8;

  let Some(native_window) = state.dispatcher.window_from_point(position)?
  else {
    return Ok(None);
  };

  if let Some(window) = state.window_from_native(&native_window) {
    return Ok(Some(window));
  }

  if !native_window.is_non_standalone_overlay()
    || !native_window.is_overlay_hit_passthrough(position)
  {
    return Ok(None);
  }

  let mut current = native_window;
  for _ in 0..MAX_OWNER_HOPS {
    let Some(owner) = current.owner_window() else {
      break;
    };
    if owner.id() == current.id() {
      break;
    }
    if let Some(window) = state.window_from_native(&owner) {
      return Ok(Some(window));
    }
    current = owner;
  }

  Ok(None)
}

#[cfg(target_os = "windows")]
fn is_point_near_resize_border(
  position: &Point,
  frame: &Rect,
  dpi: u32,
) -> bool {
  let dpi_i32 = i32::try_from(dpi).unwrap_or(96);
  let side_margin = ((12 * dpi_i32) / 96).clamp(10, 24);
  let top_margin = ((8 * dpi_i32) / 96).clamp(6, 16);
  let grip_size = ((32 * dpi_i32) / 96).clamp(24, 64);

  let near_left = position.x <= frame.left + side_margin;
  let near_right = position.x >= frame.right - side_margin;
  let near_bottom = position.y >= frame.bottom - side_margin;
  let near_top = position.y <= frame.top + top_margin;
  let in_size_grip = position.x >= frame.right - grip_size
    && position.y >= frame.bottom - grip_size;

  near_left || near_right || near_bottom || near_top || in_size_grip
}

#[cfg(target_os = "windows")]
fn handle_windows_left_down(
  position: &Point,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  state.pending_maximize_drag = None;
  state.last_left_button_down = None;

  if state.active_manual_drag.is_some() {
    finish_manual_drag(Some(position), state, config)?;
  }

  // Skip if another window is already in a native drag.
  if state.windows().iter().any(|w| w.active_drag().is_some()) {
    return Ok(());
  }

  let Some(window) = resolve_managed_window_at_point(position, state)?
  else {
    return Ok(());
  };

  let native = window.native();
  let hit_test = native.hit_test_at_point(position);
  let frame = native
    .frame()
    .unwrap_or_else(|_| window.native_properties().frame);
  let dpi = native.dpi();
  let is_maximized = native.is_maximized().unwrap_or(false);
  let window_handle = native.id();
  drop(native);

  let is_near_resize_border =
    is_point_near_resize_border(position, &frame, dpi);

  state.last_left_button_down = Some(LeftButtonDownHit {
    window_id: window.id(),
    cursor: position.clone(),
    hit_test,
    is_near_resize_border,
    timestamp: Instant::now(),
  });

  if !is_maximized {
    return Ok(());
  }

  let dpi_i32 = i32::try_from(dpi).unwrap_or(96);
  let top_bar_band_px = ((56 * dpi_i32) / 96).clamp(48, 120);

  let (arm_pending, requires_capture_release) = match hit_test {
    WindowHitTest::Caption => (true, false),
    WindowHitTest::Client => {
      let in_top_band = position.y >= frame.top
        && position.y <= frame.top + top_bar_band_px;
      (in_top_band, true)
    }
    WindowHitTest::ResizeBorder | WindowHitTest::Other => (false, false),
  };

  if !arm_pending {
    return Ok(());
  }

  let (start_capture_seq, started_captured) = if requires_capture_release {
    state.dispatcher.capture_snapshot(window_handle)
  } else {
    (0, false)
  };

  state.pending_maximize_drag = Some(PendingMaximizeDrag {
    window_id: window.id(),
    window_handle,
    start_cursor: position.clone(),
    max_frame: frame,
    requires_capture_release,
    start_capture_seq,
    started_captured,
  });

  Ok(())
}

#[cfg(target_os = "windows")]
fn handle_active_manual_drag(
  drag: &ManualWindowDrag,
  position: &Point,
  pressed_buttons: &wm_platform::PressedButtons,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<bool> {
  if !pressed_buttons.contains(&MouseButton::Left)
    || !state.dispatcher.is_mouse_down(&MouseButton::Left)
  {
    finish_manual_drag(Some(position), state, config)?;
    return Ok(false);
  }

  let Some(window) = state.window_by_id(drag.window_id) else {
    state.active_manual_drag = None;
    state.dispatcher.hide_snap_overlay();
    return Ok(false);
  };

  let native = window.native();
  if !native.is_valid() {
    state.active_manual_drag = None;
    state.dispatcher.hide_snap_overlay();
    return Ok(false);
  }

  let target_x = position.x - drag.cursor_offset_x;
  let target_y = position.y - drag.cursor_offset_y;
  let is_maximized = native.is_maximized().unwrap_or(false);
  let mut did_sw_restore_fallback = drag.did_sw_restore_fallback;
  let mut did_confirm_restored_size = drag.did_confirm_restored_size;

  if is_maximized {
    if !did_sw_restore_fallback
      && drag.started_at.elapsed() >= Duration::from_millis(40)
    {
      let _ = native.restore(None);
      did_sw_restore_fallback = true;
    }

    let target_rect = Rect::from_xy(
      target_x,
      target_y,
      drag.restored_width,
      drag.restored_height,
    );
    let _ = native.set_bounds_preserving_zorder(&target_rect);
  } else if !did_confirm_restored_size {
    let current_rect = native
      .frame_with_shadows()
      .unwrap_or_else(|_| window.native_properties().frame);
    if (current_rect.width() - drag.restored_width).abs() <= 40
      && (current_rect.height() - drag.restored_height).abs() <= 40
    {
      did_confirm_restored_size = true;
      let _ = native.move_without_resize(target_x, target_y);
    } else {
      let target_rect = Rect::from_xy(
        target_x,
        target_y,
        drag.restored_width,
        drag.restored_height,
      );
      let _ = native.set_bounds_preserving_zorder(&target_rect);
    }
  } else {
    let _ = native.move_without_resize(target_x, target_y);
  }

  let shadow_borders = if is_maximized {
    None
  } else {
    native.shadow_borders().ok()
  };
  drop(native);

  if let Some(active_drag) = state.active_manual_drag.as_mut() {
    active_drag.did_sw_restore_fallback = did_sw_restore_fallback;
    active_drag.did_confirm_restored_size = did_confirm_restored_size;
  }

  if let Some(shadow_borders) = shadow_borders {
    window.update_native_properties(|properties| {
      properties.is_maximized = false;
      properties.shadow_borders = shadow_borders;
    });
  }

  update_drag_snap_preview(&window, Some(position), state)?;
  Ok(true)
}

#[cfg(target_os = "windows")]
fn handle_drag_move(
  position: &Point,
  pressed_buttons: &wm_platform::PressedButtons,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<bool> {
  const DRAG_THRESHOLD_PX: i32 = 10;

  if let Some(drag) = state.active_manual_drag.clone() {
    return handle_active_manual_drag(
      &drag,
      position,
      pressed_buttons,
      state,
      config,
    );
  }

  let Some(pending) = state.pending_maximize_drag.clone() else {
    return Ok(false);
  };

  if !pressed_buttons.contains(&MouseButton::Left)
    || !state.dispatcher.is_mouse_down(&MouseButton::Left)
  {
    state.pending_maximize_drag = None;
    return Ok(false);
  }

  let dx = (position.x - pending.start_cursor.x).abs();
  let dy = (position.y - pending.start_cursor.y).abs();
  if dx < DRAG_THRESHOLD_PX && dy < DRAG_THRESHOLD_PX {
    return Ok(false);
  }

  let Some(window) = state.window_by_id(pending.window_id) else {
    state.pending_maximize_drag = None;
    return Ok(false);
  };

  if pending.requires_capture_release {
    let (had_capture_cycle, had_dragdrop) =
      state.dispatcher.capture_cycle_since(
        pending.window_handle,
        pending.start_capture_seq,
        pending.started_captured,
      );

    if had_dragdrop {
      state.pending_maximize_drag = None;
      return Ok(false);
    }

    if window.active_drag().is_some()
      || window.native().is_in_native_movesize()
      || !window.native().is_maximized().unwrap_or(false)
    {
      state.pending_maximize_drag = None;
      return Ok(false);
    }

    if !had_capture_cycle
      || !window.native().is_ready_for_client_maximize_drag()
    {
      return Ok(false);
    }

    state.pending_maximize_drag = None;
    start_maximize_drag(window, &pending, position, state, config)?;
    return Ok(true);
  }

  state.pending_maximize_drag = None;

  // If Windows Snap already entered its native modal drag loop, yield to
  // native tracking instead of duplicating the drag.
  if window.active_drag().is_some()
    || window.native().is_in_native_movesize()
    || !window.native().is_maximized().unwrap_or(false)
  {
    return Ok(false);
  }

  start_maximize_drag(window, &pending, position, state, config)?;
  Ok(true)
}

#[cfg(target_os = "windows")]
fn restored_window_size(
  window: &WindowContainer,
  max_width: i32,
  max_height: i32,
) -> (i32, i32) {
  let fits_restored = |width: i32, height: i32| {
    width >= 250
      && height >= 140
      && (width < max_width - 20 || height < max_height - 20)
  };

  let placement = window.floating_placement();
  if fits_restored(placement.width(), placement.height()) {
    return (placement.width(), placement.height());
  }

  if let Ok((normal_width, normal_height)) =
    window.native().normal_frame_size()
  {
    let borders = window.native_properties().shadow_borders;
    let border_width =
      borders.left.to_px(0, None) + borders.right.to_px(0, None);
    let border_height =
      borders.top.to_px(0, None) + borders.bottom.to_px(0, None);
    let width = normal_width - border_width;
    let height = normal_height - border_height;
    if fits_restored(width, height) {
      return (width, height);
    }
  }

  ((max_width * 7) / 10, (max_height * 75) / 100)
}

#[cfg(target_os = "windows")]
fn start_maximize_drag(
  window: WindowContainer,
  pending: &PendingMaximizeDrag,
  position: &Point,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let max_width = pending.max_frame.width().max(320);
  let max_height = pending.max_frame.height().max(200);
  let (restored_width, restored_height) =
    restored_window_size(&window, max_width, max_height);

  let restored_width =
    restored_width.clamp(250, (max_width - 20).max(250));
  let restored_height =
    restored_height.clamp(140, (max_height - 20).max(140));

  #[allow(clippy::cast_precision_loss)]
  let cursor_offset =
    (pending.start_cursor.x - pending.max_frame.left) as f32;
  #[allow(clippy::cast_precision_loss)]
  let rel_x = (cursor_offset / max_width as f32).clamp(0.15, 0.85);
  let grab_offset_y =
    (pending.start_cursor.y - pending.max_frame.top).clamp(8, 36);

  #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
  let offset_x = (restored_width as f32 * rel_x).round() as i32;
  let frame_x = position.x - offset_x;
  let frame_y = position.y - grab_offset_y;
  let restored_frame =
    Rect::from_xy(frame_x, frame_y, restored_width, restored_height);

  let border_delta = window.total_border_delta()?;
  let outer_rect = restored_frame.apply_delta(&border_delta, None);

  let native = window.native();
  native.restore(Some(&outer_rect))?;
  let _ = native.set_bounds_preserving_zorder(&outer_rect);
  drop(native);

  window.set_floating_placement(restored_frame.clone());
  window.set_has_custom_floating_placement(true);
  window.update_native_properties(|properties| {
    properties.is_maximized = false;
    properties.frame = restored_frame.clone();
    properties.pending_maximize_until = None;
  });

  window.set_active_drag(Some(ActiveDrag {
    operation: Some(ActiveDragOperation::Move),
    is_from_floating: true,
    initial_position: restored_frame,
    origin_void_id: None,
  }));

  let window = update_window_state(
    window,
    WindowState::Floating(FloatingStateConfig {
      centered: false,
      ..config.value.window_behavior.state_defaults.floating
    }),
    state,
    config,
  )?;

  state.pending_sync.dequeue_container_from_redraw(window.clone());

  state.active_manual_drag = Some(ManualWindowDrag {
    window_id: window.id(),
    cursor_offset_x: position.x - outer_rect.left,
    cursor_offset_y: position.y - outer_rect.top,
    restored_width: outer_rect.width(),
    restored_height: outer_rect.height(),
    started_at: Instant::now(),
    did_sw_restore_fallback: false,
    did_confirm_restored_size: false,
  });

  Ok(())
}

#[cfg(target_os = "windows")]
fn finish_manual_drag(
  cursor_override: Option<&Point>,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  state.dispatcher.hide_snap_overlay();

  let Some(drag) = state.active_manual_drag.take() else {
    return Ok(());
  };

  let Some(window) = state.window_by_id(drag.window_id) else {
    state.clear_window_drag_state(drag.window_id);
    return Ok(());
  };

  if !window.native().is_valid() {
    window.set_active_drag(None);
    return Ok(());
  }

  let frame = try_warn!(window.native().frame());
  let shadow_borders = try_warn!(window.native().shadow_borders());
  let is_maximized = try_warn!(window.native().is_maximized());

  window.update_native_properties(|properties| {
    properties.frame = frame.clone();
    properties.is_maximized = is_maximized;
    if is_maximized {
      properties.pending_maximize_until = None;
    } else {
      properties.shadow_borders = shadow_borders;
    }
  });

  let active_drag = window.active_drag();

  if let (
    Some(active_drag),
    WindowContainer::NonTilingWindow(non_tiling),
  ) = (active_drag, &window)
  {
    let finished_id = non_tiling.id();
    finalize_move_drag(
      non_tiling,
      frame,
      &active_drag,
      cursor_override,
      state,
      config,
    )?;
    state.clear_window_drag_state(finished_id);
  } else {
    let finished_id = window.id();
    window.set_active_drag(None);
    state.clear_window_drag_state(finished_id);

    if let WindowContainer::NonTilingWindow(non_tiling) = window {
      let nearest_monitor = state
        .nearest_monitor(&non_tiling.native())
        .context("Failed to get workspace of nearest monitor.")?;

      update_floating_window_position(
        &non_tiling,
        frame,
        &nearest_monitor,
        state,
      )?;
    }
  }

  Ok(())
}
