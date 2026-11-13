use anyhow::Context;
use uuid::Uuid;
use wm_common::{
  ActiveDrag, ActiveDragOperation, FullscreenStateConfig, TilingDirection,
  WindowState,
};
#[cfg(target_os = "windows")]
use wm_platform::{NativeWindowWindowsExt, WS_MAXIMIZEBOX};
use wm_platform::{Point, Rect};

use super::{
  absorb_unfillable_void_slivers, clear_workspace_voids,
  collapse_origin_void, normalize_workspace_voids, replace_tiling_slot,
  swap_tiling_containers, update_window_state, MIN_VOID_SPAN_PX,
};
use crate::{
  commands::container::{
    attach_container, detach_container, set_focused_descendant,
    wrap_in_split_container,
  },
  events::update_floating_window_position,
  models::{
    Container, NonTilingWindow, SplitContainer, TilingContainer,
    TilingWindow, VoidSlot, WindowContainer, Workspace,
  },
  traits::{
    CommonGetters, PositionGetters, TilingDirectionGetters,
    TilingSizeGetters, WindowGetters, MIN_TILING_SIZE,
  },
  user_config::UserConfig,
  wm_state::{SingleAxisKind, WmState},
};

/// Screen-edge and corner zones active during mouse move drags.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ScreenZone {
  TopCenter,
  Left,
  Right,
  Top,
  Bottom,
  TopLeft,
  TopRight,
  BottomLeft,
  BottomRight,
}

/// Side of an existing tiling window when splitting it in place.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum SplitSide {
  Left,
  Right,
  Top,
  Bottom,
}

/// Resolved drop target during a window move drag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropTarget {
  Maximize {
    workspace_id: Uuid,
  },
  SnapZone {
    workspace_id: Uuid,
    zone: ScreenZone,
  },
  SwapTile {
    workspace_id: Uuid,
    target_window_id: Uuid,
  },
  SplitTile {
    workspace_id: Uuid,
    target_window_id: Uuid,
    side: SplitSide,
  },
}

#[derive(Debug, Clone)]
enum VoidMatch {
  Exact(VoidSlot),
  SplitHalf {
    slot: VoidSlot,
    split_dir: TilingDirection,
    insert_first: bool,
  },
}

/// Detects which screen snap zone contains `cursor` within a monitor's
/// `bounds` (falling back to `working_area`).
#[must_use]
pub fn detect_screen_zone(
  cursor: &Point,
  working_area: &Rect,
  monitor_bounds: Option<&Rect>,
) -> Option<ScreenZone> {
  let containment = monitor_bounds.unwrap_or(working_area);
  if !containment.contains_point(cursor) {
    return None;
  }

  let edge_px = (working_area.width().min(working_area.height()) / 50)
    .clamp(14, 24);
  let corner_w = (working_area.width() / 6).clamp(100, 200);
  let corner_h = (working_area.height() / 5).clamp(100, 200);

  let near_left = cursor.x <= working_area.left + edge_px;
  let near_right = cursor.x >= working_area.right - 1 - edge_px;
  let near_top = cursor.y <= working_area.top + edge_px;
  let near_bottom = cursor.y >= working_area.bottom - 1 - edge_px;

  if !near_left && !near_right && !near_top && !near_bottom {
    return None;
  }

  let is_top_left = (near_top
    && (near_left || cursor.x < working_area.left + corner_w))
    || (near_left && cursor.y < working_area.top + corner_h);
  if is_top_left {
    return Some(ScreenZone::TopLeft);
  }

  let is_top_right = (near_top
    && (near_right || cursor.x >= working_area.right - corner_w))
    || (near_right && cursor.y < working_area.top + corner_h);
  if is_top_right {
    return Some(ScreenZone::TopRight);
  }

  let is_bottom_left = (near_bottom
    && (near_left || cursor.x < working_area.left + corner_w))
    || (near_left && cursor.y >= working_area.bottom - corner_h);
  if is_bottom_left {
    return Some(ScreenZone::BottomLeft);
  }

  let is_bottom_right = (near_bottom
    && (near_right || cursor.x >= working_area.right - corner_w))
    || (near_right && cursor.y >= working_area.bottom - corner_h);
  if is_bottom_right {
    return Some(ScreenZone::BottomRight);
  }

  let third_w = working_area.width() / 3;
  let mid_left = working_area.left + third_w;
  let mid_right = working_area.right - third_w;

  if near_top && cursor.x >= mid_left && cursor.x <= mid_right {
    return Some(ScreenZone::TopCenter);
  }

  if near_top {
    return Some(ScreenZone::Top);
  }
  if near_bottom {
    return Some(ScreenZone::Bottom);
  }
  if near_left {
    return Some(ScreenZone::Left);
  }
  if near_right {
    return Some(ScreenZone::Right);
  }

  None
}

/// Detects breakaway snap/maximize zones during a single-axis tiled move
/// drag, gating perpendicular edges so sliding along the locked track to a
/// grid edge never triggers an unintended corner/edge breakaway.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn detect_single_axis_breakaway_zone(
  cursor: &Point,
  working_area: &Rect,
  monitor_bounds: Option<&Rect>,
  axis: SingleAxisKind,
  start_cursor: &Point,
) -> Option<ScreenZone> {
  let containment = monitor_bounds.unwrap_or(working_area);
  if !containment.contains_point(cursor) {
    return None;
  }

  let edge_px = (working_area.width().min(working_area.height()) / 50)
    .clamp(14, 24);
  let corner_w = (working_area.width() / 6).clamp(100, 200);
  let corner_h = (working_area.height() / 5).clamp(100, 200);
  let rail_px = 5;
  let pull_away_px = 36;
  let third_w = working_area.width() / 3;
  let mid_left = working_area.left + third_w;
  let mid_right = working_area.right - third_w;

  match axis {
    SingleAxisKind::Horizontal => {
      let near_top_rail = cursor.y <= working_area.top + rail_px
        && (start_cursor.y > working_area.top + rail_px
          || cursor.y <= working_area.top + 1);
      let near_bottom = cursor.y >= working_area.bottom - 1 - edge_px;

      if near_top_rail && cursor.x < working_area.left + corner_w {
        return Some(ScreenZone::TopLeft);
      }
      if near_top_rail && cursor.x >= working_area.right - corner_w {
        return Some(ScreenZone::TopRight);
      }
      if near_bottom && cursor.x < working_area.left + corner_w {
        return Some(ScreenZone::BottomLeft);
      }
      if near_bottom && cursor.x >= working_area.right - corner_w {
        return Some(ScreenZone::BottomRight);
      }
      if near_top_rail && cursor.x >= mid_left && cursor.x <= mid_right {
        return Some(ScreenZone::TopCenter);
      }
      if near_top_rail {
        return Some(ScreenZone::Top);
      }
      if near_bottom {
        return Some(ScreenZone::Bottom);
      }
      None
    }
    SingleAxisKind::Vertical => {
      let near_top_rail = cursor.y <= working_area.top + rail_px
        && (start_cursor.y > working_area.top + rail_px
          || cursor.y <= working_area.top + 1);
      let near_left_rail = cursor.x <= working_area.left + rail_px
        && (start_cursor.x > working_area.left + rail_px
          || cursor.x <= working_area.left + 1);
      let near_right_rail = cursor.x >= working_area.right - 1 - rail_px
        && (start_cursor.x < working_area.right - 1 - rail_px
          || cursor.x >= working_area.right - 2);
      let near_top = cursor.y <= working_area.top + edge_px;
      let near_bottom = cursor.y >= working_area.bottom - 1 - edge_px;
      let pulled_left = (start_cursor.x - cursor.x) >= pull_away_px;
      let pulled_right = (cursor.x - start_cursor.x) >= pull_away_px;

      if (near_top
        && (near_left_rail
          || (pulled_left && cursor.x < working_area.left + corner_w)))
        || (near_left_rail && cursor.y < working_area.top + corner_h)
      {
        return Some(ScreenZone::TopLeft);
      }
      if (near_top
        && (near_right_rail
          || (pulled_right && cursor.x >= working_area.right - corner_w)))
        || (near_right_rail && cursor.y < working_area.top + corner_h)
      {
        return Some(ScreenZone::TopRight);
      }
      if (near_bottom
        && (near_left_rail
          || (pulled_left && cursor.x < working_area.left + corner_w)))
        || (near_left_rail && cursor.y >= working_area.bottom - corner_h)
      {
        return Some(ScreenZone::BottomLeft);
      }
      if (near_bottom
        && (near_right_rail
          || (pulled_right && cursor.x >= working_area.right - corner_w)))
        || (near_right_rail && cursor.y >= working_area.bottom - corner_h)
      {
        return Some(ScreenZone::BottomRight);
      }
      if near_top_rail && cursor.x >= mid_left && cursor.x <= mid_right {
        return Some(ScreenZone::TopCenter);
      }
      if near_left_rail {
        return Some(ScreenZone::Left);
      }
      if near_right_rail {
        return Some(ScreenZone::Right);
      }
      None
    }
  }
}

/// Resolves the drop target for `dragged_window` at `cursor` under the
/// active drag's origin rules.
///
/// - `is_from_floating == true`: only screen edges, corners, and
///   top-center maximize can trigger (never interior tiles).
/// - `is_from_floating == false`: screen edges/corners/top-center trigger;
///   hovering over an interior `TilingWindow` swaps (if `origin_void_id`
///   exists on the target workspace) or splits; empty space / `VoidSlot`
///   returns `None` (leaving the window floating on drop).
pub fn resolve_drop_target(
  dragged_window: &WindowContainer,
  cursor: &Point,
  active_drag: &ActiveDrag,
  state: &WmState,
) -> anyhow::Result<Option<DropTarget>> {
  let monitor = state
    .monitor_at_point(cursor)
    .or_else(|| dragged_window.monitor())
    .context("No monitor found for snap target.")?;

  let workspace = monitor
    .displayed_workspace()
    .context("No displayed workspace on monitor.")?;

  let props = monitor.native_properties();
  if let Some(zone) =
    detect_screen_zone(cursor, &props.working_area, Some(&props.bounds))
  {
    return Ok(Some(match zone {
      ScreenZone::TopCenter => DropTarget::Maximize {
        workspace_id: workspace.id(),
      },
      _ => DropTarget::SnapZone {
        workspace_id: workspace.id(),
        zone,
      },
    }));
  }

  if active_drag.is_from_floating {
    return Ok(None);
  }

  let target_tile = workspace
    .tiling_windows()
    .filter(|tile| tile.id() != dragged_window.id())
    .find(|tile| {
      tile
        .to_rect()
        .is_ok_and(|rect| rect.contains_point(cursor))
    });

  let Some(target_tile) = target_tile else {
    return Ok(None);
  };

  let has_origin_void_in_ws = active_drag
    .origin_void_id
    .and_then(|id| super::void_slot_by_id(&workspace, id))
    .is_some();

  if has_origin_void_in_ws {
    return Ok(Some(DropTarget::SwapTile {
      workspace_id: workspace.id(),
      target_window_id: target_tile.id(),
    }));
  }

  let tile_rect = target_tile.to_rect()?;
  let side = split_side_for_cursor(cursor, &tile_rect);

  Ok(Some(DropTarget::SplitTile {
    workspace_id: workspace.id(),
    target_window_id: target_tile.id(),
    side,
  }))
}

fn clamp_rect_edges(rect: &Rect, bounds: &Rect) -> Rect {
  let left = rect.left.max(bounds.left).min(bounds.right);
  let top = rect.top.max(bounds.top).min(bounds.bottom);
  let right = rect.right.min(bounds.right).max(left);
  let bottom = rect.bottom.min(bounds.bottom).max(top);

  Rect::from_ltrb(left, top, right, bottom)
}

/// Computes the exact preview rect in screen coordinates for `target`.
pub fn compute_drop_preview_rect(
  target: &DropTarget,
  dragged_window: &WindowContainer,
  active_drag: &ActiveDrag,
  state: &WmState,
) -> anyhow::Result<Option<Rect>> {
  compute_drop_preview_rect_at(
    target,
    dragged_window,
    active_drag,
    None,
    state,
  )
}

/// Computes the exact preview rect in screen coordinates for `target` at
/// an optional cursor position (used to disambiguate multiple side voids).
pub fn compute_drop_preview_rect_at(
  target: &DropTarget,
  dragged_window: &WindowContainer,
  active_drag: &ActiveDrag,
  cursor: Option<&Point>,
  state: &WmState,
) -> anyhow::Result<Option<Rect>> {
  let border_delta = if state.dispatcher.is_openglass_active() {
    dragged_window.total_border_delta()?
  } else {
    dragged_window.border_delta()
  };

  match target {
    DropTarget::Maximize { workspace_id } => {
      let Some(workspace) = state.workspace_by_id(*workspace_id) else {
        return Ok(None);
      };
      let monitor = workspace.monitor().context("No monitor.")?;
      let working_area = monitor.native_properties().working_area;
      let can_maximize =
        window_supports_native_maximize(dragged_window, state);
      let rect = if can_maximize {
        working_area
      } else {
        let max_rect =
          workspace.max_workspace_rect()?.apply_delta(&border_delta, None);
        clamp_rect_edges(&max_rect, &working_area)
      };
      Ok(Some(rect))
    }
    DropTarget::SwapTile {
      workspace_id,
      target_window_id,
    } => {
      let Some(workspace) = state.workspace_by_id(*workspace_id) else {
        return Ok(None);
      };
      let Some(container) = state.container_by_id(*target_window_id) else {
        return Ok(None);
      };
      let monitor = workspace.monitor().context("No monitor.")?;
      let working_area = monitor.native_properties().working_area;
      Ok(Some(clamp_rect_edges(
        &container.to_rect()?.apply_delta(&border_delta, None),
        &working_area,
      )))
    }
    DropTarget::SplitTile {
      workspace_id,
      target_window_id,
      side,
    } => {
      let Some(workspace) = state.workspace_by_id(*workspace_id) else {
        return Ok(None);
      };
      let Some(container) = state.container_by_id(*target_window_id) else {
        return Ok(None);
      };
      let monitor = workspace.monitor().context("No monitor.")?;
      let working_area = monitor.native_properties().working_area;
      let (h_gap, v_gap) = workspace.workspace_inner_gaps()?;
      let rect = container.to_rect()?;
      let (dir, is_first, gap) = match side {
        SplitSide::Left => (TilingDirection::Horizontal, true, h_gap),
        SplitSide::Right => (TilingDirection::Horizontal, false, h_gap),
        SplitSide::Top => (TilingDirection::Vertical, true, v_gap),
        SplitSide::Bottom => (TilingDirection::Vertical, false, v_gap),
      };
      let split = split_half_rect(&rect, &dir, is_first, gap)
        .apply_delta(&border_delta, None);
      Ok(Some(clamp_rect_edges(&split, &working_area)))
    }
    DropTarget::SnapZone { workspace_id, zone } => {
      let Some(workspace) = state.workspace_by_id(*workspace_id) else {
        return Ok(None);
      };
      let monitor = workspace.monitor().context("No monitor.")?;
      let working_area = monitor.native_properties().working_area;
      let tiling_id =
        dragged_window.as_tiling_window().map(CommonGetters::id);
      let exclude_id = active_drag.origin_void_id.or(tiling_id);
      let preview = compute_snap_zone_preview_rect(
        &workspace,
        *zone,
        exclude_id,
        cursor,
      )?
      .apply_delta(&border_delta, None);
      Ok(Some(clamp_rect_edges(&preview, &working_area)))
    }
  }
}

/// Updates or hides the Aero glass snap overlay during a move drag.
pub fn update_drag_snap_preview(
  window: &WindowContainer,
  cursor_override: Option<&Point>,
  state: &WmState,
) -> anyhow::Result<()> {
  let Some(active_drag) = window.active_drag() else {
    state.dispatcher.hide_snap_overlay();
    return Ok(());
  };

  if active_drag.operation != Some(ActiveDragOperation::Move) {
    state.dispatcher.hide_snap_overlay();
    return Ok(());
  }

  let cursor = match cursor_override {
    Some(pos) => pos.clone(),
    None => state.dispatcher.cursor_position()?,
  };

  let target = resolve_drop_target(window, &cursor, &active_drag, state)?;
  let Some(target) = target else {
    state.dispatcher.hide_snap_overlay();
    return Ok(());
  };

  let Some(rect) = compute_drop_preview_rect_at(
    &target,
    window,
    &active_drag,
    Some(&cursor),
    state,
  )?
  else {
    state.dispatcher.hide_snap_overlay();
    return Ok(());
  };

  state
    .dispatcher
    .show_snap_overlay(&rect, Some(window.native().id()));

  Ok(())
}

/// Finalizes a move drag on button/movesize release: snaps to edge/corner,
/// maximizes, swaps/splits an interior tile, or leaves floating over free
/// space.
pub fn finalize_move_drag(
  moved_window: &NonTilingWindow,
  frame_position: Rect,
  active_drag: &ActiveDrag,
  cursor_override: Option<&Point>,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<WindowContainer> {
  state.dispatcher.hide_snap_overlay();

  let window_container: WindowContainer = moved_window.clone().into();
  let cursor = match cursor_override {
    Some(pos) => pos.clone(),
    None => state.dispatcher.cursor_position()?,
  };

  let drop_target = if active_drag.operation
    == Some(ActiveDragOperation::Move)
  {
    resolve_drop_target(&window_container, &cursor, active_drag, state)?
  } else {
    None
  };

  match drop_target {
    Some(DropTarget::Maximize { workspace_id }) => commit_maximize_drop(
      moved_window,
      workspace_id,
      active_drag,
      state,
      config,
    ),
    Some(DropTarget::SwapTile {
      workspace_id,
      target_window_id,
    }) => commit_swap_tile(
      moved_window,
      workspace_id,
      target_window_id,
      active_drag,
      state,
      config,
    ),
    Some(DropTarget::SplitTile {
      workspace_id,
      target_window_id,
      side,
    }) => commit_split_tile(
      moved_window,
      workspace_id,
      target_window_id,
      side,
      active_drag,
      state,
      config,
    ),
    Some(DropTarget::SnapZone { workspace_id, zone }) => commit_snap_zone(
      moved_window,
      workspace_id,
      zone,
      active_drag,
      Some(&cursor),
      state,
      config,
    ),
    None => {
      // Dropped in free space: stay floating and keep voids in place.
      if let Some(workspace) = moved_window.workspace() {
        if active_drag
          .origin_void_id
          .and_then(|id| super::void_slot_by_id(&workspace, id))
          .is_some()
        {
          normalize_workspace_voids(&workspace)?;
        }
      }

      let nearest_monitor = state
        .nearest_monitor(&moved_window.native())
        .context("Failed to get workspace of nearest monitor.")?;

      update_floating_window_position(
        moved_window,
        frame_position,
        &nearest_monitor,
        state,
      )?;
      moved_window.set_active_drag(None);
      Ok(moved_window.clone().into())
    }
  }
}

fn commit_maximize_drop(
  moved_window: &NonTilingWindow,
  target_workspace_id: Uuid,
  active_drag: &ActiveDrag,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<WindowContainer> {
  let old_workspace = moved_window.workspace();
  let target_workspace = state
    .workspace_by_id(target_workspace_id)
    .context("Target workspace not found.")?;

  let moved_across_ws = old_workspace
    .as_ref()
    .is_some_and(|ws| ws.id() != target_workspace.id());

  if moved_across_ws {
    if let (Some(old_mon), Some(new_mon)) = (
      old_workspace.as_ref().and_then(CommonGetters::monitor),
      target_workspace.monitor(),
    ) {
      if old_mon.has_dpi_difference(&new_mon.clone().into())? {
        moved_window.set_has_pending_dpi_adjustment(true);
      }
      if old_mon.id() != new_mon.id() {
        moved_window.set_floating_placement(
          moved_window
            .floating_placement()
            .translate_to_center(&target_workspace.to_rect()?),
        );
      }
    }

    moved_window.set_insertion_target(None);
    move_container_within_tree_or_attach(moved_window, &target_workspace)?;
  }

  let fullscreen_state = if let WindowState::Fullscreen(fullscreen) =
    moved_window.state()
  {
    fullscreen
  } else {
    config
      .value
      .window_behavior
      .state_defaults
      .fullscreen
      .clone()
  };

  // Clear `active_drag` before transitioning state so `prev_state`
  // records the pre-maximize state (e.g. `Floating`) for later restore.
  moved_window.set_active_drag(None);

  let maximized = update_window_state(
    moved_window.clone().into(),
    WindowState::Fullscreen(FullscreenStateConfig {
      maximized: true,
      ..fullscreen_state
    }),
    state,
    config,
  )?;

  if let (Some(origin_void_id), Some(old_ws)) =
    (active_drag.origin_void_id, old_workspace)
  {
    let preferred_front = if old_ws.id() == target_workspace.id() {
      Some(maximized.id())
    } else {
      None
    };
    collapse_origin_void(
      origin_void_id,
      &old_ws,
      preferred_front,
      state,
      config,
    )?;
  }

  set_focused_descendant(&maximized.clone().into(), None);
  state
    .pending_sync
    .queue_container_to_redraw(maximized.clone())
    .queue_workspace_to_reorder(target_workspace);

  Ok(maximized)
}

fn commit_swap_tile(
  moved_window: &NonTilingWindow,
  workspace_id: Uuid,
  target_window_id: Uuid,
  active_drag: &ActiveDrag,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<WindowContainer> {
  let workspace = state
    .workspace_by_id(workspace_id)
    .context("Target workspace not found.")?;

  let Some(target_window) = state
    .container_by_id(target_window_id)
    .and_then(|c| c.as_tiling_window().cloned())
  else {
    return Ok(moved_window.clone().into());
  };

  let Some(origin_void) = active_drag
    .origin_void_id
    .and_then(|id| super::void_slot_by_id(&workspace, id))
  else {
    return Ok(moved_window.clone().into());
  };

  let tiling_window = moved_window.to_tiling(config.value.gaps.clone());
  detach_container(moved_window.clone().into())?;
  replace_tiling_slot(&origin_void.into(), &tiling_window.clone().into())?;
  swap_tiling_containers(
    &tiling_window.clone().into(),
    &target_window.into(),
  )?;
  normalize_workspace_voids(&workspace)?;

  tiling_window.set_active_drag(None);
  set_focused_descendant(&tiling_window.clone().into(), None);

  state
    .pending_sync
    .queue_containers_to_redraw(workspace.tiling_children())
    .queue_workspace_to_reorder(workspace);

  Ok(tiling_window.into())
}

fn commit_split_tile(
  moved_window: &NonTilingWindow,
  workspace_id: Uuid,
  target_window_id: Uuid,
  side: SplitSide,
  active_drag: &ActiveDrag,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<WindowContainer> {
  let old_workspace = moved_window.workspace();
  let workspace = state
    .workspace_by_id(workspace_id)
    .context("Target workspace not found.")?;

  let Some(target_window) = state
    .container_by_id(target_window_id)
    .and_then(|c| c.as_tiling_window().cloned())
  else {
    return Ok(moved_window.clone().into());
  };

  if let (Some(origin_void_id), Some(old_ws)) =
    (active_drag.origin_void_id, &old_workspace)
  {
    if old_ws.id() != workspace.id() {
      collapse_origin_void(origin_void_id, old_ws, None, state, config)?;
    }
  }

  let (split_dir, insert_first) = match side {
    SplitSide::Left => (TilingDirection::Horizontal, true),
    SplitSide::Right => (TilingDirection::Horizontal, false),
    SplitSide::Top => (TilingDirection::Vertical, true),
    SplitSide::Bottom => (TilingDirection::Vertical, false),
  };

  let tiling_window = moved_window.to_tiling(config.value.gaps.clone());
  detach_container(moved_window.clone().into())?;

  let target_parent = target_window.parent().context("No parent.")?;
  let split_container =
    SplitContainer::new(split_dir, config.value.gaps.clone());
  wrap_in_split_container(
    &split_container,
    &target_parent,
    &[target_window.into()],
  )?;

  let insert_idx = usize::from(!insert_first);
  attach_container(
    &tiling_window.clone().into(),
    &split_container.into(),
    Some(insert_idx),
  )?;

  normalize_workspace_voids(&workspace)?;
  tiling_window.set_active_drag(None);
  set_focused_descendant(&tiling_window.clone().into(), None);

  state
    .pending_sync
    .queue_containers_to_redraw(workspace.tiling_children())
    .queue_workspace_to_reorder(workspace);

  Ok(tiling_window.into())
}

#[allow(clippy::too_many_lines)]
fn commit_snap_zone(
  moved_window: &NonTilingWindow,
  workspace_id: Uuid,
  zone: ScreenZone,
  active_drag: &ActiveDrag,
  cursor: Option<&Point>,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<WindowContainer> {
  let old_workspace = moved_window.workspace();
  let workspace = state
    .workspace_by_id(workspace_id)
    .context("Target workspace not found.")?;

  let origin_void_in_target = active_drag
    .origin_void_id
    .filter(|id| super::void_slot_by_id(&workspace, *id).is_some());

  let has_other_real_windows = workspace
    .tiling_windows()
    .any(|win| Some(win.id()) != origin_void_in_target);

  if !has_other_real_windows {
    let tiling_window = moved_window.to_tiling(config.value.gaps.clone());
    detach_container(moved_window.clone().into())?;
    if let (Some(origin_void_id), Some(old_ws)) =
      (active_drag.origin_void_id, &old_workspace)
    {
      if old_ws.id() != workspace.id() {
        collapse_origin_void(origin_void_id, old_ws, None, state, config)?;
      }
    }
    clear_workspace_voids(&workspace)?;
    insert_into_empty_workspace(&tiling_window, &workspace, zone, config)?;
    return finish_snap_zone_commit(tiling_window, &workspace, state);
  }

  let (h_gap, v_gap) = workspace.workspace_inner_gaps()?;
  let ws_rect = workspace.multi_window_workspace_rect()?;

  let void_match = find_matching_void(
    &workspace,
    zone,
    origin_void_in_target,
    cursor,
    &ws_rect,
    h_gap,
    v_gap,
  )?;

  let tiling_window = moved_window.to_tiling(config.value.gaps.clone());
  detach_container(moved_window.clone().into())?;

  if let Some(void_match) = void_match {
    apply_void_match(&tiling_window, void_match, config)?;
    if let (Some(origin_void_id), Some(old_ws)) =
      (active_drag.origin_void_id, &old_workspace)
    {
      cleanup_origin_void_after_void_match(
        origin_void_id,
        old_ws,
        &workspace,
        state,
        config,
      )?;
    }
    return finish_snap_zone_commit(tiling_window, &workspace, state);
  }

  // Collapse any active drag's origin void cleanly so we don't leave a
  // stale hole from the dragged window itself before edge/corner insert.
  if let (Some(origin_void_id), Some(old_ws)) =
    (active_drag.origin_void_id, &old_workspace)
  {
    if let Some(origin_void) =
      super::void_slot_by_id(old_ws, origin_void_id)
    {
      super::detach_tiling_preferring_non_void_siblings(
        &origin_void.into(),
      )?;
      normalize_workspace_voids(old_ws)?;
      if old_ws.id() != workspace.id() {
        state
          .pending_sync
          .queue_containers_to_redraw(old_ws.tiling_children());
      }
    }
  }

  let existing_children: Vec<TilingContainer> =
    workspace.tiling_children().collect();

  if existing_children.is_empty() {
    insert_into_empty_workspace(&tiling_window, &workspace, zone, config)?;
  } else if existing_children.len() == 1
    && existing_children[0].is_tiling_window()
  {
    insert_beside_sole_window(
      &tiling_window,
      &workspace,
      &existing_children[0],
      zone,
      config,
    )?;
  } else {
    insert_into_populated_workspace(
      &tiling_window,
      &workspace,
      &existing_children,
      zone,
      config,
    )?;
  }

  finish_snap_zone_commit(tiling_window, &workspace, state)
}

fn apply_void_match(
  tiling_window: &TilingWindow,
  void_match: VoidMatch,
  config: &UserConfig,
) -> anyhow::Result<()> {
  match void_match {
    VoidMatch::Exact(slot) => {
      replace_tiling_slot(&slot.into(), &tiling_window.clone().into())?;
    }
    VoidMatch::SplitHalf {
      slot,
      split_dir,
      insert_first,
    } => {
      let slot_parent =
        slot.parent().context("Void slot has no parent.")?;
      let split =
        SplitContainer::new(split_dir, config.value.gaps.clone());
      let slot_tiling: TilingContainer = slot.into();
      wrap_in_split_container(&split, &slot_parent, &[slot_tiling])?;
      let insert_idx = usize::from(!insert_first);
      attach_container(
        &tiling_window.clone().into(),
        &split.into(),
        Some(insert_idx),
      )?;
    }
  }

  Ok(())
}

fn cleanup_origin_void_after_void_match(
  origin_void_id: Uuid,
  old_ws: &Workspace,
  target_ws: &Workspace,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let slot = super::void_slot_by_id(old_ws, origin_void_id);
  let Some(origin_void) = slot else {
    return Ok(());
  };

  let prev_is_void = origin_void
    .prev_siblings()
    .find_map(|s| s.as_tiling_container().ok())
    .is_some_and(|c| c.is_void());
  let next_is_void = origin_void
    .next_siblings()
    .find_map(|s| s.as_tiling_container().ok())
    .is_some_and(|c| c.is_void());

  if prev_is_void || next_is_void {
    normalize_workspace_voids(old_ws)?;
    if old_ws.id() != target_ws.id() {
      state
        .pending_sync
        .queue_containers_to_redraw(old_ws.tiling_children());
    }
  } else {
    collapse_origin_void(origin_void_id, old_ws, None, state, config)?;
  }

  Ok(())
}

fn finish_snap_zone_commit(
  tiling_window: TilingWindow,
  workspace: &Workspace,
  state: &mut WmState,
) -> anyhow::Result<WindowContainer> {
  normalize_workspace_voids(workspace)?;
  absorb_unfillable_void_slivers(workspace)?;
  tiling_window.set_active_drag(None);
  set_focused_descendant(&tiling_window.clone().into(), None);

  state
    .pending_sync
    .queue_containers_to_redraw(workspace.tiling_children())
    .queue_workspace_to_reorder(workspace.clone());

  Ok(tiling_window.into())
}

fn insert_into_empty_workspace(
  tiling_window: &TilingWindow,
  workspace: &Workspace,
  zone: ScreenZone,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let gaps = config.value.gaps.clone();
  let ws_container = workspace.clone().into();
  let win_container: crate::models::Container =
    tiling_window.clone().into();

  match zone {
    ScreenZone::Left
    | ScreenZone::Right
    | ScreenZone::Top
    | ScreenZone::Bottom => {
      let (dir, is_first) = edge_direction_and_side(zone);
      workspace.set_tiling_direction(dir);
      let void_slot = VoidSlot::new(gaps);
      if is_first {
        attach_container(&win_container, &ws_container, Some(0))?;
        attach_container(&void_slot.into(), &ws_container, Some(1))?;
      } else {
        attach_container(&void_slot.into(), &ws_container, Some(0))?;
        attach_container(&win_container, &ws_container, Some(1))?;
      }
    }
    ScreenZone::TopLeft
    | ScreenZone::TopRight
    | ScreenZone::BottomLeft
    | ScreenZone::BottomRight => {
      let (is_left, is_top) = corner_sides(zone);
      workspace.set_tiling_direction(TilingDirection::Horizontal);
      let half_split =
        SplitContainer::new(TilingDirection::Vertical, gaps.clone());
      let inner_void = VoidSlot::new(gaps.clone());
      let outer_void = VoidSlot::new(gaps);

      if is_left {
        attach_container(
          &half_split.clone().into(),
          &ws_container,
          Some(0),
        )?;
        attach_container(&outer_void.into(), &ws_container, Some(1))?;
      } else {
        attach_container(&outer_void.into(), &ws_container, Some(0))?;
        attach_container(
          &half_split.clone().into(),
          &ws_container,
          Some(1),
        )?;
      }

      let split_container = half_split.into();
      if is_top {
        attach_container(&win_container, &split_container, Some(0))?;
        attach_container(&inner_void.into(), &split_container, Some(1))?;
      } else {
        attach_container(&inner_void.into(), &split_container, Some(0))?;
        attach_container(&win_container, &split_container, Some(1))?;
      }
    }
    ScreenZone::TopCenter => {}
  }

  Ok(())
}

fn insert_beside_sole_window(
  tiling_window: &TilingWindow,
  workspace: &Workspace,
  existing: &TilingContainer,
  zone: ScreenZone,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let ws_container = workspace.clone().into();
  let gaps = config.value.gaps.clone();
  let win_container: crate::models::Container =
    tiling_window.clone().into();

  match zone {
    ScreenZone::Left
    | ScreenZone::Right
    | ScreenZone::Top
    | ScreenZone::Bottom => {
      let (dir, is_first) = edge_direction_and_side(zone);
      workspace.set_tiling_direction(dir);
      let insert_idx = usize::from(!is_first);
      attach_container(&win_container, &ws_container, Some(insert_idx))?;
    }
    ScreenZone::TopLeft
    | ScreenZone::TopRight
    | ScreenZone::BottomLeft
    | ScreenZone::BottomRight => {
      let (is_left, is_top) = corner_sides(zone);
      workspace.set_tiling_direction(TilingDirection::Horizontal);
      let half_split =
        SplitContainer::new(TilingDirection::Vertical, gaps.clone());
      let inner_void = VoidSlot::new(gaps);
      let existing_c: crate::models::Container = existing.clone().into();

      // Detach existing temporarily to place beside the new split.
      detach_container(existing_c.clone())?;
      if is_left {
        attach_container(
          &half_split.clone().into(),
          &ws_container,
          Some(0),
        )?;
        attach_container(&existing_c, &ws_container, Some(1))?;
      } else {
        attach_container(&existing_c, &ws_container, Some(0))?;
        attach_container(
          &half_split.clone().into(),
          &ws_container,
          Some(1),
        )?;
      }

      let split_container = half_split.into();
      if is_top {
        attach_container(&win_container, &split_container, Some(0))?;
        attach_container(&inner_void.into(), &split_container, Some(1))?;
      } else {
        attach_container(&inner_void.into(), &split_container, Some(0))?;
        attach_container(&win_container, &split_container, Some(1))?;
      }
    }
    ScreenZone::TopCenter => {}
  }

  Ok(())
}

struct VoidEdgeAllocation {
  absorbed_void_ids: Vec<Uuid>,
  shrunk_voids: Vec<(Uuid, f32)>,
  preserved_voids: Vec<(Uuid, f32)>,
  non_void_scale: f32,
  new_win_size: f32,
}

#[allow(
  clippy::cast_possible_truncation,
  clippy::cast_possible_wrap,
  clippy::cast_precision_loss
)]
fn allocate_void_surplus(
  children: &[(Uuid, bool, f32)],
  direct_voids: &[(Uuid, f32)],
  non_void_count: usize,
  is_first: bool,
  axis_px: i32,
  gap: i32,
) -> VoidEdgeAllocation {
  let old_avail =
    (axis_px - gap * (children.len() as i32 - 1)).max(1) as f32;
  let gap_f = gap as f32;
  let min_void_f = MIN_VOID_SPAN_PX as f32;
  let ideal_avail = (axis_px - gap * non_void_count as i32).max(1) as f32;
  let ideal_win_px = ideal_avail / (non_void_count + 1) as f32;
  let mut rem_need = ideal_win_px + gap_f;

  let mut absorbed_void_ids = Vec::new();
  let mut shrunk_map: Vec<(Uuid, f32)> = Vec::new();
  let ordered: Vec<(Uuid, f32)> = if is_first {
    direct_voids.iter().copied().rev().collect()
  } else {
    direct_voids.to_vec()
  };

  for (vid, vsize) in ordered {
    if rem_need <= 0.5 {
      break;
    }
    let v_px = vsize * old_avail;
    if v_px + gap_f <= rem_need + min_void_f - 1.0 {
      absorbed_void_ids.push(vid);
      rem_need -= v_px + gap_f;
    } else {
      let new_v_px = v_px - rem_need;
      if new_v_px < min_void_f {
        absorbed_void_ids.push(vid);
        rem_need -= v_px + gap_f;
      } else {
        shrunk_map.push((vid, new_v_px));
        rem_need = 0.0;
      }
    }
  }

  let kept_count = children.len() - absorbed_void_ids.len();
  let new_avail = (axis_px - gap * kept_count as i32).max(1) as f32;
  let non_void_total_px: f32 = children
    .iter()
    .filter(|(_, is_void, _)| !*is_void)
    .map(|(_, _, size)| *size * old_avail)
    .sum();

  let mut shrunk_voids = Vec::new();
  let mut preserved_voids = Vec::new();
  let mut other_voids_px = 0.0_f32;
  for &(vid, vsize) in direct_voids {
    if absorbed_void_ids.contains(&vid) {
      continue;
    }
    if let Some(&(_, px)) = shrunk_map.iter().find(|&&(id, _)| id == vid) {
      other_voids_px += px;
      shrunk_voids.push((vid, px / new_avail));
    } else {
      let px = vsize * old_avail;
      other_voids_px += px;
      preserved_voids.push((vid, px / new_avail));
    }
  }

  let raw_win_px = new_avail - non_void_total_px - other_voids_px;
  let (non_void_scale, new_win_size) =
    if rem_need < 0.0 || raw_win_px < ideal_win_px * 0.5 {
      let usable = (new_avail - other_voids_px).max(1.0);
      let scale = usable / (non_void_total_px + ideal_win_px).max(1.0);
      ((old_avail * scale) / new_avail, (ideal_win_px * scale) / new_avail)
    } else {
      (old_avail / new_avail, raw_win_px / new_avail)
    };

  VoidEdgeAllocation {
    absorbed_void_ids,
    shrunk_voids,
    preserved_voids,
    non_void_scale,
    new_win_size,
  }
}

#[allow(
  clippy::cast_possible_truncation,
  clippy::cast_possible_wrap,
  clippy::cast_precision_loss
)]
fn compute_edge_void_allocation(
  children: &[(Uuid, bool, f32)],
  is_first: bool,
  axis_px: i32,
  gap: i32,
) -> VoidEdgeAllocation {
  let direct_voids: Vec<(Uuid, f32)> = children
    .iter()
    .filter(|(_, is_void, _)| *is_void)
    .map(|(id, _, size)| (*id, *size))
    .collect();
  let non_void_count = children.iter().filter(|(_, v, _)| !*v).count();
  let void_sum: f32 = direct_voids.iter().map(|(_, s)| *s).sum();
  let target_frac = 1.0_f32 / (non_void_count + 1) as f32;

  if void_sum >= target_frac - 0.02 {
    return allocate_void_surplus(
      children,
      &direct_voids,
      non_void_count,
      is_first,
      axis_px,
      gap,
    );
  }

  let old_n = children.len() as i32;
  let old_avail = (axis_px - gap * (old_n - 1)).max(1) as f32;
  let new_avail = (axis_px - gap * old_n).max(1) as f32;

  let mut voids_px = 0.0_f32;
  let mut preserved_voids = Vec::new();
  for &(vid, vsize) in &direct_voids {
    let px = vsize * old_avail;
    voids_px += px;
    preserved_voids.push((vid, px / new_avail));
  }

  let non_void_total_px: f32 = children
    .iter()
    .filter(|(_, is_void, _)| !*is_void)
    .map(|(_, _, size)| *size * old_avail)
    .sum();
  let usable = (new_avail - voids_px).max(1.0);
  let win_px = usable / (non_void_count + 1) as f32;
  let scale = (usable - win_px) / non_void_total_px.max(1.0);

  VoidEdgeAllocation {
    absorbed_void_ids: Vec::new(),
    shrunk_voids: Vec::new(),
    preserved_voids,
    non_void_scale: (old_avail * scale) / new_avail,
    new_win_size: win_px / new_avail,
  }
}

fn insert_along_workspace_direction(
  tiling_window: &TilingWindow,
  workspace: &Workspace,
  existing_children: &[TilingContainer],
  edge_dir: &TilingDirection,
  is_first: bool,
) -> anyhow::Result<()> {
  let ws_container: Container = workspace.clone().into();
  let child_meta: Vec<(Uuid, bool, f32)> = existing_children
    .iter()
    .map(|c| (c.id(), c.is_void(), c.tiling_size()))
    .collect();
  let has_voids = child_meta.iter().any(|(_, is_void, _)| *is_void);
  let has_non_voids = child_meta.iter().any(|(_, is_void, _)| !*is_void);

  if !has_voids || !has_non_voids {
    let insert_idx = if is_first { 0 } else { workspace.child_count() };
    attach_container(
      &tiling_window.clone().into(),
      &ws_container,
      Some(insert_idx),
    )?;
    return Ok(());
  }

  let ws_rect = workspace.multi_window_workspace_rect()?;
  let (h_gap, v_gap) = workspace.workspace_inner_gaps()?;
  let gap = gap_for_dir(edge_dir, h_gap, v_gap);
  let axis_px = match edge_dir {
    TilingDirection::Horizontal => ws_rect.width(),
    TilingDirection::Vertical => ws_rect.height(),
  };

  let alloc =
    compute_edge_void_allocation(&child_meta, is_first, axis_px, gap);

  for vid in &alloc.absorbed_void_ids {
    if let Some(void_c) =
      existing_children.iter().find(|c| c.id() == *vid)
    {
      workspace.borrow_children_mut().retain(|c| c.id() != *vid);
      workspace
        .borrow_child_focus_order_mut()
        .retain(|id| *id != *vid);
      *void_c.borrow_parent_mut() = None;
    }
  }

  let insert_idx = if is_first { 0 } else { workspace.child_count() };
  attach_container(
    &tiling_window.clone().into(),
    &ws_container,
    Some(insert_idx),
  )?;

  for child in existing_children {
    let cid = child.id();
    if alloc.absorbed_void_ids.contains(&cid) {
      continue;
    }
    if let Some(&(_, sz)) = alloc
      .shrunk_voids
      .iter()
      .chain(alloc.preserved_voids.iter())
      .find(|&&(id, _)| id == cid)
    {
      child.set_tiling_size(sz);
    } else if !child.is_void() {
      child.set_tiling_size(child.tiling_size() * alloc.non_void_scale);
    }
  }
  tiling_window.set_tiling_size(alloc.new_win_size);

  Ok(())
}

fn insert_into_populated_workspace(
  tiling_window: &TilingWindow,
  workspace: &Workspace,
  existing_children: &[TilingContainer],
  zone: ScreenZone,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let ws_container = workspace.clone().into();
  let gaps = config.value.gaps.clone();

  match zone {
    ScreenZone::Left
    | ScreenZone::Right
    | ScreenZone::Top
    | ScreenZone::Bottom => {
      let (edge_dir, is_first) = edge_direction_and_side(zone);
      if workspace.tiling_direction() == edge_dir {
        insert_along_workspace_direction(
          tiling_window,
          workspace,
          existing_children,
          &edge_dir,
          is_first,
        )?;
      } else {
        let old_dir = workspace.tiling_direction();
        let wrapper = SplitContainer::new(old_dir, gaps);
        wrap_in_split_container(
          &wrapper,
          &ws_container,
          existing_children,
        )?;
        workspace.set_tiling_direction(edge_dir);
        let insert_idx = usize::from(!is_first);
        attach_container(
          &tiling_window.clone().into(),
          &ws_container,
          Some(insert_idx),
        )?;
      }
    }
    ScreenZone::TopLeft
    | ScreenZone::TopRight
    | ScreenZone::BottomLeft
    | ScreenZone::BottomRight => {
      let ws_rect = workspace.multi_window_workspace_rect()?;
      let leaf = find_corner_leaf(workspace, zone, &ws_rect)?;
      let leaf_rect = leaf.to_rect()?;
      let split_dir = corner_split_direction(&leaf_rect);
      let insert_first = corner_leaf_insert_first(zone, &split_dir);

      let leaf_parent =
        leaf.parent().context("Corner leaf has no parent.")?;
      let split = SplitContainer::new(split_dir, gaps);
      wrap_in_split_container(&split, &leaf_parent, &[leaf])?;
      let insert_idx = usize::from(!insert_first);
      attach_container(
        &tiling_window.clone().into(),
        &split.into(),
        Some(insert_idx),
      )?;
    }
    ScreenZone::TopCenter => {}
  }

  Ok(())
}

#[derive(Clone)]
enum SimKind {
  Window,
  Void,
  Split {
    direction: TilingDirection,
    children: Vec<SimNode>,
  },
}

#[derive(Clone)]
struct SimNode {
  id: Uuid,
  size: f32,
  kind: SimKind,
}

impl SimNode {
  const fn is_void(&self) -> bool {
    matches!(self.kind, SimKind::Void)
  }

  const fn is_window(&self) -> bool {
    matches!(self.kind, SimKind::Window)
  }

  const fn is_split(&self) -> bool {
    matches!(self.kind, SimKind::Split { .. })
  }

  fn from_tiling(container: &TilingContainer) -> Self {
    let kind = match container {
      TilingContainer::TilingWindow(_) => SimKind::Window,
      TilingContainer::Void(_) => SimKind::Void,
      TilingContainer::Split(split) => SimKind::Split {
        direction: split.tiling_direction(),
        children: split
          .tiling_children()
          .map(|child| Self::from_tiling(&child))
          .collect(),
      },
    };
    Self {
      id: container.id(),
      size: container.tiling_size(),
      kind,
    }
  }
}

struct SimWorkspace {
  direction: TilingDirection,
  ws_rect: Rect,
  h_gap: i32,
  v_gap: i32,
  children: Vec<SimNode>,
}

impl SimWorkspace {
  fn from_workspace(
    workspace: &Workspace,
    ws_rect: Rect,
    h_gap: i32,
    v_gap: i32,
  ) -> Self {
    Self {
      direction: workspace.tiling_direction(),
      ws_rect,
      h_gap,
      v_gap,
      children: workspace
        .tiling_children()
        .map(|child| SimNode::from_tiling(&child))
        .collect(),
    }
  }

  fn has_real_windows(&self) -> bool {
    sim_has_window(&self.children)
  }

  fn has_voids(&self) -> bool {
    sim_has_void(&self.children)
  }

  fn real_window_count(&self) -> usize {
    sim_count_windows(&self.children)
  }

  fn collect_rects(
    &self,
    nodes: &[SimNode],
    parent_rect: &Rect,
    dir: &TilingDirection,
    out: &mut Vec<(Uuid, Rect, bool, bool)>,
  ) {
    let Ok(sibling_gaps) = i32::try_from(nodes.len().saturating_sub(1))
    else {
      return;
    };
    let gap = gap_for_dir(dir, self.h_gap, self.v_gap);

    #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
    match dir {
      TilingDirection::Horizontal => {
        let avail = (parent_rect.width() - gap * sibling_gaps).max(1);
        let mut x = parent_rect.x();
        for node in nodes {
          let w = (avail as f32 * node.size).round() as i32;
          let rect =
            Rect::from_xy(x, parent_rect.y(), w, parent_rect.height());
          match &node.kind {
            SimKind::Split { direction, children } => {
              out.push((node.id, rect.clone(), false, false));
              self.collect_rects(children, &rect, direction, out);
            }
            SimKind::Window => out.push((node.id, rect, true, false)),
            SimKind::Void => out.push((node.id, rect, false, true)),
          }
          x += w + gap;
        }
      }
      TilingDirection::Vertical => {
        let avail = (parent_rect.height() - gap * sibling_gaps).max(1);
        let mut y = parent_rect.y();
        for node in nodes {
          let h = (node.size * avail as f32) as i32;
          let rect =
            Rect::from_xy(parent_rect.x(), y, parent_rect.width(), h);
          match &node.kind {
            SimKind::Split { direction, children } => {
              out.push((node.id, rect.clone(), false, false));
              self.collect_rects(children, &rect, direction, out);
            }
            SimKind::Window => out.push((node.id, rect, true, false)),
            SimKind::Void => out.push((node.id, rect, false, true)),
          }
          y += h + gap;
        }
      }
    }
  }

  fn all_rects(&self) -> Vec<(Uuid, Rect, bool, bool)> {
    let mut out = Vec::new();
    self.collect_rects(
      &self.children,
      &self.ws_rect,
      &self.direction,
      &mut out,
    );
    out
  }

  fn rect_of(&self, id: Uuid) -> Option<Rect> {
    self
      .all_rects()
      .into_iter()
      .find(|(nid, _, _, _)| *nid == id)
      .map(|(_, r, _, _)| r)
  }

  fn attach_node(
    children: &mut Vec<SimNode>,
    mut node: SimNode,
    idx: usize,
  ) {
    let clamped = idx.min(children.len());
    if children.is_empty() {
      node.size = 1.0;
      children.insert(clamped, node);
      return;
    }

    #[allow(clippy::cast_precision_loss)]
    let target_size = (1.0 / (children.len() + 1) as f32).clamp(
      MIN_TILING_SIZE,
      1.0 - (children.len() as f32 * MIN_TILING_SIZE),
    );
    let available: f32 = children
      .iter()
      .map(|c| c.size - MIN_TILING_SIZE)
      .sum();
    if available > f32::EPSILON {
      for child in children.iter_mut() {
        let factor = (child.size - MIN_TILING_SIZE) / available;
        child.size -= factor * target_size;
      }
    } else {
      #[allow(clippy::cast_precision_loss)]
      let even_delta = target_size / children.len() as f32;
      for child in children.iter_mut() {
        child.size -= even_delta;
      }
    }

    node.size = target_size;
    children.insert(clamped, node);
  }

  fn detach_preferring_non_void_in(
    children: &mut Vec<SimNode>,
    target_id: Uuid,
  ) -> bool {
    if let Some(pos) = children.iter().position(|c| c.id == target_id) {
      let removed = children.remove(pos);
      let freed = removed.size;
      let has_non_void = children.iter().any(|c| !c.is_void());
      let avail: f32 = children
        .iter()
        .filter(|c| !has_non_void || !c.is_void())
        .map(|c| c.size - MIN_TILING_SIZE)
        .sum();
      let rec_count = children
        .iter()
        .filter(|c| !has_non_void || !c.is_void())
        .count();

      if rec_count > 0 {
        if avail > f32::EPSILON {
          for child in children.iter_mut() {
            if !has_non_void || !child.is_void() {
              let factor = (child.size - MIN_TILING_SIZE) / avail;
              child.size += factor * freed;
            }
          }
        } else {
          #[allow(clippy::cast_precision_loss)]
          let even = freed / rec_count as f32;
          for child in children.iter_mut() {
            if !has_non_void || !child.is_void() {
              child.size += even;
            }
          }
        }
      }
      return true;
    }

    for child in children.iter_mut() {
      if let SimKind::Split { children: sub, .. } = &mut child.kind {
        if Self::detach_preferring_non_void_in(sub, target_id) {
          return true;
        }
      }
    }
    false
  }

  fn normalize(&mut self) {
    if !self.has_real_windows() {
      self.children.clear();
      return;
    }

    loop {
      let changed = Self::normalize_level(
        &mut self.direction,
        &mut self.children,
      );
      if !changed {
        break;
      }
    }
  }

  fn normalize_level(
    parent_dir: &mut TilingDirection,
    children: &mut Vec<SimNode>,
  ) -> bool {
    let mut changed = false;

    for child in children.iter_mut() {
      if let SimKind::Split {
        direction,
        children: sub,
      } = &mut child.kind
      {
        if Self::normalize_level(direction, sub) {
          changed = true;
        }
      }
    }

    let mut i = 0;
    while i + 1 < children.len() {
      if children[i].is_void() && children[i + 1].is_void() {
        let extra = children.remove(i + 1).size;
        children[i].size += extra;
        changed = true;
      } else {
        i += 1;
      }
    }

    let before_len = children.len();
    children.retain(|c| match &c.kind {
      SimKind::Split { children: sub, .. } => !sub.is_empty(),
      _ => true,
    });
    if children.len() != before_len {
      changed = true;
    }

    if children.len() == 1 && children[0].is_split() {
      let sole = children.remove(0);
      if let SimKind::Split { direction, children: sub } = sole.kind {
        *parent_dir = direction;
        *children = sub;
        changed = true;
      }
    } else {
      let mut out = Vec::with_capacity(children.len());
      for child in children.drain(..) {
        match child.kind {
          SimKind::Split {
            direction,
            children: sub,
          } if sub.len() == 1 || direction == *parent_dir => {
            for mut grand in sub {
              grand.size *= child.size;
              out.push(grand);
            }
            changed = true;
          }
          other => out.push(SimNode {
            id: child.id,
            size: child.size,
            kind: other,
          }),
        }
      }
      *children = out;
    }

    changed
  }

  fn vacate_origin(&mut self, origin_id: Uuid) -> Option<Uuid> {
    match sim_inspect_origin(&self.children, &self.direction, origin_id)? {
      SimOriginKind::Void => Some(origin_id),
      SimOriginKind::WinWithVertNeighbor => {
        Self::detach_preferring_non_void_in(&mut self.children, origin_id);
        self.normalize();
        if !self.has_voids() && self.real_window_count() == 1 {
          self.children.clear();
        }
        None
      }
      SimOriginKind::WinWithoutVertNeighbor => {
        sim_turn_to_void(&mut self.children, origin_id);
        Some(origin_id)
      }
    }
  }

  fn absorb_void_pair_in(
    nodes: &mut Vec<SimNode>,
    dir: &TilingDirection,
    parent_rect: &Rect,
    sliver_id: Uuid,
    h_gap: i32,
    v_gap: i32,
  ) -> bool {
    if let Some(pos) = nodes.iter().position(|n| n.id == sliver_id) {
      return sim_absorb_void_at(
        nodes,
        dir,
        parent_rect,
        pos,
        h_gap,
        v_gap,
      );
    }

    let gap = gap_for_dir(dir, h_gap, v_gap);
    let Ok(sib_gaps) = i32::try_from(nodes.len().saturating_sub(1)) else {
      return false;
    };
    let mut offset = match dir {
      TilingDirection::Horizontal => parent_rect.x(),
      TilingDirection::Vertical => parent_rect.y(),
    };
    let avail = match dir {
      TilingDirection::Horizontal => {
        (parent_rect.width() - gap * sib_gaps).max(1)
      }
      TilingDirection::Vertical => {
        (parent_rect.height() - gap * sib_gaps).max(1)
      }
    };

    for node in nodes.iter_mut() {
      #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_precision_loss
      )]
      let child_rect = match dir {
        TilingDirection::Horizontal => {
          let w = (avail as f32 * node.size).round() as i32;
          let r = Rect::from_xy(
            offset,
            parent_rect.y(),
            w,
            parent_rect.height(),
          );
          offset += w + gap;
          r
        }
        TilingDirection::Vertical => {
          let h = (node.size * avail as f32) as i32;
          let r = Rect::from_xy(
            parent_rect.x(),
            offset,
            parent_rect.width(),
            h,
          );
          offset += h + gap;
          r
        }
      };

      if let SimKind::Split { direction, children } = &mut node.kind {
        if Self::absorb_void_pair_in(
          children,
          direction,
          &child_rect,
          sliver_id,
          h_gap,
          v_gap,
        ) {
          return true;
        }
      }
    }
    false
  }

  fn absorb_slivers(&mut self) {
    self.normalize();
    for _ in 0..32 {
      let sliver = self.all_rects().into_iter().find_map(
        |(id, rect, _, is_void)| {
          (is_void
            && (rect.width() < MIN_VOID_SPAN_PX
              || rect.height() < MIN_VOID_SPAN_PX))
            .then_some(id)
        },
      );
      let Some(sliver_id) = sliver else {
        break;
      };
      if !Self::absorb_void_pair_in(
        &mut self.children,
        &self.direction,
        &self.ws_rect,
        sliver_id,
        self.h_gap,
        self.v_gap,
      ) {
        break;
      }
      self.normalize();
    }
  }

  fn find_matching_void(
    &self,
    zone: ScreenZone,
    origin_void_id: Option<Uuid>,
    cursor: Option<&Point>,
  ) -> Option<(Uuid, VoidMatchKind)> {
    let mut best: Option<(u8, (Uuid, VoidMatchKind))> = None;
    for (id, rect, _, is_void) in self.all_rects() {
      if !is_void || Some(id) == origin_void_id {
        continue;
      }
      if let Some((rank, kind)) = match_void_slot_rect(
        &rect,
        zone,
        cursor,
        &self.ws_rect,
        self.h_gap,
        self.v_gap,
      ) {
        if best.as_ref().is_none_or(|(br, _)| rank < *br) {
          best = Some((rank, (id, kind)));
        }
      }
    }
    best.map(|(_, matched)| matched)
  }

  fn apply_void_match(
    &mut self,
    new_win_id: Uuid,
    slot_id: Uuid,
    kind: &VoidMatchKind,
  ) {
    sim_apply_void_match(&mut self.children, new_win_id, slot_id, kind);
  }

  fn cleanup_origin_void_after_match(&mut self, origin_id: Uuid) {
    match sim_has_adjacent_void(&self.children, origin_id) {
      Some(true) => self.normalize(),
      Some(false) => {
        Self::detach_preferring_non_void_in(&mut self.children, origin_id);
        self.normalize();
      }
      None => {}
    }
  }

  fn insert_populated(&mut self, new_win_id: Uuid, zone: ScreenZone) {
    match zone {
      ScreenZone::Left
      | ScreenZone::Right
      | ScreenZone::Top
      | ScreenZone::Bottom => {
        self.insert_populated_edge(new_win_id, zone);
      }
      ScreenZone::TopLeft
      | ScreenZone::TopRight
      | ScreenZone::BottomLeft
      | ScreenZone::BottomRight => {
        self.insert_populated_corner(new_win_id, zone);
      }
      ScreenZone::TopCenter => {}
    }
  }

  fn insert_populated_edge(&mut self, new_win_id: Uuid, zone: ScreenZone) {
    let (edge_dir, is_first) = edge_direction_and_side(zone);
    if self.direction != edge_dir {
      let old_children = std::mem::take(&mut self.children);
      let wrapper = SimNode {
        id: Uuid::new_v4(),
        size: 0.5,
        kind: SimKind::Split {
          direction: self.direction.clone(),
          children: old_children,
        },
      };
      let inserted = SimNode {
        id: new_win_id,
        size: 0.5,
        kind: SimKind::Window,
      };
      self.direction = edge_dir;
      self.children = if is_first {
        vec![inserted, wrapper]
      } else {
        vec![wrapper, inserted]
      };
      return;
    }

    let meta: Vec<(Uuid, bool, f32)> = self
      .children
      .iter()
      .map(|c| (c.id, c.is_void(), c.size))
      .collect();
    let has_voids = meta.iter().any(|(_, v, _)| *v);
    let has_non_voids = meta.iter().any(|(_, v, _)| !*v);
    let mut new_node = SimNode {
      id: new_win_id,
      size: 1.0,
      kind: SimKind::Window,
    };
    if !has_voids || !has_non_voids {
      let idx = if is_first { 0 } else { self.children.len() };
      Self::attach_node(&mut self.children, new_node, idx);
      return;
    }

    let gap = gap_for_dir(&edge_dir, self.h_gap, self.v_gap);
    let axis_px = match edge_dir {
      TilingDirection::Horizontal => self.ws_rect.width(),
      TilingDirection::Vertical => self.ws_rect.height(),
    };
    let alloc =
      compute_edge_void_allocation(&meta, is_first, axis_px, gap);
    self
      .children
      .retain(|c| !alloc.absorbed_void_ids.contains(&c.id));
    for child in &mut self.children {
      if let Some(&(_, sz)) = alloc
        .shrunk_voids
        .iter()
        .chain(alloc.preserved_voids.iter())
        .find(|&&(id, _)| id == child.id)
      {
        child.size = sz;
      } else if !child.is_void() {
        child.size *= alloc.non_void_scale;
      }
    }
    let idx = if is_first { 0 } else { self.children.len() };
    new_node.size = alloc.new_win_size;
    self.children.insert(idx, new_node);
  }

  fn insert_populated_corner(
    &mut self,
    new_win_id: Uuid,
    zone: ScreenZone,
  ) {
    let target_pt = corner_target_point(zone, &self.ws_rect);
    let Some((leaf_id, leaf_rect)) = self
      .all_rects()
      .into_iter()
      .filter(|(_, _, is_win, is_void)| *is_win || *is_void)
      .min_by_key(|(_, rect, _, _)| {
        let pt = corner_rect_point(zone, rect);
        (pt.x - target_pt.x).abs() + (pt.y - target_pt.y).abs()
      })
      .map(|(id, rect, _, _)| (id, rect))
    else {
      return;
    };

    let split_dir = corner_split_direction(&leaf_rect);
    let insert_first = corner_leaf_insert_first(zone, &split_dir);
    sim_wrap_leaf(
      &mut self.children,
      leaf_id,
      new_win_id,
      &split_dir,
      insert_first,
    );
  }
}

enum SimOriginKind {
  Void,
  WinWithVertNeighbor,
  WinWithoutVertNeighbor,
}

fn sim_has_window(nodes: &[SimNode]) -> bool {
  nodes.iter().any(|n| match &n.kind {
    SimKind::Window => true,
    SimKind::Void => false,
    SimKind::Split { children, .. } => sim_has_window(children),
  })
}

fn sim_has_void(nodes: &[SimNode]) -> bool {
  nodes.iter().any(|n| match &n.kind {
    SimKind::Window => false,
    SimKind::Void => true,
    SimKind::Split { children, .. } => sim_has_void(children),
  })
}

fn sim_count_windows(nodes: &[SimNode]) -> usize {
  nodes
    .iter()
    .map(|n| match &n.kind {
      SimKind::Window => 1,
      SimKind::Void => 0,
      SimKind::Split { children, .. } => sim_count_windows(children),
    })
    .sum()
}

fn sim_inspect_origin(
  nodes: &[SimNode],
  dir: &TilingDirection,
  origin_id: Uuid,
) -> Option<SimOriginKind> {
  for node in nodes {
    if node.id == origin_id {
      if node.is_void() {
        return Some(SimOriginKind::Void);
      }
      let has_vert = *dir == TilingDirection::Vertical
        && nodes.iter().any(|c| c.id != origin_id && !c.is_void());
      return Some(if has_vert {
        SimOriginKind::WinWithVertNeighbor
      } else {
        SimOriginKind::WinWithoutVertNeighbor
      });
    }
    if let SimKind::Split { direction, children } = &node.kind {
      if let Some(found) =
        sim_inspect_origin(children, direction, origin_id)
      {
        return Some(found);
      }
    }
  }
  None
}

fn sim_turn_to_void(nodes: &mut [SimNode], origin_id: Uuid) -> bool {
  for node in nodes.iter_mut() {
    if node.id == origin_id {
      node.kind = SimKind::Void;
      return true;
    }
    if let SimKind::Split { children, .. } = &mut node.kind {
      if sim_turn_to_void(children, origin_id) {
        return true;
      }
    }
  }
  false
}

fn sim_absorb_void_at(
  nodes: &mut Vec<SimNode>,
  dir: &TilingDirection,
  parent_rect: &Rect,
  pos: usize,
  h_gap: i32,
  v_gap: i32,
) -> bool {
  if nodes.len() < 2 {
    return false;
  }
  let prev = pos.checked_sub(1);
  let next = (pos + 1 < nodes.len()).then_some(pos + 1);
  let Some(ben_idx) = [prev, next]
    .into_iter()
    .flatten()
    .find(|&idx| !nodes[idx].is_void())
    .or(prev.or(next))
  else {
    return false;
  };

  let gap = gap_for_dir(dir, h_gap, v_gap);
  let axis_px = match dir {
    TilingDirection::Horizontal => parent_rect.width(),
    TilingDirection::Vertical => parent_rect.height(),
  };
  let Ok(sib_gaps) = i32::try_from(nodes.len().saturating_sub(1)) else {
    return false;
  };
  #[allow(clippy::cast_precision_loss)]
  let old_avail = (axis_px - gap * sib_gaps).max(1) as f32;
  #[allow(clippy::cast_precision_loss)]
  let gap_f = gap as f32;
  let new_avail = (old_avail + gap_f).max(1.0);
  let merged =
    ((nodes[ben_idx].size + nodes[pos].size) * old_avail + gap_f)
      / new_avail;

  for (idx, node) in nodes.iter_mut().enumerate() {
    if idx != pos && idx != ben_idx {
      node.size = (node.size * old_avail) / new_avail;
    }
  }
  nodes[ben_idx].size = merged;
  nodes.remove(pos);
  true
}

fn sim_apply_void_match(
  nodes: &mut [SimNode],
  new_win_id: Uuid,
  slot_id: Uuid,
  kind: &VoidMatchKind,
) -> bool {
  for node in nodes.iter_mut() {
    if node.id == slot_id {
      match kind {
        VoidMatchKind::Exact => {
          node.id = new_win_id;
          node.kind = SimKind::Window;
        }
        VoidMatchKind::SplitHalf { split_dir, insert_first } => {
          let void_half = SimNode {
            id: slot_id,
            size: 0.5,
            kind: SimKind::Void,
          };
          let win_half = SimNode {
            id: new_win_id,
            size: 0.5,
            kind: SimKind::Window,
          };
          let pair = if *insert_first {
            vec![win_half, void_half]
          } else {
            vec![void_half, win_half]
          };
          node.id = Uuid::new_v4();
          node.kind = SimKind::Split {
            direction: split_dir.clone(),
            children: pair,
          };
        }
      }
      return true;
    }
    if let SimKind::Split { children, .. } = &mut node.kind {
      if sim_apply_void_match(children, new_win_id, slot_id, kind) {
        return true;
      }
    }
  }
  false
}

fn sim_has_adjacent_void(
  nodes: &[SimNode],
  origin_id: Uuid,
) -> Option<bool> {
  if let Some(pos) = nodes.iter().position(|n| n.id == origin_id) {
    let prev_v = pos > 0 && nodes[pos - 1].is_void();
    let next_v = pos + 1 < nodes.len() && nodes[pos + 1].is_void();
    return Some(prev_v || next_v);
  }
  for node in nodes {
    if let SimKind::Split { children, .. } = &node.kind {
      if let Some(found) = sim_has_adjacent_void(children, origin_id) {
        return Some(found);
      }
    }
  }
  None
}

fn sim_wrap_leaf(
  nodes: &mut [SimNode],
  leaf_id: Uuid,
  new_win_id: Uuid,
  split_dir: &TilingDirection,
  insert_first: bool,
) -> bool {
  for node in nodes.iter_mut() {
    if node.id == leaf_id {
      let mut leaf_node = SimNode {
        id: leaf_id,
        size: 0.5,
        kind: SimKind::Void,
      };
      std::mem::swap(&mut leaf_node.kind, &mut node.kind);
      let win_node = SimNode {
        id: new_win_id,
        size: 0.5,
        kind: SimKind::Window,
      };
      node.id = Uuid::new_v4();
      node.kind = SimKind::Split {
        direction: split_dir.clone(),
        children: if insert_first {
          vec![win_node, leaf_node]
        } else {
          vec![leaf_node, win_node]
        },
      };
      return true;
    }
    if let SimKind::Split { children, .. } = &mut node.kind {
      if sim_wrap_leaf(
        children,
        leaf_id,
        new_win_id,
        split_dir,
        insert_first,
      ) {
        return true;
      }
    }
  }
  false
}

fn empty_workspace_snap_rect(
  ws_rect: &Rect,
  zone: ScreenZone,
  h_gap: i32,
  v_gap: i32,
) -> Rect {
  match zone {
    ScreenZone::Left
    | ScreenZone::Right
    | ScreenZone::Top
    | ScreenZone::Bottom => {
      let (dir, is_first) = edge_direction_and_side(zone);
      let gap = gap_for_dir(&dir, h_gap, v_gap);
      split_half_rect(ws_rect, &dir, is_first, gap)
    }
    ScreenZone::TopLeft
    | ScreenZone::TopRight
    | ScreenZone::BottomLeft
    | ScreenZone::BottomRight => {
      let (is_left, is_top) = corner_sides(zone);
      let half = split_half_rect(
        ws_rect,
        &TilingDirection::Horizontal,
        is_left,
        h_gap,
      );
      split_half_rect(&half, &TilingDirection::Vertical, is_top, v_gap)
    }
    ScreenZone::TopCenter => ws_rect.clone(),
  }
}

fn compute_snap_zone_preview_rect(
  workspace: &Workspace,
  zone: ScreenZone,
  exclude_slot_id: Option<Uuid>,
  cursor: Option<&Point>,
) -> anyhow::Result<Rect> {
  let ws_rect = workspace.multi_window_workspace_rect()?;
  let (h_gap, v_gap) = workspace.workspace_inner_gaps()?;

  if zone == ScreenZone::TopCenter {
    return Ok(ws_rect);
  }

  let mut sim =
    SimWorkspace::from_workspace(workspace, ws_rect.clone(), h_gap, v_gap);

  let origin_void_id =
    exclude_slot_id.and_then(|id| sim.vacate_origin(id));

  if !sim.has_real_windows() {
    return Ok(empty_workspace_snap_rect(&ws_rect, zone, h_gap, v_gap));
  }

  let new_win_id = Uuid::new_v4();
  if let Some((slot_id, kind)) =
    sim.find_matching_void(zone, origin_void_id, cursor)
  {
    sim.apply_void_match(new_win_id, slot_id, &kind);
    if let Some(oid) = origin_void_id {
      sim.cleanup_origin_void_after_match(oid);
    }
    sim.normalize();
    sim.absorb_slivers();
    let fallback = empty_workspace_snap_rect(&ws_rect, zone, h_gap, v_gap);
    return Ok(sim.rect_of(new_win_id).unwrap_or(fallback));
  }

  if let Some(oid) = origin_void_id {
    SimWorkspace::detach_preferring_non_void_in(&mut sim.children, oid);
    sim.normalize();
  }

  if sim.children.is_empty()
    || (sim.children.len() == 1 && sim.children[0].is_window())
  {
    return Ok(empty_workspace_snap_rect(&ws_rect, zone, h_gap, v_gap));
  }

  sim.insert_populated(new_win_id, zone);
  sim.normalize();
  sim.absorb_slivers();
  let fallback = empty_workspace_snap_rect(&ws_rect, zone, h_gap, v_gap);
  Ok(sim.rect_of(new_win_id).unwrap_or(fallback))
}

#[derive(Clone)]
enum VoidMatchKind {
  Exact,
  SplitHalf {
    split_dir: TilingDirection,
    insert_first: bool,
  },
}

#[allow(clippy::too_many_lines)]
fn match_void_slot_rect(
  rect: &Rect,
  zone: ScreenZone,
  cursor: Option<&Point>,
  ws_rect: &Rect,
  h_gap: i32,
  v_gap: i32,
) -> Option<(u8, VoidMatchKind)> {
  if rect.width() < MIN_VOID_SPAN_PX || rect.height() < MIN_VOID_SPAN_PX {
    return None;
  }

  let near_l = rect.left <= ws_rect.left + h_gap + 8;
  let near_r = rect.right >= ws_rect.right - h_gap - 8;
  let near_t = rect.top <= ws_rect.top + v_gap + 8;
  let near_b = rect.bottom >= ws_rect.bottom - v_gap - 8;
  let is_full_h = rect.height() >= (ws_rect.height() * 2) / 3;
  let is_full_w = rect.width() >= (ws_rect.width() * 2) / 3;
  let is_partial_w = rect.width() < ws_rect.width() - 40;
  let is_partial_h = rect.height() < ws_rect.height() - 40;
  let contains_y = cursor.is_some_and(|pt| {
    pt.y >= rect.top - v_gap - 8 && pt.y <= rect.bottom + v_gap + 8
  });
  let contains_x = cursor.is_some_and(|pt| {
    pt.x >= rect.left - h_gap - 8 && pt.x <= rect.right + h_gap + 8
  });

  match zone {
    ScreenZone::Left if near_l && is_partial_w => {
      let rank = if contains_y {
        0
      } else {
        u8::from(!is_full_h) + 1
      };
      Some((rank, VoidMatchKind::Exact))
    }
    ScreenZone::Right if near_r && is_partial_w => {
      let rank = if contains_y {
        0
      } else {
        u8::from(!is_full_h) + 1
      };
      Some((rank, VoidMatchKind::Exact))
    }
    ScreenZone::Top if near_t && is_partial_h => {
      let rank = if contains_x {
        0
      } else {
        u8::from(!is_full_w) + 1
      };
      Some((rank, VoidMatchKind::Exact))
    }
    ScreenZone::Bottom if near_b && is_partial_h => {
      let rank = if contains_x {
        0
      } else {
        u8::from(!is_full_w) + 1
      };
      Some((rank, VoidMatchKind::Exact))
    }
    ScreenZone::TopLeft
    | ScreenZone::TopRight
    | ScreenZone::BottomLeft
    | ScreenZone::BottomRight => {
      let (is_left, is_top) = corner_sides(zone);
      let horiz_ok = if is_left { near_l } else { near_r };
      let vert_ok = if is_top { near_t } else { near_b };

      if horiz_ok && vert_ok && is_partial_w && is_partial_h {
        Some((0, VoidMatchKind::Exact))
      } else if horiz_ok && is_full_h && is_partial_w {
        Some((
          1,
          VoidMatchKind::SplitHalf {
            split_dir: TilingDirection::Vertical,
            insert_first: is_top,
          },
        ))
      } else if vert_ok && is_full_w && is_partial_h {
        Some((
          1,
          VoidMatchKind::SplitHalf {
            split_dir: TilingDirection::Horizontal,
            insert_first: is_left,
          },
        ))
      } else {
        None
      }
    }
    _ => None,
  }
}

fn find_matching_void(
  workspace: &Workspace,
  zone: ScreenZone,
  origin_void_id: Option<Uuid>,
  cursor: Option<&Point>,
  ws_rect: &Rect,
  h_gap: i32,
  v_gap: i32,
) -> anyhow::Result<Option<VoidMatch>> {
  let slots: Vec<VoidSlot> = workspace
    .descendants()
    .filter_map(|c| c.as_void().cloned())
    .filter(|slot| Some(slot.id()) != origin_void_id)
    .collect();

  let mut best: Option<(u8, VoidMatch)> = None;

  for slot in slots {
    let rect = slot.to_rect()?;
    if let Some((rank, kind)) = match_void_slot_rect(
      &rect,
      zone,
      cursor,
      ws_rect,
      h_gap,
      v_gap,
    ) {
      let matched = match kind {
        VoidMatchKind::Exact => VoidMatch::Exact(slot),
        VoidMatchKind::SplitHalf {
          split_dir,
          insert_first,
        } => VoidMatch::SplitHalf {
          slot,
          split_dir,
          insert_first,
        },
      };
      if best.as_ref().is_none_or(|(best_rank, _)| rank < *best_rank) {
        best = Some((rank, matched));
      }
    }
  }

  Ok(best.map(|(_, matched)| matched))
}

const fn corner_target_point(zone: ScreenZone, ws_rect: &Rect) -> Point {
  match zone {
    ScreenZone::TopLeft => Point {
      x: ws_rect.left,
      y: ws_rect.top,
    },
    ScreenZone::TopRight => Point {
      x: ws_rect.right,
      y: ws_rect.top,
    },
    ScreenZone::BottomLeft => Point {
      x: ws_rect.left,
      y: ws_rect.bottom,
    },
    _ => Point {
      x: ws_rect.right,
      y: ws_rect.bottom,
    },
  }
}

const fn corner_rect_point(zone: ScreenZone, rect: &Rect) -> Point {
  match zone {
    ScreenZone::TopLeft => Point {
      x: rect.left,
      y: rect.top,
    },
    ScreenZone::TopRight => Point {
      x: rect.right,
      y: rect.top,
    },
    ScreenZone::BottomLeft => Point {
      x: rect.left,
      y: rect.bottom,
    },
    _ => Point {
      x: rect.right,
      y: rect.bottom,
    },
  }
}

fn find_corner_leaf(
  workspace: &Workspace,
  zone: ScreenZone,
  ws_rect: &Rect,
) -> anyhow::Result<TilingContainer> {
  let target_pt = corner_target_point(zone, ws_rect);

  let mut best: Option<(i32, TilingContainer)> = None;
  for descendant in workspace.descendants() {
    let Ok(tiling) = descendant.as_tiling_container() else {
      continue;
    };
    if tiling.is_split() {
      continue;
    }

    let rect = tiling.to_rect()?;
    let corner_pt = corner_rect_point(zone, &rect);

    let dist = (corner_pt.x - target_pt.x).abs()
      + (corner_pt.y - target_pt.y).abs();
    if best.as_ref().is_none_or(|(best_dist, _)| dist < *best_dist) {
      best = Some((dist, tiling));
    }
  }

  best
    .map(|(_, leaf)| leaf)
    .context("No tiling leaf container found in workspace.")
}

fn split_side_for_cursor(cursor: &Point, rect: &Rect) -> SplitSide {
  let dx = cursor.x - rect.center_point().x;
  let dy = cursor.y - rect.center_point().y;

  if dx.abs() * rect.height() >= dy.abs() * rect.width() {
    if dx >= 0 {
      SplitSide::Right
    } else {
      SplitSide::Left
    }
  } else if dy >= 0 {
    SplitSide::Bottom
  } else {
    SplitSide::Top
  }
}

fn edge_direction_and_side(zone: ScreenZone) -> (TilingDirection, bool) {
  match zone {
    ScreenZone::Left => (TilingDirection::Horizontal, true),
    ScreenZone::Right => (TilingDirection::Horizontal, false),
    ScreenZone::Top => (TilingDirection::Vertical, true),
    _ => (TilingDirection::Vertical, false),
  }
}

fn corner_sides(zone: ScreenZone) -> (bool, bool) {
  match zone {
    ScreenZone::TopLeft => (true, true),
    ScreenZone::TopRight => (false, true),
    ScreenZone::BottomLeft => (true, false),
    _ => (false, false),
  }
}

fn corner_split_direction(leaf_rect: &Rect) -> TilingDirection {
  let is_wide_strip = leaf_rect.height() < 160
    && leaf_rect.width() > leaf_rect.height() * 2;
  if is_wide_strip {
    TilingDirection::Horizontal
  } else {
    TilingDirection::Vertical
  }
}

fn corner_leaf_insert_first(
  zone: ScreenZone,
  split_dir: &TilingDirection,
) -> bool {
  let (is_left, is_top) = corner_sides(zone);
  match split_dir {
    TilingDirection::Horizontal => is_left,
    TilingDirection::Vertical => is_top,
  }
}

fn gap_for_dir(dir: &TilingDirection, h_gap: i32, v_gap: i32) -> i32 {
  match dir {
    TilingDirection::Horizontal => h_gap,
    TilingDirection::Vertical => v_gap,
  }
}

#[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
fn split_half_rect(
  rect: &Rect,
  direction: &TilingDirection,
  is_first: bool,
  gap: i32,
) -> Rect {
  match direction {
    TilingDirection::Horizontal => {
      let avail_w = (rect.width() - gap).max(1);
      let half_w = (avail_w as f32 * 0.5).round() as i32;
      let x = if is_first {
        rect.x()
      } else {
        rect.x() + half_w + gap
      };
      Rect::from_xy(x, rect.y(), half_w, rect.height())
    }
    TilingDirection::Vertical => {
      let avail_h = (rect.height() - gap).max(1);
      let half_h = (0.5 * avail_h as f32) as i32;
      let y = if is_first {
        rect.y()
      } else {
        rect.y() + half_h + gap
      };
      Rect::from_xy(rect.x(), y, rect.width(), half_h)
    }
  }
}

fn move_container_within_tree_or_attach(
  window: &NonTilingWindow,
  target_workspace: &Workspace,
) -> anyhow::Result<()> {
  detach_container(window.clone().into())?;
  attach_container(
    &window.clone().into(),
    &target_workspace.clone().into(),
    Some(target_workspace.child_count()),
  )?;
  Ok(())
}

fn window_supports_native_maximize(
  window: &WindowContainer,
  state: &WmState,
) -> bool {
  #[cfg(target_os = "windows")]
  if state.dispatcher.has_event_loop() && window.native().id().0 != 0 {
    return window.native().has_window_style(WS_MAXIMIZEBOX);
  }
  let _ = (window, state);
  true
}

#[cfg(test)]
mod tests {
  use tokio::sync::mpsc;
  use wm_common::FloatingStateConfig;
  use wm_platform::Dispatcher;

  use super::*;
  use crate::{
    commands::window::{
      first_void_slot, insert_void_preserving_siblings, unmanage_window,
      vacate_tiling_slot,
    },
    models::{Container, DirectionContainer, Monitor},
  };

  fn setup_state() -> (WmState, UserConfig, Workspace) {
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let (exit_tx, _exit_rx) = mpsc::unbounded_channel();
    let state = WmState::new(Dispatcher::mock(), event_tx, exit_tx);
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

  fn add_floating(
    workspace: &Workspace,
    rect: Rect,
  ) -> NonTilingWindow {
    let win = NonTilingWindow::mock()
      .floating_placement(rect)
      .state(WindowState::Floating(FloatingStateConfig::default()))
      .call();
    attach_container(
      &win.clone().into(),
      &workspace.clone().into(),
      None,
    )
    .unwrap();
    set_focused_descendant(&win.clone().into(), None);
    win
  }

  fn drag_move(
    is_from_floating: bool,
    origin_void_id: Option<Uuid>,
  ) -> ActiveDrag {
    ActiveDrag {
      operation: Some(ActiveDragOperation::Move),
      is_from_floating,
      initial_position: Rect::from_xy(200, 200, 600, 400),
      origin_void_id,
    }
  }

  #[test]
  fn detects_top_center_edges_and_corners() {
    let work = Rect::from_xy(0, 0, 1920, 1032);
    let bounds = Rect::from_xy(0, 0, 1920, 1080);

    assert_eq!(
      detect_screen_zone(&Point { x: 960, y: 4 }, &work, Some(&bounds)),
      Some(ScreenZone::TopCenter)
    );
    assert_eq!(
      detect_screen_zone(&Point { x: 650, y: 0 }, &work, Some(&bounds)),
      Some(ScreenZone::TopCenter)
    );
    assert_eq!(
      detect_screen_zone(&Point { x: 300, y: 4 }, &work, Some(&bounds)),
      Some(ScreenZone::Top)
    );
    assert_eq!(
      detect_screen_zone(&Point { x: 20, y: 10 }, &work, Some(&bounds)),
      Some(ScreenZone::TopLeft)
    );
    assert_eq!(
      detect_screen_zone(&Point { x: 1910, y: 10 }, &work, Some(&bounds)),
      Some(ScreenZone::TopRight)
    );
    assert_eq!(
      detect_screen_zone(&Point { x: 10, y: 500 }, &work, Some(&bounds)),
      Some(ScreenZone::Left)
    );
    assert_eq!(
      detect_screen_zone(&Point { x: 1915, y: 500 }, &work, Some(&bounds)),
      Some(ScreenZone::Right)
    );
    assert_eq!(
      detect_screen_zone(&Point { x: 960, y: 1030 }, &work, Some(&bounds)),
      Some(ScreenZone::Bottom)
    );
    assert_eq!(
      detect_screen_zone(
        &Point { x: 15, y: 1070 },
        &work,
        Some(&bounds),
      ),
      Some(ScreenZone::BottomLeft)
    );
    assert_eq!(
      detect_screen_zone(
        &Point { x: 1910, y: 1070 },
        &work,
        Some(&bounds),
      ),
      Some(ScreenZone::BottomRight)
    );
    assert_eq!(
      detect_screen_zone(&Point { x: 960, y: 500 }, &work, Some(&bounds)),
      None
    );
  }

  #[test]
  fn floating_drag_ignores_interior_tiles_and_snaps_only_at_edges() {
    let (state, config, workspace) = setup_state();
    let tile = TilingWindow::mock()
      .gaps_config(config.value.gaps.clone())
      .call();
    attach_container(
      &tile.clone().into(),
      &workspace.clone().into(),
      None,
    )
    .unwrap();

    let floater =
      add_floating(&workspace, Rect::from_xy(100, 100, 500, 400));
    let floater_c: WindowContainer = floater.into();
    let drag = drag_move(true, None);

    // Interior hover over a tile must not trigger for floating drags.
    let interior = resolve_drop_target(
      &floater_c,
      &Point { x: 500, y: 500 },
      &drag,
      &state,
    )
    .unwrap();
    assert_eq!(interior, None);

    // Top-center triggers maximize.
    let top_center = resolve_drop_target(
      &floater_c,
      &Point { x: 960, y: 5 },
      &drag,
      &state,
    )
    .unwrap();
    assert_eq!(
      top_center,
      Some(DropTarget::Maximize {
        workspace_id: workspace.id(),
      })
    );

    // Screen left edge triggers `SnapZone::Left`.
    let left_edge = resolve_drop_target(
      &floater_c,
      &Point { x: 5, y: 500 },
      &drag,
      &state,
    )
    .unwrap();
    assert_eq!(
      left_edge,
      Some(DropTarget::SnapZone {
        workspace_id: workspace.id(),
        zone: ScreenZone::Left,
      })
    );
  }

  #[test]
  fn edge_and_corner_snapping_populates_voids_matching_preview_rect() {
    let (mut state, config, workspace) = setup_state();

    // 1. Floating w1 snaps Left on empty workspace -> [w1, Void].
    let w1 = add_floating(&workspace, Rect::from_xy(100, 100, 500, 400));
    let drag1 = drag_move(true, None);
    let target1 = resolve_drop_target(
      &w1.clone().into(),
      &Point { x: 4, y: 500 },
      &drag1,
      &state,
    )
    .unwrap()
    .unwrap();
    let preview1 = compute_drop_preview_rect(
      &target1,
      &w1.clone().into(),
      &drag1,
      &state,
    )
    .unwrap()
    .unwrap();
    let res1 = finalize_move_drag(
      &w1,
      Rect::from_xy(100, 100, 500, 400),
      &drag1,
      Some(&Point { x: 4, y: 500 }),
      &mut state,
      &config,
    )
    .unwrap();
    assert_eq!(res1.state(), WindowState::Tiling);
    assert_eq!(res1.to_rect().unwrap(), preview1);
    assert_eq!(workspace.tiling_children().count(), 2);
    assert!(first_void_slot(&workspace).is_some());

    // 2. Floating w2 snaps TopRight -> splits right Void.
    let w2 = add_floating(&workspace, Rect::from_xy(200, 200, 500, 400));
    let drag2 = drag_move(true, None);
    let target2 = resolve_drop_target(
      &w2.clone().into(),
      &Point { x: 1915, y: 10 },
      &drag2,
      &state,
    )
    .unwrap()
    .unwrap();
    let preview2 = compute_drop_preview_rect(
      &target2,
      &w2.clone().into(),
      &drag2,
      &state,
    )
    .unwrap()
    .unwrap();
    let res2 = finalize_move_drag(
      &w2,
      Rect::from_xy(200, 200, 500, 400),
      &drag2,
      Some(&Point { x: 1915, y: 10 }),
      &mut state,
      &config,
    )
    .unwrap();
    assert_eq!(res2.to_rect().unwrap(), preview2);

    // 3. Floating w3 snaps BottomRight -> fills bottom-right Void.
    let w3 = add_floating(&workspace, Rect::from_xy(300, 300, 500, 400));
    let drag3 = drag_move(true, None);
    let target3 = resolve_drop_target(
      &w3.clone().into(),
      &Point { x: 1915, y: 1025 },
      &drag3,
      &state,
    )
    .unwrap()
    .unwrap();
    let preview3 = compute_drop_preview_rect(
      &target3,
      &w3.clone().into(),
      &drag3,
      &state,
    )
    .unwrap()
    .unwrap();
    let res3 = finalize_move_drag(
      &w3,
      Rect::from_xy(300, 300, 500, 400),
      &drag3,
      Some(&Point { x: 1915, y: 1025 }),
      &mut state,
      &config,
    )
    .unwrap();
    assert_eq!(res3.to_rect().unwrap(), preview3);
    assert!(first_void_slot(&workspace).is_none());
  }

  #[test]
  fn void_lifecycle_horizontal_persists_vertical_auto_maximizes() {
    let (mut state, config, workspace) = setup_state();

    // Horizontal layout: [a(0.5), b(0.5)]. Closing b leaves a VoidSlot.
    let a = TilingWindow::mock()
      .gaps_config(config.value.gaps.clone())
      .call();
    let b = TilingWindow::mock()
      .gaps_config(config.value.gaps.clone())
      .call();
    attach_container(&a.clone().into(), &workspace.clone().into(), None)
      .unwrap();
    attach_container(&b.clone().into(), &workspace.clone().into(), None)
      .unwrap();

    unmanage_window(b.into(), &mut state, &config).unwrap();
    assert!(first_void_slot(&workspace).is_some());
    assert_eq!(workspace.tiling_windows().count(), 1);
    assert_eq!(a.state(), WindowState::Tiling);

    // Closing a too clears all voids cleanly.
    unmanage_window(a.into(), &mut state, &config).unwrap();
    assert!(first_void_slot(&workspace).is_none());
    assert_eq!(workspace.tiling_windows().count(), 0);

    // Vertical layout: V[c, d]. Closing d expands c -> auto-maximize.
    workspace.set_tiling_direction(TilingDirection::Vertical);
    let c = TilingWindow::mock()
      .gaps_config(config.value.gaps.clone())
      .call();
    let d = TilingWindow::mock()
      .gaps_config(config.value.gaps.clone())
      .call();
    attach_container(&c.clone().into(), &workspace.clone().into(), None)
      .unwrap();
    attach_container(&d.clone().into(), &workspace.clone().into(), None)
      .unwrap();
    let c_id = c.id();

    unmanage_window(d.into(), &mut state, &config).unwrap();
    assert!(first_void_slot(&workspace).is_none());
    let sole_c = state
      .window_by_id(c_id)
      .unwrap()
      .as_non_tiling_window()
      .unwrap()
      .clone();
    assert!(matches!(
      sole_c.state(),
      WindowState::Fullscreen(FullscreenStateConfig {
        maximized: true,
        ..
      })
    ));
  }

  #[test]
  fn dragging_tiled_window_over_tile_swaps_slots() {
    let (mut state, config, workspace) = setup_state();

    let w1 = TilingWindow::mock()
      .gaps_config(config.value.gaps.clone())
      .call();
    let w2 = TilingWindow::mock()
      .gaps_config(config.value.gaps.clone())
      .call();
    attach_container(&w1.clone().into(), &workspace.clone().into(), None)
      .unwrap();
    attach_container(&w2.clone().into(), &workspace.clone().into(), None)
      .unwrap();
    let w1_id = w1.id();
    let w2_id = w2.id();
    let w1_rect = w1.to_rect().unwrap();

    let origin_void_id =
      vacate_tiling_slot(&w2, &workspace, true, &mut state, &config)
        .unwrap()
        .unwrap();
    let w2_floating = NonTilingWindow::new(
      Some(w2_id),
      w2.native().clone(),
      w2.native_properties().clone(),
      WindowState::Floating(FloatingStateConfig::default()),
      None,
      w2.border_delta(),
      None,
      w2.floating_placement(),
      false,
      vec![],
      Some(drag_move(false, Some(origin_void_id))),
    );
    attach_container(
      &w2_floating.clone().into(),
      &workspace.clone().into(),
      None,
    )
    .unwrap();

    let drag_swap = drag_move(false, Some(origin_void_id));
    finalize_move_drag(
      &w2_floating,
      Rect::from_xy(100, 100, 400, 300),
      &drag_swap,
      Some(&w1_rect.center_point()),
      &mut state,
      &config,
    )
    .unwrap();

    let children: Vec<_> = workspace.tiling_children().collect();
    assert_eq!(children.len(), 2);
    assert_eq!(children[0].id(), w2_id);
    assert_eq!(children[1].id(), w1_id);
  }

  #[test]
  fn dragging_tiled_window_to_top_center_maximizes_both() {
    let (mut state, config, workspace) = setup_state();

    let w1 = TilingWindow::mock()
      .gaps_config(config.value.gaps.clone())
      .call();
    let w2 = TilingWindow::mock()
      .gaps_config(config.value.gaps.clone())
      .call();
    attach_container(&w1.clone().into(), &workspace.clone().into(), None)
      .unwrap();
    attach_container(&w2.clone().into(), &workspace.clone().into(), None)
      .unwrap();
    let w1_id = w1.id();
    let w2_id = w2.id();

    w2.set_active_drag(Some(drag_move(false, None)));
    let w2_float = update_window_state(
      w2.into(),
      WindowState::Floating(FloatingStateConfig::default()),
      &mut state,
      &config,
    )
    .unwrap();
    let w2_float = w2_float.as_non_tiling_window().unwrap().clone();
    let drag_max = w2_float.active_drag().unwrap();

    let maxed_w2 = finalize_move_drag(
      &w2_float,
      Rect::from_xy(300, 100, 400, 300),
      &drag_max,
      Some(&Point { x: 960, y: 4 }),
      &mut state,
      &config,
    )
    .unwrap();

    assert!(matches!(
      maxed_w2.state(),
      WindowState::Fullscreen(FullscreenStateConfig {
        maximized: true,
        ..
      })
    ));
    assert!(matches!(
      state.window_by_id(w1_id).unwrap().state(),
      WindowState::Fullscreen(FullscreenStateConfig {
        maximized: true,
        ..
      })
    ));
    assert!(first_void_slot(&workspace).is_none());
    assert_eq!(state.focused_container().unwrap().id(), w2_id);
    let w2_prev = maxed_w2.prev_state();
    assert!(matches!(w2_prev, Some(WindowState::Floating(_))));
    assert!(maxed_w2.active_drag().is_none());
  }

  #[test]
  fn maximize_drop_from_floating_preserves_floating_prev_state() {
    let (mut state, config, workspace) = setup_state();
    let w1 = add_floating(&workspace, Rect::from_xy(100, 100, 600, 400));
    let drag = drag_move(true, None);
    w1.set_active_drag(Some(drag.clone()));

    let maxed = finalize_move_drag(
      &w1,
      Rect::from_xy(100, 100, 600, 400),
      &drag,
      Some(&Point { x: 960, y: 4 }),
      &mut state,
      &config,
    )
    .unwrap();

    assert!(matches!(
      maxed.state(),
      WindowState::Fullscreen(FullscreenStateConfig {
        maximized: true,
        ..
      })
    ));
    assert!(matches!(maxed.prev_state(), Some(WindowState::Floating(_))));
    assert!(maxed.active_drag().is_none());
  }

  #[test]
  fn drop_preview_clips_border_delta_edges_to_working_area() {
    let (state, _config, workspace) = setup_state();
    let work = Rect::from_ltrb(0, 0, 1920, 1032);
    let delta = wm_platform::RectDelta::new(
      wm_platform::LengthValue::from_px(7),
      wm_platform::LengthValue::from_px(0),
      wm_platform::LengthValue::from_px(7),
      wm_platform::LengthValue::from_px(7),
    );
    let zero_left = Rect::from_ltrb(0, 0, 956, 1032);
    assert_eq!(
      clamp_rect_edges(&zero_left.apply_delta(&delta, None), &work),
      Rect::from_ltrb(0, 0, 963, 1032)
    );

    let base = add_floating(&workspace, Rect::from_xy(100, 100, 600, 400));
    let drag = drag_move(true, None);
    let left_target = DropTarget::SnapZone {
      workspace_id: workspace.id(),
      zone: ScreenZone::Left,
    };
    let base_c: WindowContainer = base.clone().into();
    let base_preview =
      compute_drop_preview_rect(&left_target, &base_c, &drag, &state)
        .unwrap()
        .unwrap();

    detach_container(base.clone().into()).unwrap();
    let win = NonTilingWindow::new(
      Some(base.id()),
      base.native().clone(),
      base.native_properties().clone(),
      WindowState::Floating(FloatingStateConfig::default()),
      None,
      delta.clone(),
      None,
      base.floating_placement(),
      false,
      vec![],
      None,
    );
    attach_container(&win.clone().into(), &workspace.clone().into(), None)
      .unwrap();

    let win_c: WindowContainer = win.into();
    let preview =
      compute_drop_preview_rect(&left_target, &win_c, &drag, &state)
        .unwrap()
        .unwrap();
    assert_eq!(
      preview,
      clamp_rect_edges(&base_preview.apply_delta(&delta, None), &work)
    );

    let max_target = DropTarget::Maximize {
      workspace_id: workspace.id(),
    };
    let max_preview =
      compute_drop_preview_rect(&max_target, &win_c, &drag, &state)
        .unwrap()
        .unwrap();
    assert_eq!(max_preview, work);
  }

  #[test]
  fn side_void_over_three_quarters_matches_edge_drop_instead_of_half() {
    let (mut state, config, workspace) = setup_state();
    let ws_c: Container = workspace.clone().into();
    workspace.set_tiling_direction(TilingDirection::Horizontal);

    let void_left = VoidSlot::new(config.value.gaps.clone());
    let tile_right = TilingWindow::mock()
      .gaps_config(config.value.gaps.clone())
      .call();
    attach_container(&void_left.clone().into(), &ws_c, None).unwrap();
    attach_container(&tile_right.clone().into(), &ws_c, None).unwrap();
    void_left.set_tiling_size(0.8);
    tile_right.set_tiling_size(0.2);

    let floating =
      add_floating(&workspace, Rect::from_xy(300, 300, 500, 400));
    let drag = drag_move(true, None);
    let left_edge = Point { x: 8, y: 500 };

    let target = resolve_drop_target(
      &floating.clone().into(),
      &left_edge,
      &drag,
      &state,
    )
    .unwrap()
    .unwrap();
    let preview = compute_drop_preview_rect_at(
      &target,
      &floating.clone().into(),
      &drag,
      Some(&left_edge),
      &state,
    )
    .unwrap()
    .unwrap();
    assert_eq!(preview, void_left.to_rect().unwrap());

    let committed = finalize_move_drag(
      &floating,
      floating.floating_placement(),
      &drag,
      Some(&left_edge),
      &mut state,
      &config,
    )
    .unwrap();
    assert_eq!(committed.to_rect().unwrap(), void_left.to_rect().unwrap());
  }

  #[test]
  fn nested_side_void_matches_edge_drop() {
    let (mut state, config, workspace) = setup_state();
    let ws_c: Container = workspace.clone().into();
    workspace.set_tiling_direction(TilingDirection::Horizontal);

    let left_split = SplitContainer::new(
      TilingDirection::Vertical,
      config.value.gaps.clone(),
    );
    left_split.set_tiling_size(0.5);
    let tile_right = TilingWindow::mock()
      .tiling_size(0.5)
      .gaps_config(config.value.gaps.clone())
      .call();
    attach_container(&left_split.clone().into(), &ws_c, None).unwrap();
    attach_container(&tile_right.into(), &ws_c, None).unwrap();

    let top_left = TilingWindow::mock()
      .tiling_size(0.5)
      .gaps_config(config.value.gaps.clone())
      .call();
    let void_bl = VoidSlot::new(config.value.gaps.clone());
    void_bl.set_tiling_size(0.5);
    let split_c: Container = left_split.into();
    attach_container(&top_left.into(), &split_c, None).unwrap();
    attach_container(&void_bl.clone().into(), &split_c, None).unwrap();

    let floating =
      add_floating(&workspace, Rect::from_xy(300, 300, 500, 400));
    let drag = drag_move(true, None);
    let left_edge = Point { x: 8, y: 800 };

    let committed = finalize_move_drag(
      &floating,
      floating.floating_placement(),
      &drag,
      Some(&left_edge),
      &mut state,
      &config,
    )
    .unwrap();

    assert_eq!(committed.to_rect().unwrap(), void_bl.to_rect().unwrap());
  }

  #[test]
  fn single_axis_breakaway_ignores_titlebar_slide_until_outer_rail() {
    let work = Rect::from_xy(0, 0, 1920, 1032);
    let bounds = Rect::from_xy(0, 0, 1920, 1080);
    let start_h = Point { x: 1100, y: 18 };

    assert_eq!(
      detect_single_axis_breakaway_zone(
        &Point { x: 12, y: 18 },
        &work,
        Some(&bounds),
        SingleAxisKind::Horizontal,
        &start_h,
      ),
      None
    );
    assert_eq!(
      detect_single_axis_breakaway_zone(
        &Point { x: 1912, y: 18 },
        &work,
        Some(&bounds),
        SingleAxisKind::Horizontal,
        &start_h,
      ),
      None
    );
    assert_eq!(
      detect_single_axis_breakaway_zone(
        &Point { x: 960, y: 2 },
        &work,
        Some(&bounds),
        SingleAxisKind::Horizontal,
        &start_h,
      ),
      Some(ScreenZone::TopCenter)
    );
    assert_eq!(
      detect_single_axis_breakaway_zone(
        &Point { x: 1880, y: 1025 },
        &work,
        Some(&bounds),
        SingleAxisKind::Horizontal,
        &start_h,
      ),
      Some(ScreenZone::BottomRight)
    );

    let start_v = Point { x: 400, y: 20 };
    assert_eq!(
      detect_single_axis_breakaway_zone(
        &Point { x: 400, y: 2 },
        &work,
        Some(&bounds),
        SingleAxisKind::Vertical,
        &start_v,
      ),
      None
    );
    assert_eq!(
      detect_single_axis_breakaway_zone(
        &Point { x: 3, y: 500 },
        &work,
        Some(&bounds),
        SingleAxisKind::Vertical,
        &start_v,
      ),
      Some(ScreenZone::Left)
    );
  }

  fn drop_at(
    win: &NonTilingWindow,
    pt: &Point,
    state: &mut WmState,
    config: &UserConfig,
  ) -> (Rect, Rect) {
    let drag = drag_move(true, None);
    let c: WindowContainer = win.clone().into();
    let t = resolve_drop_target(&c, pt, &drag, state).unwrap().unwrap();
    let p = Some(pt);
    let pr = compute_drop_preview_rect_at(&t, &c, &drag, p, state);
    let preview = pr.unwrap().unwrap();
    let placed = win.floating_placement();
    let res = finalize_move_drag(win, placed, &drag, p, state, config);
    (preview, res.unwrap().to_rect().unwrap())
  }

  #[test]
  fn vertical_taskbar_and_bottom_insert_with_void_leave_no_gaps() {
    let (event_tx, _event_rx) = mpsc::unbounded_channel();
    let (exit_tx, _exit_rx) = mpsc::unbounded_channel();
    let mut state = WmState::new(Dispatcher::mock(), event_tx, exit_tx);
    let config = UserConfig::mock();

    let workspace = Workspace::mock()
      .gaps_config(config.value.gaps.clone())
      .call();
    let monitor = Monitor::mock()
      .bounds(Rect::from_xy(0, 0, 1360, 768))
      .working_area(Rect::from_ltrb(0, 0, 1304, 768))
      .workspaces(vec![workspace.clone()])
      .call();
    attach_container(
      &monitor.into(),
      &state.root_container.clone().into(),
      None,
    )
    .unwrap();
    set_focused_descendant(&workspace.clone().into(), None);

    // Build [Win1(0.5), Void(0.5)], then detach Win1's left edge inward.
    let w1 = TilingWindow::mock()
      .gaps_config(config.value.gaps.clone())
      .call();
    let ws_c: Container = workspace.clone().into();
    let right_void = VoidSlot::new(config.value.gaps.clone());
    attach_container(&w1.clone().into(), &ws_c, None).unwrap();
    attach_container(&right_void.into(), &ws_c, None).unwrap();

    let ws_rect = workspace.multi_window_workspace_rect().unwrap();
    let (h_gap, _v_gap) = workspace.workspace_inner_gaps().unwrap();
    let parent_dir: DirectionContainer = workspace.clone().into();
    insert_void_preserving_siblings(
      &parent_dir,
      &w1.clone().into(),
      true,
      60,
      h_gap,
      ws_rect.width(),
      config.value.gaps.clone(),
    )
    .unwrap();

    let w2 = add_floating(&workspace, Rect::from_xy(200, 200, 400, 300));
    let (prev2, rect2) =
      drop_at(&w2, &Point { x: 650, y: 760 }, &mut state, &config);
    assert_eq!(rect2, prev2);
    assert!((rect2.bottom - ws_rect.bottom).abs() <= 1);

    let w3 = add_floating(&workspace, Rect::from_xy(300, 200, 400, 300));
    let (prev3, rect3) =
      drop_at(&w3, &Point { x: 1300, y: 400 }, &mut state, &config);
    assert!((rect3.right - prev3.right).abs() <= 2);
    assert!((rect3.right - ws_rect.right).abs() <= 2);
  }

  #[test]
  fn full_height_and_width_edge_entries_do_not_shrink_against_voids() {
    // 1) Full-height W1=Left(0.5), W2=TopRight(0.5) + Void_BR ->
    // breaking away W1 to Top consumes Void_BR, keeps half height.
    let (mut state, config, workspace) = setup_state();
    let ws_c: Container = workspace.clone().into();
    workspace.set_tiling_direction(TilingDirection::Horizontal);
    let w1 = TilingWindow::mock()
      .gaps_config(config.value.gaps.clone())
      .call();
    let split_r = SplitContainer::new(
      TilingDirection::Vertical,
      config.value.gaps.clone(),
    );
    let w2 = TilingWindow::mock()
      .gaps_config(config.value.gaps.clone())
      .call();
    let void_br = VoidSlot::new(config.value.gaps.clone());
    attach_container(&w1.clone().into(), &ws_c, None).unwrap();
    attach_container(&split_r.clone().into(), &ws_c, None).unwrap();
    attach_container(&w2.into(), &split_r.clone().into(), None).unwrap();
    attach_container(&void_br.into(), &split_r.into(), None).unwrap();

    let ws_rect = workspace.multi_window_workspace_rect().unwrap();
    let drag1 = drag_move(false, None);
    let top_target = DropTarget::SnapZone {
      workspace_id: workspace.id(),
      zone: ScreenZone::Top,
    };
    let preview_top = compute_drop_preview_rect_at(
      &top_target,
      &w1.clone().into(),
      &drag1,
      Some(&Point { x: 200, y: 4 }),
      &state,
    )
    .unwrap()
    .unwrap();
    w1.set_active_drag(Some(drag1));
    let w1_float = update_window_state(
      w1.into(),
      WindowState::Floating(FloatingStateConfig::default()),
      &mut state,
      &config,
    )
    .unwrap()
    .as_non_tiling_window()
    .unwrap()
    .clone();
    let w1_drag = w1_float.active_drag().unwrap();
    let comm_w1 = finalize_move_drag(
      &w1_float,
      Rect::from_xy(100, 100, 400, 400),
      &w1_drag,
      Some(&Point { x: 200, y: 4 }),
      &mut state,
      &config,
    )
    .unwrap();
    let rect_w1 = comm_w1.to_rect().unwrap();
    assert_eq!(rect_w1, preview_top);
    assert!((rect_w1.height() - ws_rect.height() / 2).abs() <= 20);

    // 2) Full-width W_top=Top(0.5), W_bl=BottomLeft(0.5) + Void_BR ->
    // breaking away W_top to Left consumes Void_BR, keeps half width.
    let (mut vert_state, vert_cfg, vert_ws) = setup_state();
    let vert_ws_c: Container = vert_ws.clone().into();
    vert_ws.set_tiling_direction(TilingDirection::Vertical);
    let w_top = TilingWindow::mock()
      .gaps_config(vert_cfg.value.gaps.clone())
      .call();
    let split_b = SplitContainer::new(
      TilingDirection::Horizontal,
      vert_cfg.value.gaps.clone(),
    );
    let w_bl = TilingWindow::mock()
      .gaps_config(vert_cfg.value.gaps.clone())
      .call();
    let void_b_r = VoidSlot::new(vert_cfg.value.gaps.clone());
    attach_container(&w_top.clone().into(), &vert_ws_c, None).unwrap();
    attach_container(&split_b.clone().into(), &vert_ws_c, None).unwrap();
    attach_container(&w_bl.into(), &split_b.clone().into(), None).unwrap();
    attach_container(&void_b_r.into(), &split_b.into(), None).unwrap();

    let vert_ws_rect = vert_ws.multi_window_workspace_rect().unwrap();
    let vert_drag = drag_move(false, None);
    let left_target = DropTarget::SnapZone {
      workspace_id: vert_ws.id(),
      zone: ScreenZone::Left,
    };
    let preview_left = compute_drop_preview_rect_at(
      &left_target,
      &w_top.clone().into(),
      &vert_drag,
      Some(&Point { x: 4, y: 200 }),
      &vert_state,
    )
    .unwrap()
    .unwrap();
    w_top.set_active_drag(Some(vert_drag));
    let w_top_float = update_window_state(
      w_top.into(),
      WindowState::Floating(FloatingStateConfig::default()),
      &mut vert_state,
      &vert_cfg,
    )
    .unwrap()
    .as_non_tiling_window()
    .unwrap()
    .clone();
    let w_top_drag = w_top_float.active_drag().unwrap();
    let comm_wt = finalize_move_drag(
      &w_top_float,
      Rect::from_xy(100, 100, 400, 400),
      &w_top_drag,
      Some(&Point { x: 4, y: 200 }),
      &mut vert_state,
      &vert_cfg,
    )
    .unwrap();
    let rect_wt = comm_wt.to_rect().unwrap();
    assert_eq!(rect_wt, preview_left);
    assert!((rect_wt.width() - vert_ws_rect.width() / 2).abs() <= 20);
  }

  #[test]
  fn horizontal_full_width_single_axis_supports_top_center_maximize() {
    let work = Rect::from_xy(0, 0, 1920, 1032);
    let bounds = Rect::from_xy(0, 0, 1920, 1080);
    let start_v = Point { x: 960, y: 20 };

    assert_eq!(
      detect_single_axis_breakaway_zone(
        &Point { x: 960, y: 2 },
        &work,
        Some(&bounds),
        SingleAxisKind::Vertical,
        &start_v,
      ),
      Some(ScreenZone::TopCenter)
    );
  }
}
