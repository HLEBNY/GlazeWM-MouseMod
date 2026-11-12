use anyhow::Context;
use uuid::Uuid;
use wm_common::{
  ActiveDrag, ActiveDragOperation, FullscreenStateConfig, TilingDirection,
  WindowState,
};
use wm_platform::{Point, Rect};

use super::{
  collapse_origin_void, normalize_workspace_voids, replace_tiling_slot,
  swap_tiling_containers, update_window_state,
};
use crate::{
  commands::container::{
    attach_container, detach_container, set_focused_descendant,
    wrap_in_split_container,
  },
  events::update_floating_window_position,
  models::{
    NonTilingWindow, SplitContainer, TilingContainer, TilingWindow,
    VoidSlot, WindowContainer, Workspace,
  },
  traits::{
    CommonGetters, PositionGetters, TilingDirectionGetters,
    TilingSizeGetters, WindowGetters,
  },
  user_config::UserConfig,
  wm_state::WmState,
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

/// Computes the exact preview rect in screen coordinates for `target`.
pub fn compute_drop_preview_rect(
  target: &DropTarget,
  dragged_window: &WindowContainer,
  active_drag: &ActiveDrag,
  state: &WmState,
) -> anyhow::Result<Option<Rect>> {
  match target {
    DropTarget::Maximize { workspace_id } => {
      let Some(workspace) = state.workspace_by_id(*workspace_id) else {
        return Ok(None);
      };
      let monitor = workspace.monitor().context("No monitor.")?;
      let can_maximize =
        window_supports_native_maximize(dragged_window, state);
      let rect = if can_maximize {
        monitor.native_properties().working_area
      } else {
        workspace.max_workspace_rect()?
      };
      Ok(Some(rect))
    }
    DropTarget::SwapTile {
      target_window_id, ..
    } => {
      let Some(container) = state.container_by_id(*target_window_id) else {
        return Ok(None);
      };
      Ok(Some(container.to_rect()?))
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
      let (h_gap, v_gap) = workspace.workspace_inner_gaps()?;
      let rect = container.to_rect()?;
      let (dir, is_first, gap) = match side {
        SplitSide::Left => (TilingDirection::Horizontal, true, h_gap),
        SplitSide::Right => (TilingDirection::Horizontal, false, h_gap),
        SplitSide::Top => (TilingDirection::Vertical, true, v_gap),
        SplitSide::Bottom => (TilingDirection::Vertical, false, v_gap),
      };
      Ok(Some(split_half_rect(&rect, &dir, is_first, gap)))
    }
    DropTarget::SnapZone { workspace_id, zone } => {
      let Some(workspace) = state.workspace_by_id(*workspace_id) else {
        return Ok(None);
      };
      Ok(Some(compute_snap_zone_preview_rect(
        &workspace,
        *zone,
        active_drag.origin_void_id,
      )?))
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

  let Some(rect) =
    compute_drop_preview_rect(&target, window, &active_drag, state)?
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
      old_workspace.as_ref().and_then(|ws| ws.monitor().ok()),
      target_workspace.monitor().ok(),
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
  maximized.set_active_drag(None);
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

  let (h_gap, v_gap) = workspace.workspace_inner_gaps()?;
  let ws_rect = workspace.multi_window_workspace_rect()?;

  let void_match = find_matching_void(
    &workspace,
    zone,
    origin_void_in_target,
    &ws_rect,
    h_gap,
    v_gap,
  )?;

  let tiling_window = moved_window.to_tiling(config.value.gaps.clone());
  detach_container(moved_window.clone().into())?;

  if let Some(void_match) = void_match {
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

    if let (Some(origin_void_id), Some(old_ws)) =
      (active_drag.origin_void_id, &old_workspace)
    {
      collapse_origin_void(origin_void_id, old_ws, None, state, config)?;
    }

    normalize_workspace_voids(&workspace)?;
    tiling_window.set_active_drag(None);
    set_focused_descendant(&tiling_window.clone().into(), None);

    state
      .pending_sync
      .queue_containers_to_redraw(workspace.tiling_children())
      .queue_workspace_to_reorder(workspace);

    return Ok(tiling_window.into());
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

  normalize_workspace_voids(&workspace)?;
  tiling_window.set_active_drag(None);
  set_focused_descendant(&tiling_window.clone().into(), None);

  state
    .pending_sync
    .queue_containers_to_redraw(workspace.tiling_children())
    .queue_workspace_to_reorder(workspace);

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
        let insert_idx =
          if is_first { 0 } else { existing_children.len() };
        attach_container(
          &tiling_window.clone().into(),
          &ws_container,
          Some(insert_idx),
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
      let leaf = find_corner_leaf(workspace, None, zone, &ws_rect)?;
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

fn compute_snap_zone_preview_rect(
  workspace: &Workspace,
  zone: ScreenZone,
  origin_void_id: Option<Uuid>,
) -> anyhow::Result<Rect> {
  let ws_rect = workspace.multi_window_workspace_rect()?;
  let (h_gap, v_gap) = workspace.workspace_inner_gaps()?;

  let origin_in_ws = origin_void_id
    .filter(|id| super::void_slot_by_id(workspace, *id).is_some());

  if let Some(void_match) = find_matching_void(
    workspace,
    zone,
    origin_in_ws,
    &ws_rect,
    h_gap,
    v_gap,
  )? {
    return match void_match {
      VoidMatch::Exact(slot) => slot.to_rect(),
      VoidMatch::SplitHalf {
        slot,
        split_dir,
        insert_first,
      } => {
        let slot_rect = slot.to_rect()?;
        let gap = gap_for_dir(&split_dir, h_gap, v_gap);
        Ok(split_half_rect(&slot_rect, &split_dir, insert_first, gap))
      }
    };
  }

  let effective_children: Vec<TilingContainer> = workspace
    .tiling_children()
    .filter(|c| Some(c.id()) != origin_in_ws)
    .collect();

  if effective_children.is_empty()
    || (effective_children.len() == 1
      && effective_children[0].is_tiling_window())
  {
    return Ok(match zone {
      ScreenZone::Left
      | ScreenZone::Right
      | ScreenZone::Top
      | ScreenZone::Bottom => {
        let (dir, is_first) = edge_direction_and_side(zone);
        let gap = gap_for_dir(&dir, h_gap, v_gap);
        split_half_rect(&ws_rect, &dir, is_first, gap)
      }
      ScreenZone::TopLeft
      | ScreenZone::TopRight
      | ScreenZone::BottomLeft
      | ScreenZone::BottomRight => {
        let (is_left, is_top) = corner_sides(zone);
        let half = split_half_rect(
          &ws_rect,
          &TilingDirection::Horizontal,
          is_left,
          h_gap,
        );
        split_half_rect(&half, &TilingDirection::Vertical, is_top, v_gap)
      }
      ScreenZone::TopCenter => ws_rect,
    });
  }

  compute_populated_snap_zone_preview(
    workspace,
    zone,
    origin_in_ws,
    &ws_rect,
    effective_children.len(),
    h_gap,
    v_gap,
  )
}

fn compute_populated_snap_zone_preview(
  workspace: &Workspace,
  zone: ScreenZone,
  origin_in_ws: Option<Uuid>,
  ws_rect: &Rect,
  sibling_count: usize,
  h_gap: i32,
  v_gap: i32,
) -> anyhow::Result<Rect> {
  match zone {
    ScreenZone::Left
    | ScreenZone::Right
    | ScreenZone::Top
    | ScreenZone::Bottom => {
      let (edge_dir, is_first) = edge_direction_and_side(zone);
      let gap = gap_for_dir(&edge_dir, h_gap, v_gap);
      if workspace.tiling_direction() == edge_dir {
        #[allow(clippy::cast_precision_loss)]
        let new_fraction = 1.0 / (sibling_count + 1) as f32;
        Ok(edge_insertion_rect(
          ws_rect,
          &edge_dir,
          is_first,
          gap,
          sibling_count,
          new_fraction,
        ))
      } else {
        Ok(split_half_rect(ws_rect, &edge_dir, is_first, gap))
      }
    }
    ScreenZone::TopLeft
    | ScreenZone::TopRight
    | ScreenZone::BottomLeft
    | ScreenZone::BottomRight => {
      let leaf = find_corner_leaf(workspace, origin_in_ws, zone, ws_rect)?;
      let leaf_rect = effective_leaf_rect(&leaf, origin_in_ws)?;
      let split_dir = corner_split_direction(&leaf_rect);
      let insert_first = corner_leaf_insert_first(zone, &split_dir);
      let gap = gap_for_dir(&split_dir, h_gap, v_gap);
      Ok(split_half_rect(&leaf_rect, &split_dir, insert_first, gap))
    }
    ScreenZone::TopCenter => Ok(ws_rect.clone()),
  }
}

fn find_matching_void(
  workspace: &Workspace,
  zone: ScreenZone,
  origin_void_id: Option<Uuid>,
  ws_rect: &Rect,
  h_gap: i32,
  v_gap: i32,
) -> anyhow::Result<Option<VoidMatch>> {
  let slots: Vec<VoidSlot> = workspace
    .descendants()
    .filter_map(|c| c.as_void().cloned())
    .filter(|slot| Some(slot.id()) != origin_void_id)
    .collect();

  for slot in slots {
    let r = slot.to_rect()?;
    let near_l = r.left <= ws_rect.left + h_gap + 6;
    let near_r = r.right >= ws_rect.right - h_gap - 6;
    let near_t = r.top <= ws_rect.top + v_gap + 6;
    let near_b = r.bottom >= ws_rect.bottom - v_gap - 6;
    let is_full_h = r.height() >= (ws_rect.height() * 2) / 3;
    let is_full_w = r.width() >= (ws_rect.width() * 2) / 3;
    let is_sub_w = r.width() <= (ws_rect.width() * 3) / 4;
    let is_sub_h = r.height() <= (ws_rect.height() * 3) / 4;

    let matched = match zone {
      ScreenZone::Left if near_l && is_full_h && is_sub_w => {
        Some(VoidMatch::Exact(slot))
      }
      ScreenZone::Right if near_r && is_full_h && is_sub_w => {
        Some(VoidMatch::Exact(slot))
      }
      ScreenZone::Top if near_t && is_full_w && is_sub_h => {
        Some(VoidMatch::Exact(slot))
      }
      ScreenZone::Bottom if near_b && is_full_w && is_sub_h => {
        Some(VoidMatch::Exact(slot))
      }
      ScreenZone::TopLeft
      | ScreenZone::TopRight
      | ScreenZone::BottomLeft
      | ScreenZone::BottomRight => {
        let (is_left, is_top) = corner_sides(zone);
        let horiz_ok = if is_left { near_l } else { near_r };
        let vert_ok = if is_top { near_t } else { near_b };

        if horiz_ok && vert_ok && is_sub_w && is_sub_h {
          Some(VoidMatch::Exact(slot))
        } else if horiz_ok && is_full_h && is_sub_w {
          Some(VoidMatch::SplitHalf {
            slot,
            split_dir: TilingDirection::Vertical,
            insert_first: is_top,
          })
        } else if vert_ok && is_full_w && is_sub_h {
          Some(VoidMatch::SplitHalf {
            slot,
            split_dir: TilingDirection::Horizontal,
            insert_first: is_left,
          })
        } else {
          None
        }
      }
      _ => None,
    };

    if matched.is_some() {
      return Ok(matched);
    }
  }

  Ok(None)
}

fn find_corner_leaf(
  workspace: &Workspace,
  exclude_void_id: Option<Uuid>,
  zone: ScreenZone,
  ws_rect: &Rect,
) -> anyhow::Result<TilingContainer> {
  let target_pt = match zone {
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
  };

  let mut best: Option<(i32, TilingContainer)> = None;
  for descendant in workspace.descendants() {
    let Ok(tiling) = descendant.as_tiling_container() else {
      continue;
    };
    if tiling.is_split() || Some(tiling.id()) == exclude_void_id {
      continue;
    }

    let rect = tiling.to_rect()?;
    let corner_pt = match zone {
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
    };

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

fn effective_leaf_rect(
  leaf: &TilingContainer,
  origin_void_id: Option<Uuid>,
) -> anyhow::Result<Rect> {
  let rect = leaf.to_rect()?;
  let Some(origin_id) = origin_void_id else {
    return Ok(rect);
  };

  let Some(parent) = leaf.direction_container() else {
    return Ok(rect);
  };

  let Some(origin_void) = parent
    .tiling_children()
    .find(|child| child.id() == origin_id)
  else {
    return Ok(rect);
  };

  let remaining_sum: f32 = parent
    .tiling_children()
    .filter(|child| child.id() != origin_id)
    .map(|child| child.tiling_size())
    .sum();
  if remaining_sum <= f32::EPSILON {
    return Ok(rect);
  }

  let scale = (remaining_sum + origin_void.tiling_size()) / remaining_sum;
  let parent_rect = parent.to_rect()?;
  let (h_gap, v_gap) = parent.as_tiling_container().ok().map_or_else(
    || {
      leaf
        .workspace()
        .and_then(|ws| ws.workspace_inner_gaps().ok())
        .unwrap_or((0, 0))
    },
    |tc| tc.inner_gaps().unwrap_or((0, 0)),
  );
  let remaining_siblings = parent
    .tiling_children()
    .filter(|child| child.id() != origin_id)
    .count()
    .saturating_sub(1);

  #[allow(
    clippy::cast_possible_wrap,
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss
  )]
  match parent.tiling_direction() {
    TilingDirection::Horizontal => {
      let avail = parent_rect.width() - h_gap * remaining_siblings as i32;
      let mut x = parent_rect.x();
      for sibling in parent
        .tiling_children()
        .filter(|child| child.id() != origin_id)
      {
        let width =
          (avail as f32 * sibling.tiling_size() * scale).round() as i32;
        if sibling.id() == leaf.id() {
          return Ok(Rect::from_xy(
            x,
            parent_rect.y(),
            width,
            parent_rect.height(),
          ));
        }
        x += width + h_gap;
      }
      Ok(rect)
    }
    TilingDirection::Vertical => {
      let avail = parent_rect.height() - v_gap * remaining_siblings as i32;
      let mut y = parent_rect.y();
      for sibling in parent
        .tiling_children()
        .filter(|child| child.id() != origin_id)
      {
        let height =
          (sibling.tiling_size() * scale * avail as f32) as i32;
        if sibling.id() == leaf.id() {
          return Ok(Rect::from_xy(
            parent_rect.x(),
            y,
            parent_rect.width(),
            height,
          ));
        }
        y += height + v_gap;
      }
      Ok(rect)
    }
  }
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

#[allow(
  clippy::cast_possible_truncation,
  clippy::cast_possible_wrap,
  clippy::cast_precision_loss
)]
fn edge_insertion_rect(
  ws_rect: &Rect,
  direction: &TilingDirection,
  is_first: bool,
  gap: i32,
  sibling_count: usize,
  tiling_size: f32,
) -> Rect {
  match direction {
    TilingDirection::Horizontal => {
      let avail_w = (ws_rect.width() - gap * sibling_count as i32).max(1);
      let width = (avail_w as f32 * tiling_size).round() as i32;
      let x = if is_first {
        ws_rect.x()
      } else {
        ws_rect.right - width
      };
      Rect::from_xy(x, ws_rect.y(), width, ws_rect.height())
    }
    TilingDirection::Vertical => {
      let avail_h = (ws_rect.height() - gap * sibling_count as i32).max(1);
      let height = (tiling_size * avail_h as f32) as i32;
      let y = if is_first {
        ws_rect.y()
      } else {
        ws_rect.bottom - height
      };
      Rect::from_xy(ws_rect.x(), y, ws_rect.width(), height)
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
    use wm_platform::{NativeWindowWindowsExt, WS_MAXIMIZEBOX};
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
      first_void_slot, unmanage_window, vacate_tiling_slot,
    },
    models::Monitor,
  };

  fn setup_state() -> (WmState, UserConfig, Workspace) {
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
  }
}
