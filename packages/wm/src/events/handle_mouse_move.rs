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
  commands::window::update_window_state,
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
  match event {
    MouseEvent::ButtonDown {
      button: MouseButton::Left,
      position,
      ..
    } => {
      return handle_windows_left_down(position, state, config);
    }
    MouseEvent::ButtonUp {
      button: MouseButton::Left,
      ..
    } => {
      state.pending_maximize_drag = None;
      if state.active_manual_drag.is_some() {
        return finish_manual_drag(state);
      }
      return Ok(());
    }
    _ => {}
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
    if handle_windows_drag_move(
      position,
      pressed_buttons,
      state,
      config,
    )? {
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
fn handle_windows_left_down(
  position: &Point,
  state: &mut WmState,
  _config: &UserConfig,
) -> anyhow::Result<()> {
  state.pending_maximize_drag = None;

  if state.active_manual_drag.is_some() {
    finish_manual_drag(state)?;
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

  let max_frame = window
    .native()
    .frame()
    .unwrap_or_else(|_| window.native_properties().frame);

  state.pending_maximize_drag = Some(PendingMaximizeDrag {
    window_id: window.id(),
    start_cursor: position.clone(),
    max_frame,
  });

  Ok(())
}

#[cfg(target_os = "windows")]
fn handle_windows_drag_move(
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
      finish_manual_drag(state)?;
      return Ok(false);
    }

    let Some(window) = state
      .windows()
      .into_iter()
      .find(|w| w.id() == drag.window_id)
    else {
      state.active_manual_drag = None;
      return Ok(false);
    };

    if !window.native().is_valid() {
      state.active_manual_drag = None;
      return Ok(false);
    }

    let _ = window.native().move_without_resize(
      position.x - drag.cursor_offset_x,
      position.y - drag.cursor_offset_y,
    );

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

  let Some(window) = state
    .windows()
    .into_iter()
    .find(|w| w.id() == pending.window_id)
  else {
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
fn start_maximize_drag(
  window: WindowContainer,
  pending: &PendingMaximizeDrag,
  position: &Point,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let max_w = pending.max_frame.width().max(320);
  let max_h = pending.max_frame.height().max(200);

  let fp = window.floating_placement();
  let (restored_w, restored_h) = if fp.width() >= 250
    && fp.height() >= 140
    && (fp.width() < max_w - 20 || fp.height() < max_h - 20)
  {
    (fp.width(), fp.height())
  } else if let Ok((norm_w, norm_h)) =
    window.native().normal_frame_size()
  {
    let sb = window.native_properties().shadow_borders;
    let w = norm_w - sb.left.to_px(0, None) - sb.right.to_px(0, None);
    let h = norm_h - sb.top.to_px(0, None) - sb.bottom.to_px(0, None);
    if w >= 250 && h >= 140 && (w < max_w - 20 || h < max_h - 20) {
      (w, h)
    } else {
      ((max_w * 7) / 10, (max_h * 75) / 100)
    }
  } else {
    ((max_w * 7) / 10, (max_h * 75) / 100)
  };

  let restored_w = restored_w.clamp(250, (max_w - 20).max(250));
  let restored_h = restored_h.clamp(140, (max_h - 20).max(140));

  #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
  let rel_x = ((pending.start_cursor.x - pending.max_frame.left) as f32
    / max_w as f32)
    .clamp(0.15, 0.85);
  let grab_offset_y =
    (pending.start_cursor.y - pending.max_frame.top).clamp(8, 36);

  #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
  let frame_x = position.x - (restored_w as f32 * rel_x).round() as i32;
  let frame_y = position.y - grab_offset_y;
  let restored_frame =
    Rect::from_xy(frame_x, frame_y, restored_w, restored_h);

  let border_delta = window.total_border_delta()?;
  let outer_rect = restored_frame.apply_delta(&border_delta, None);

  window.native().restore(Some(&outer_rect))?;
  let _ = window
    .native()
    .move_without_resize(outer_rect.left, outer_rect.top);

  window.set_floating_placement(restored_frame.clone());
  window.set_has_custom_floating_placement(true);
  window.update_native_properties(|properties| {
    properties.is_maximized = false;
    properties.frame = restored_frame.clone();
  });

  window.set_active_drag(Some(ActiveDrag {
    operation: Some(ActiveDragOperation::Move),
    is_from_floating: true,
    initial_position: restored_frame,
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
fn finish_manual_drag(state: &mut WmState) -> anyhow::Result<()> {
  let Some(drag) = state.active_manual_drag.take() else {
    return Ok(());
  };

  let Some(window) = state
    .windows()
    .into_iter()
    .find(|w| w.id() == drag.window_id)
  else {
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
    if !is_maximized {
      properties.shadow_borders = shadow_borders;
    }
  });

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

  Ok(())
}
