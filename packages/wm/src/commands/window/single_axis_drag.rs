use anyhow::Context;
use wm_common::{
  ActiveDrag, ActiveDragOperation, FloatingStateConfig, WindowState,
};
#[cfg(target_os = "windows")]
use wm_platform::NativeWindowWindowsExt;
use wm_platform::{MouseButton, Point, Rect};

#[cfg(target_os = "windows")]
use super::{compute_drop_preview_rect_at, DropTarget, ScreenZone};
use super::{
  absorb_void_into_sibling, detect_single_axis_breakaway_zone,
  finalize_move_drag, normalize_workspace_voids, reorder_tiling_sibling,
  update_window_state,
};
use crate::{
  commands::container::set_focused_descendant,
  models::{
    Container, DirectionContainer, TilingContainer, TilingWindow, VoidSlot,
    WindowContainer, Workspace,
  },
  traits::{
    CommonGetters, PositionGetters, TilingDirectionGetters,
    TilingSizeGetters, WindowGetters,
  },
  user_config::UserConfig,
  wm_state::{SingleAxisDrag, SingleAxisKind, WmState},
};

const SINGLE_AXIS_THRESHOLD_PX: i32 = 10;
const SINGLE_AXIS_TOLERANCE_PX: i32 = 6;
const SINGLE_AXIS_MIN_DIFF_PX: i32 = 40;
const VOID_ABSORB_SNAP_PX: i32 = 14;

/// Returns whether `window` spans the entire workspace along one axis
/// while occupying a strict subset of the other axis, making it
/// eligible for single-axis in-grid sliding.
pub fn single_axis_kind_for_window(
  window: &TilingWindow,
) -> anyhow::Result<Option<SingleAxisKind>> {
  let Some(workspace) = window.workspace() else {
    return Ok(None);
  };

  let ws_rect = workspace.multi_window_workspace_rect()?;
  let tile_rect = window.to_rect()?;

  let h_diff = (tile_rect.height() - ws_rect.height()).abs();
  let w_diff = (tile_rect.width() - ws_rect.width()).abs();
  let is_full_height = h_diff <= SINGLE_AXIS_TOLERANCE_PX;
  let is_full_width = w_diff <= SINGLE_AXIS_TOLERANCE_PX;
  let is_sub_width =
    tile_rect.width() < ws_rect.width() - SINGLE_AXIS_MIN_DIFF_PX;
  let is_sub_height =
    tile_rect.height() < ws_rect.height() - SINGLE_AXIS_MIN_DIFF_PX;

  if is_full_height && is_sub_width {
    Ok(Some(SingleAxisKind::Horizontal))
  } else if is_full_width && is_sub_height {
    Ok(Some(SingleAxisKind::Vertical))
  } else {
    Ok(None)
  }
}

/// Attempts to initiate a single-axis in-grid move drag for a
/// full-height or full-width `TilingWindow`.
pub fn try_start_single_axis_drag(
  window: &TilingWindow,
  start_cursor: Option<&Point>,
  state: &mut WmState,
) -> anyhow::Result<bool> {
  if state.active_single_axis_drag.is_some() {
    return Ok(false);
  }

  if state.dispatcher.has_event_loop()
    && !state.dispatcher.is_mouse_down(&MouseButton::Left)
  {
    return Ok(false);
  }

  let Some(axis) = single_axis_kind_for_window(window)? else {
    return Ok(false);
  };

  let Some(workspace) = window.workspace() else {
    return Ok(false);
  };
  let Some(monitor) = workspace.monitor() else {
    return Ok(false);
  };

  normalize_workspace_voids(&workspace)?;

  let expected_dir = match axis {
    SingleAxisKind::Horizontal => wm_common::TilingDirection::Horizontal,
    SingleAxisKind::Vertical => wm_common::TilingDirection::Vertical,
  };

  if !window
    .parent()
    .is_some_and(|parent| parent.id() == workspace.id())
    || workspace.tiling_direction() != expected_dir
  {
    return Ok(false);
  }

  let initial_tile_rect = window.to_rect()?;
  let initial_outer_rect =
    initial_tile_rect.apply_delta(&window.total_border_delta()?, None);

  let cursor = start_cursor.cloned().unwrap_or_else(|| Point {
    x: (initial_tile_rect.left + initial_tile_rect.right) / 2,
    y: initial_tile_rect.top + 16,
  });

  let cursor_offset_axis = match axis {
    SingleAxisKind::Horizontal => cursor.x - initial_tile_rect.x(),
    SingleAxisKind::Vertical => cursor.y - initial_tile_rect.y(),
  };

  state.active_single_axis_drag = Some(SingleAxisDrag {
    window_id: window.id(),
    workspace_id: workspace.id(),
    monitor_id: monitor.id(),
    axis,
    start_cursor: cursor.clone(),
    initial_tile_rect: initial_tile_rect.clone(),
    initial_outer_rect: initial_outer_rect.clone(),
    cursor_offset_axis,
    cursor_offset_x: cursor.x - initial_outer_rect.left,
    cursor_offset_y: cursor.y - initial_outer_rect.top,
    has_crossed_threshold: false,
    last_swap_guard: None,
    is_cross_monitor: false,
  });

  window.set_active_drag(Some(ActiveDrag {
    operation: Some(ActiveDragOperation::Move),
    is_from_floating: false,
    initial_position: initial_tile_rect,
    origin_void_id: None,
  }));

  #[cfg(target_os = "windows")]
  if state.dispatcher.has_event_loop() {
    let native = window.native();
    native.cancel_native_movesize();
    let _ = native.raise_to_top_at_bounds(&initial_outer_rect);
    drop(native);
  }

  Ok(true)
}

/// Computes the clamped single-axis visual rect for `drag` at `cursor`.
#[must_use]
pub fn single_axis_tile_rect(
  drag: &SingleAxisDrag,
  ws_rect: &Rect,
  cursor: &Point,
) -> Rect {
  match drag.axis {
    SingleAxisKind::Horizontal => {
      let tile_w = drag.initial_tile_rect.width();
      let max_x = (ws_rect.right - tile_w).max(ws_rect.left);
      let x = (cursor.x - drag.cursor_offset_axis)
        .clamp(ws_rect.left, max_x);
      Rect::from_xy(
        x,
        drag.initial_tile_rect.y(),
        drag.initial_tile_rect.width(),
        drag.initial_tile_rect.height(),
      )
    }
    SingleAxisKind::Vertical => {
      let tile_h = drag.initial_tile_rect.height();
      let max_y = (ws_rect.bottom - tile_h).max(ws_rect.top);
      let y = (cursor.y - drag.cursor_offset_axis)
        .clamp(ws_rect.top, max_y);
      Rect::from_xy(
        drag.initial_tile_rect.x(),
        y,
        drag.initial_tile_rect.width(),
        drag.initial_tile_rect.height(),
      )
    }
  }
}

/// Updates an active single-axis drag in response to raw mouse movement on
/// Windows. Returns `Ok(true)` if the event was consumed.
#[cfg(target_os = "windows")]
#[allow(clippy::too_many_lines)]
pub fn handle_active_single_axis_drag(
  position: &Point,
  pressed_buttons: &wm_platform::PressedButtons,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<bool> {
  let Some(mut drag) = state.active_single_axis_drag.clone() else {
    return Ok(false);
  };

  if !pressed_buttons.contains(&MouseButton::Left)
    || !state.dispatcher.is_mouse_down(&MouseButton::Left)
  {
    finish_single_axis_drag(Some(position), state, config)?;
    return Ok(false);
  }

  let Some(WindowContainer::TilingWindow(tiling_win)) =
    state.window_by_id(drag.window_id)
  else {
    state.active_single_axis_drag = None;
    state.dispatcher.hide_snap_overlay();
    return Ok(false);
  };

  let Some(workspace) = state.workspace_by_id(drag.workspace_id) else {
    state.active_single_axis_drag = None;
    state.dispatcher.hide_snap_overlay();
    return Ok(false);
  };

  if !drag.has_crossed_threshold {
    let dx = (position.x - drag.start_cursor.x).abs();
    let dy = (position.y - drag.start_cursor.y).abs();
    if dx < SINGLE_AXIS_THRESHOLD_PX && dy < SINGLE_AXIS_THRESHOLD_PX {
      return Ok(true);
    }
    drag.has_crossed_threshold = true;
  }

  let cursor_monitor = state.monitor_at_point(position);
  let on_origin_monitor =
    cursor_monitor.as_ref().is_some_and(|m| m.id() == drag.monitor_id);

  if !on_origin_monitor {
    drag.is_cross_monitor = true;
    state.active_single_axis_drag = Some(drag.clone());

    let new_x = position.x - drag.cursor_offset_x;
    let new_y = position.y - drag.cursor_offset_y;
    let native = tiling_win.native();
    let _ = native.move_without_resize(new_x, new_y);
    let updated_frame = native.frame();
    let native_handle = native.id();
    drop(native);

    if let Ok(frame) = updated_frame {
      tiling_win.update_native_properties(|props| {
        props.frame = frame;
      });
    }

    if let Some(target_mon) = cursor_monitor {
      let mon_props = target_mon.native_properties();
      if let Some(target_ws) = target_mon.displayed_workspace() {
        let zone = super::detect_screen_zone(
          position,
          &mon_props.working_area,
          Some(&mon_props.bounds),
        );
        if let Some(zone) = zone {
          let target = match zone {
            ScreenZone::TopCenter => DropTarget::Maximize {
              workspace_id: target_ws.id(),
            },
            _ => DropTarget::SnapZone {
              workspace_id: target_ws.id(),
              zone,
            },
          };
          let pseudo_drag = ActiveDrag {
            operation: Some(ActiveDragOperation::Move),
            is_from_floating: false,
            initial_position: drag.initial_tile_rect.clone(),
            origin_void_id: None,
          };
          let win_container: WindowContainer = tiling_win.clone().into();
          if let Ok(Some(rect)) = compute_drop_preview_rect_at(
            &target,
            &win_container,
            &pseudo_drag,
            Some(position),
            state,
          ) {
            state
              .dispatcher
              .show_snap_overlay(&rect, Some(native_handle));
            return Ok(true);
          }
        }
      }
    }

    state.dispatcher.hide_snap_overlay();
    return Ok(true);
  }

  if drag.is_cross_monitor {
    drag.is_cross_monitor = false;
  }

  let ws_rect = workspace.multi_window_workspace_rect()?;
  let tile_rect = single_axis_tile_rect(&drag, &ws_rect, position);
  let outer_rect =
    tile_rect.apply_delta(&tiling_win.total_border_delta()?, None);

  let native = tiling_win.native();
  let _ = native.move_without_resize(outer_rect.x(), outer_rect.y());
  let updated_frame = native.frame();
  let native_handle = native.id();
  drop(native);

  if let Ok(frame) = updated_frame {
    tiling_win.update_native_properties(|props| {
      props.frame = frame;
    });
  }

  let origin_mon_props = workspace
    .monitor()
    .map(|mon| mon.native_properties())
    .context("Workspace has no parent monitor.")?;

  let breakaway_zone = detect_single_axis_breakaway_zone(
    position,
    &origin_mon_props.working_area,
    Some(&origin_mon_props.bounds),
    drag.axis,
    &drag.start_cursor,
  );

  if let Some(zone) = breakaway_zone {
    let target = match zone {
      ScreenZone::TopCenter => DropTarget::Maximize {
        workspace_id: workspace.id(),
      },
      _ => DropTarget::SnapZone {
        workspace_id: workspace.id(),
        zone,
      },
    };
    let pseudo_drag = ActiveDrag {
      operation: Some(ActiveDragOperation::Move),
      is_from_floating: false,
      initial_position: drag.initial_tile_rect.clone(),
      origin_void_id: None,
    };
    let win_container: WindowContainer = tiling_win.clone().into();
    if let Some(rect) = compute_drop_preview_rect_at(
      &target,
      &win_container,
      &pseudo_drag,
      Some(position),
      state,
    )? {
      state
        .dispatcher
        .show_snap_overlay(&rect, Some(native_handle));
    } else {
      state.dispatcher.hide_snap_overlay();
    }
    state.active_single_axis_drag = Some(drag);
    return Ok(true);
  }

  state.dispatcher.hide_snap_overlay();

  let target_pos = match drag.axis {
    SingleAxisKind::Horizontal => tile_rect.x(),
    SingleAxisKind::Vertical => tile_rect.y(),
  };

  step_single_axis_grid(
    &tiling_win,
    &workspace,
    target_pos,
    position,
    &mut drag,
    state,
    config,
  )?;

  state.active_single_axis_drag = Some(drag);
  Ok(true)
}

/// Reorders `window` against real siblings and slides it into/past
/// adjacent `VoidSlot`s along the locked axis without altering `window`'s
/// size or leaving `WindowState::Tiling`.
#[allow(clippy::too_many_lines)]
pub fn step_single_axis_grid(
  window: &TilingWindow,
  workspace: &Workspace,
  target_pos: i32,
  cursor: &Point,
  drag: &mut SingleAxisDrag,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let is_horiz = matches!(drag.axis, SingleAxisKind::Horizontal);
  let ws_rect = workspace.multi_window_workspace_rect()?;
  let (h_gap, v_gap) = workspace.workspace_inner_gaps()?;
  let gap = if is_horiz { h_gap } else { v_gap };
  let parent_axis_px = axis_len(&ws_rect, is_horiz);
  let parent: DirectionContainer = workspace.clone().into();
  let cursor_axis = if is_horiz { cursor.x } else { cursor.y };

  let old_rects: Vec<(uuid::Uuid, Rect)> = workspace
    .tiling_windows()
    .filter(|w| w.id() != window.id())
    .filter_map(|w| w.to_rect().ok().map(|r| (w.id(), r)))
    .collect();

  for _ in 0..8 {
    let curr_rect = window.to_rect()?;
    let curr_pos = axis_start(&curr_rect, is_horiz);
    let win_span = axis_len(&curr_rect, is_horiz);
    let delta = target_pos - curr_pos;
    if delta == 0 {
      break;
    }

    let neighbor = if delta > 0 {
      window
        .next_siblings()
        .find_map(|s| s.as_tiling_container().ok())
    } else {
      window
        .prev_siblings()
        .find_map(|s| s.as_tiling_container().ok())
    };

    let Some(neighbor) = neighbor else {
      break;
    };

    if !neighbor.is_void() {
      let n_rect = neighbor.to_rect()?;
      let n_start = axis_start(&n_rect, is_horiz);
      let n_end = axis_end(&n_rect, is_horiz);
      let n_mid = (n_start + n_end) / 2;
      let n_span = n_end - n_start;
      let edge_thresh = (n_span / 3).clamp(24, 100);
      let dir_sign = if delta > 0 { 1 } else { -1 };

      let guard_blocks = drag.last_swap_guard.is_some_and(
        |(id, prev_dir, prev_cursor)| {
          id == neighbor.id()
            && prev_dir == -dir_sign
            && (cursor_axis - prev_cursor) * dir_sign < 40
        },
      );

      let target_mid = target_pos + win_span / 2;
      let target_end = target_pos + win_span;
      let should_swap = if delta > 0 {
        cursor_axis >= n_mid
          || target_mid >= n_mid
          || target_end >= n_end - edge_thresh
      } else {
        cursor_axis <= n_mid
          || target_mid <= n_mid
          || target_pos <= n_start + edge_thresh
      };

      if guard_blocks || !should_swap {
        break;
      }

      reorder_tiling_sibling(&window.clone().into(), &neighbor)?;
      drag.last_swap_guard = Some((neighbor.id(), dir_sign, cursor_axis));
      continue;
    }

    let Some(void_ahead) = neighbor.as_void().cloned() else {
      break;
    };

    let absorbed_or_stepped = step_into_adjacent_void(
      window,
      &void_ahead,
      &parent,
      &curr_rect,
      delta,
      gap,
      parent_axis_px,
      is_horiz,
      config,
    )?;

    if !absorbed_or_stepped {
      break;
    }
  }

  for (win_id, old_rect) in old_rects {
    if let Some(other) =
      workspace.tiling_windows().find(|w| w.id() == win_id)
    {
      if other.to_rect().ok().as_ref() != Some(&old_rect) {
        state.pending_sync.queue_container_to_redraw(other);
      }
    }
  }

  Ok(())
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn step_into_adjacent_void(
  window: &TilingWindow,
  void_ahead: &VoidSlot,
  parent: &DirectionContainer,
  curr_rect: &Rect,
  delta: i32,
  gap: i32,
  parent_axis_px: i32,
  is_horiz: bool,
  config: &UserConfig,
) -> anyhow::Result<bool> {
  let move_forward = delta > 0;
  let abs_delta = delta.abs();
  let v_ahead_rect = void_ahead.to_rect()?;

  let total_slide = if move_forward {
    axis_end(&v_ahead_rect, is_horiz) - axis_end(curr_rect, is_horiz)
  } else {
    axis_start(curr_rect, is_horiz) - axis_start(&v_ahead_rect, is_horiz)
  };
  if total_slide <= 0 {
    return Ok(false);
  }

  let snap_all = total_slide - abs_delta <= VOID_ABSORB_SNAP_PX;
  let step = if snap_all {
    total_slide
  } else {
    abs_delta.min(total_slide)
  };

  let adjacent_sibling = if move_forward {
    window
      .prev_siblings()
      .find_map(|s| s.as_tiling_container().ok())
  } else {
    window
      .next_siblings()
      .find_map(|s| s.as_tiling_container().ok())
  };
  let trailing_void = adjacent_sibling.and_then(|s| s.as_void().cloned());

  let siblings: Vec<TilingContainer> = parent.tiling_children().collect();
  #[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
  )]
  let old_avail =
    (parent_axis_px - gap * (siblings.len() as i32 - 1)).max(1) as f32;
  #[allow(clippy::cast_precision_loss)]
  let step_f = step as f32;
  let ahead_old_f = void_ahead.tiling_size() * old_avail;

  if let Some(v_behind) = trailing_void {
    if step >= total_slide || ahead_old_f - step_f < 4.0 {
      absorb_void_into_sibling(
        void_ahead,
        &v_behind.into(),
        parent,
        gap,
        parent_axis_px,
      )?;
      return Ok(true);
    }

    let total_pair = v_behind.tiling_size() + void_ahead.tiling_size();
    let min_void = (4.0 / old_avail).max(0.001);
    if total_pair <= min_void * 2.0 {
      return Ok(false);
    }

    let raw_behind = v_behind.tiling_size() + step_f / old_avail;
    let next_behind = raw_behind.clamp(min_void, total_pair - min_void);
    let next_ahead = total_pair - next_behind;
    v_behind.set_tiling_size(next_behind);
    void_ahead.set_tiling_size(next_ahead);
    return Ok(false);
  }

  let min_create_step = (gap + 12).max(14);
  if step >= total_slide || ahead_old_f - step_f < 4.0 {
    reorder_tiling_sibling(
      &window.clone().into(),
      &void_ahead.clone().into(),
    )?;
    return Ok(true);
  }

  if step < min_create_step {
    return Ok(false);
  }

  #[allow(clippy::cast_precision_loss)]
  let gap_f = gap as f32;
  let ahead_new_f = ahead_old_f - step_f;
  let behind_f = ahead_old_f - gap_f - ahead_new_f;
  if behind_f < 4.0 || ahead_new_f < 4.0 {
    return Ok(false);
  }

  #[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
  )]
  let new_avail =
    (parent_axis_px - gap * siblings.len() as i32).max(1) as f32;

  for child in &siblings {
    if child.id() != void_ahead.id() {
      child.set_tiling_size((child.tiling_size() * old_avail) / new_avail);
    }
  }

  void_ahead.set_tiling_size(ahead_new_f / new_avail);

  let v_behind = VoidSlot::new(config.value.gaps.clone());
  v_behind.set_tiling_size(behind_f / new_avail);

  let insert_idx = if move_forward {
    window.index()
  } else {
    window.index() + 1
  };
  let v_behind_container: Container = v_behind.into();
  let clamped_idx = insert_idx.min(parent.child_count());
  parent
    .borrow_children_mut()
    .insert(clamped_idx, v_behind_container.clone());
  *v_behind_container.borrow_parent_mut() = Some(parent.clone().into());

  Ok(false)
}

/// Finalizes an active single-axis tiled move drag on mouse release or
/// drag cancellation, committing in-grid slots (keeping
/// `WindowState::Tiling`) or executing any active breakaway /
/// cross-monitor snap drop.
#[allow(clippy::too_many_lines)]
pub fn finish_single_axis_drag(
  cursor_override: Option<&Point>,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let Some(mut drag) = state.active_single_axis_drag.take() else {
    return Ok(());
  };

  state.dispatcher.hide_snap_overlay();
  state.clear_window_drag_state(drag.window_id);

  let Some(WindowContainer::TilingWindow(tiling_win)) =
    state.window_by_id(drag.window_id)
  else {
    return Ok(());
  };

  let Some(workspace) = state.workspace_by_id(drag.workspace_id) else {
    tiling_win.set_active_drag(None);
    return Ok(());
  };

  let cursor = cursor_override
    .cloned()
    .or_else(|| {
      if state.dispatcher.has_event_loop() {
        state.dispatcher.cursor_position().ok()
      } else {
        None
      }
    })
    .unwrap_or_else(|| drag.start_cursor.clone());

  let cursor_monitor = state.monitor_at_point(&cursor);
  let on_origin_monitor =
    cursor_monitor.as_ref().is_some_and(|m| m.id() == drag.monitor_id);

  let breakaway_zone = if drag.has_crossed_threshold && on_origin_monitor {
    workspace.monitor().and_then(|mon| {
      let props = mon.native_properties();
      detect_single_axis_breakaway_zone(
        &cursor,
        &props.working_area,
        Some(&props.bounds),
        drag.axis,
        &drag.start_cursor,
      )
    })
  } else {
    None
  };

  let should_breakaway = drag.has_crossed_threshold
    && (!on_origin_monitor || breakaway_zone.is_some());

  if should_breakaway {
    let updated = update_window_state(
      tiling_win.clone().into(),
      WindowState::Floating(FloatingStateConfig {
        centered: false,
        ..config.value.window_behavior.state_defaults.floating.clone()
      }),
      state,
      config,
    )?;

    if let WindowContainer::NonTilingWindow(non_tiling) = updated {
      let active_drag = non_tiling.active_drag().unwrap_or(ActiveDrag {
        operation: Some(ActiveDragOperation::Move),
        is_from_floating: false,
        initial_position: drag.initial_tile_rect,
        origin_void_id: None,
      });
      let frame = non_tiling.native_properties().frame;
      finalize_move_drag(
        &non_tiling,
        frame,
        &active_drag,
        Some(&cursor),
        state,
        config,
      )?;
    }

    return Ok(());
  }

  if drag.has_crossed_threshold {
    let ws_rect = workspace.multi_window_workspace_rect()?;
    let tile_rect = single_axis_tile_rect(&drag, &ws_rect, &cursor);
    let target_pos = match drag.axis {
      SingleAxisKind::Horizontal => tile_rect.x(),
      SingleAxisKind::Vertical => tile_rect.y(),
    };
    step_single_axis_grid(
      &tiling_win,
      &workspace,
      target_pos,
      &cursor,
      &mut drag,
      state,
      config,
    )?;
  }

  normalize_workspace_voids(&workspace)?;
  tiling_win.set_active_drag(None);
  set_focused_descendant(&tiling_win.into(), None);

  state
    .pending_sync
    .queue_containers_to_redraw(workspace.tiling_children())
    .queue_workspace_to_reorder(workspace);

  Ok(())
}

const fn axis_start(rect: &Rect, is_horiz: bool) -> i32 {
  if is_horiz {
    rect.left
  } else {
    rect.top
  }
}

const fn axis_end(rect: &Rect, is_horiz: bool) -> i32 {
  if is_horiz {
    rect.right
  } else {
    rect.bottom
  }
}

const fn axis_len(rect: &Rect, is_horiz: bool) -> i32 {
  if is_horiz {
    rect.right - rect.left
  } else {
    rect.bottom - rect.top
  }
}

#[cfg(test)]
mod tests {
  use tokio::sync::mpsc;
  use wm_common::WindowState;
  use wm_platform::{Dispatcher, Point, Rect};

  use super::{
    finish_single_axis_drag, single_axis_kind_for_window,
    step_single_axis_grid, try_start_single_axis_drag,
  };
  use crate::{
    commands::container::{attach_container, set_focused_descendant},
    models::{Container, Monitor, TilingWindow, VoidSlot, Workspace},
    traits::{
      CommonGetters, PositionGetters, TilingSizeGetters, WindowGetters,
    },
    user_config::UserConfig,
    wm_state::{SingleAxisKind, WmState},
  };

  fn setup_workspace() -> (WmState, UserConfig, Workspace) {
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let (exit_tx, _exit_rx) = mpsc::unbounded_channel();
    let mut state = WmState::new(Dispatcher::mock(), event_tx, exit_tx);
    let config = UserConfig::mock();

    let workspace = Workspace::mock()
      .gaps_config(config.value.gaps.clone())
      .call();
    let monitor = Monitor::mock()
      .bounds(Rect::from_xy(0, 0, 1920, 1080))
      .working_area(Rect::from_xy(0, 0, 1920, 1032))
      .workspaces(vec![workspace.clone()])
      .call();
    attach_container(
      &monitor.into(),
      &state.root_container.clone().into(),
      None,
    )
    .unwrap();
    set_focused_descendant(&workspace.clone().into(), None);

    (state, config, workspace)
  }

  #[test]
  fn sliding_full_height_tile_away_from_edge_creates_void() {
    let (mut state, config, workspace) = setup_workspace();
    let ws_c: Container = workspace.clone().into();
    let win = TilingWindow::mock()
      .tiling_size(0.5)
      .gaps_config(config.value.gaps.clone())
      .call();
    let void_r = VoidSlot::new(config.value.gaps.clone());
    void_r.set_tiling_size(0.5);
    attach_container(&win.clone().into(), &ws_c, None).unwrap();
    attach_container(&void_r.into(), &ws_c, None).unwrap();

    assert_eq!(
      single_axis_kind_for_window(&win).unwrap(),
      Some(SingleAxisKind::Horizontal)
    );

    let start_cursor = Point { x: 200, y: 18 };
    assert!(try_start_single_axis_drag(
      &win,
      Some(&start_cursor),
      &mut state,
    )
    .unwrap());

    let mut drag = state.active_single_axis_drag.take().unwrap();
    drag.has_crossed_threshold = true;
    let moved_cursor = Point { x: 500, y: 80 };
    step_single_axis_grid(
      &win,
      &workspace,
      300,
      &moved_cursor,
      &mut drag,
      &mut state,
      &config,
    )
    .unwrap();
    state.active_single_axis_drag = Some(drag);

    finish_single_axis_drag(Some(&moved_cursor), &mut state, &config)
      .unwrap();

    let children: Vec<_> = workspace.tiling_children().collect();
    assert_eq!(children.len(), 3);
    assert!(children[0].is_void());
    assert_eq!(children[1].id(), win.id());
    assert!(children[2].is_void());
    assert_eq!(win.state(), WindowState::Tiling);
    assert!((win.to_rect().unwrap().x() - 300).abs() <= 2);
  }

  #[test]
  fn sliding_tile_swaps_siblings_without_exchanging_sizes() {
    let (mut state, config, workspace) = setup_workspace();
    let ws_c: Container = workspace.clone().into();
    let wide = TilingWindow::mock()
      .gaps_config(config.value.gaps.clone())
      .call();
    let narrow = TilingWindow::mock()
      .gaps_config(config.value.gaps.clone())
      .call();
    attach_container(&wide.clone().into(), &ws_c, None).unwrap();
    attach_container(&narrow.clone().into(), &ws_c, None).unwrap();
    wide.set_tiling_size(0.7);
    narrow.set_tiling_size(0.3);

    let start_cursor = Point { x: 600, y: 18 };
    assert!(try_start_single_axis_drag(
      &wide,
      Some(&start_cursor),
      &mut state,
    )
    .unwrap());

    let mut drag = state.active_single_axis_drag.take().unwrap();
    drag.has_crossed_threshold = true;
    let moved_cursor = Point { x: 1180, y: 20 };
    step_single_axis_grid(
      &wide,
      &workspace,
      580,
      &moved_cursor,
      &mut drag,
      &mut state,
      &config,
    )
    .unwrap();
    state.active_single_axis_drag = Some(drag);

    finish_single_axis_drag(Some(&moved_cursor), &mut state, &config)
      .unwrap();

    let children: Vec<_> = workspace.tiling_children().collect();
    assert_eq!(children.len(), 2);
    assert_eq!(children[0].id(), narrow.id());
    assert_eq!(children[1].id(), wide.id());
    assert!((wide.tiling_size() - 0.7).abs() < 0.001);
    assert!((narrow.tiling_size() - 0.3).abs() < 0.001);
  }

  #[test]
  fn single_axis_breakaway_to_top_center_maximizes_both_windows() {
    let (mut state, config, workspace) = setup_workspace();
    let ws_c: Container = workspace.clone().into();
    let win_a = TilingWindow::mock()
      .tiling_size(0.5)
      .gaps_config(config.value.gaps.clone())
      .call();
    let win_b = TilingWindow::mock()
      .tiling_size(0.5)
      .gaps_config(config.value.gaps.clone())
      .call();
    attach_container(&win_a.clone().into(), &ws_c, None).unwrap();
    attach_container(&win_b.clone().into(), &ws_c, None).unwrap();

    let start_cursor = Point { x: 400, y: 18 };
    assert!(try_start_single_axis_drag(
      &win_a,
      Some(&start_cursor),
      &mut state,
    )
    .unwrap());

    if let Some(drag) = state.active_single_axis_drag.as_mut() {
      drag.has_crossed_threshold = true;
    }

    let top_center = Point { x: 960, y: 2 };
    finish_single_axis_drag(Some(&top_center), &mut state, &config)
      .unwrap();

    let updated_a = state.window_by_id(win_a.id()).unwrap();
    let updated_b = state.window_by_id(win_b.id()).unwrap();
    assert!(matches!(updated_a.state(), WindowState::Fullscreen(_)));
    assert!(matches!(updated_b.state(), WindowState::Fullscreen(_)));
  }

  #[test]
  fn sliding_to_opposite_edge_preserves_window_size_and_total_sum() {
    let (mut state, config, workspace) = setup_workspace();
    let ws_c: Container = workspace.clone().into();
    let win = TilingWindow::mock()
      .tiling_size(0.5)
      .gaps_config(config.value.gaps.clone())
      .call();
    let void_r = VoidSlot::new(config.value.gaps.clone());
    void_r.set_tiling_size(0.5);
    attach_container(&win.clone().into(), &ws_c, None).unwrap();
    attach_container(&void_r.into(), &ws_c, None).unwrap();

    let start_rect = win.to_rect().unwrap();
    let start_cursor = Point { x: 200, y: 18 };
    assert!(try_start_single_axis_drag(
      &win,
      Some(&start_cursor),
      &mut state,
    )
    .unwrap());

    let mut drag = state.active_single_axis_drag.take().unwrap();
    drag.has_crossed_threshold = true;

    for target_x in [120, 360, 620, 960] {
      let cursor = Point { x: 200 + target_x, y: 22 };
      step_single_axis_grid(
        &win,
        &workspace,
        target_x,
        &cursor,
        &mut drag,
        &mut state,
        &config,
      )
      .unwrap();
    }
    state.active_single_axis_drag = Some(drag);

    let end_cursor = Point { x: 1160, y: 22 };
    finish_single_axis_drag(Some(&end_cursor), &mut state, &config)
      .unwrap();

    let end_rect = win.to_rect().unwrap();
    let sum: f32 =
      workspace.tiling_children().map(|c| c.tiling_size()).sum();
    assert!((sum - 1.0).abs() < 0.001);
    let ws_right = workspace.to_rect().unwrap().right;
    assert!((end_rect.width() - start_rect.width()).abs() <= 1);
    assert!((end_rect.right - ws_right).abs() <= 2);
  }

  #[test]
  fn pulling_single_axis_away_from_neighbor_leaves_neighbor_intact() {
    let (mut state, config, workspace) = setup_workspace();
    let ws_c: Container = workspace.clone().into();
    let void_l = VoidSlot::new(config.value.gaps.clone());
    let win_a = TilingWindow::mock()
      .gaps_config(config.value.gaps.clone())
      .call();
    let win_b = TilingWindow::mock()
      .gaps_config(config.value.gaps.clone())
      .call();
    let void_r = VoidSlot::new(config.value.gaps.clone());
    attach_container(&void_l.clone().into(), &ws_c, None).unwrap();
    attach_container(&win_a.clone().into(), &ws_c, None).unwrap();
    attach_container(&win_b.clone().into(), &ws_c, None).unwrap();
    attach_container(&void_r.clone().into(), &ws_c, None).unwrap();
    void_l.set_tiling_size(0.2);
    win_a.set_tiling_size(0.25);
    win_b.set_tiling_size(0.25);
    void_r.set_tiling_size(0.3);

    let a_before = win_a.to_rect().unwrap();
    let b_before = win_b.to_rect().unwrap();

    let start_cursor = Point { x: b_before.x() + 50, y: 18 };
    assert!(try_start_single_axis_drag(
      &win_b,
      Some(&start_cursor),
      &mut state,
    )
    .unwrap());

    let mut drag = state.active_single_axis_drag.take().unwrap();
    drag.has_crossed_threshold = true;
    let moved_cursor = Point { x: start_cursor.x + 80, y: 22 };
    step_single_axis_grid(
      &win_b,
      &workspace,
      b_before.x() + 80,
      &moved_cursor,
      &mut drag,
      &mut state,
      &config,
    )
    .unwrap();
    state.active_single_axis_drag = Some(drag);

    finish_single_axis_drag(Some(&moved_cursor), &mut state, &config)
      .unwrap();

    let sum: f32 =
      workspace.tiling_children().map(|c| c.tiling_size()).sum();
    assert!((sum - 1.0).abs() < 0.001);
    assert_eq!(win_a.to_rect().unwrap(), a_before);
    let b_after = win_b.to_rect().unwrap();
    assert!((b_after.width() - b_before.width()).abs() <= 1);
  }
}
