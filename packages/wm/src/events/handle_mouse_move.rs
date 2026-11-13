use anyhow::Context;
#[cfg(any(target_os = "macos", target_os = "windows"))]
use wm_common::try_warn;
#[cfg(target_os = "windows")]
use wm_common::{
  ActiveDrag, ActiveDragOperation, FloatingStateConfig, WindowState,
};
#[cfg(target_os = "windows")]
use wm_platform::{NativeWindowWindowsExt, Point, Rect};
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
  wm_state::{ManualWindowDrag, PendingMaximizeDrag},
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
        state
          .dispatcher
          .window_from_point(position)?
          .and_then(|native| state.window_from_native(&native))
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
fn handle_windows_left_down(
  position: &Point,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  state.pending_maximize_drag = None;

  if state.active_manual_drag.is_some() {
    finish_manual_drag(Some(position), state, config)?;
  }

  // Skip if another window is already in a native drag.
  if state.windows().iter().any(|w| w.active_drag().is_some()) {
    return Ok(());
  }

  let Some(native_window) = state.dispatcher.window_from_point(position)?
  else {
    return Ok(());
  };

  let Some(window) = state.window_from_native(&native_window) else {
    return Ok(());
  };

  if !window.native().is_maximized().unwrap_or(false) {
    return Ok(());
  }

  if !window.native().is_caption_at_point(position) {
    return Ok(());
  }

  let max_frame = match window.native().frame() {
    Ok(frame) => frame,
    Err(_) => window.native_properties().frame,
  };

  state.pending_maximize_drag = Some(PendingMaximizeDrag {
    window_id: window.id(),
    start_cursor: position.clone(),
    max_frame,
  });

  Ok(())
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

    let _ = native.move_without_resize(
      position.x - drag.cursor_offset_x,
      position.y - drag.cursor_offset_y,
    );
    let is_maximized = native.is_maximized().unwrap_or(false);
    if !is_maximized {
      if let Ok(shadow_borders) = native.shadow_borders() {
        window.update_native_properties(|properties| {
          properties.is_maximized = false;
          properties.shadow_borders = shadow_borders;
        });
      }
    }
    drop(native);

    update_drag_snap_preview(&window, Some(position), state)?;

    return Ok(true);
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

  state.pending_maximize_drag = None;

  let Some(window) = state.window_by_id(pending.window_id) else {
    return Ok(false);
  };

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
  let _ = native.move_without_resize(outer_rect.left, outer_rect.top);
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
    finalize_move_drag(
      non_tiling,
      frame,
      &active_drag,
      cursor_override,
      state,
      config,
    )?;
  } else {
    window.set_active_drag(None);

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
