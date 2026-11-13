use anyhow::Context;
use uuid::Uuid;
use wm_common::{
  FullscreenStateConfig, GapsConfig, TilingDirection, WindowState,
};

use super::update_window_state;
use crate::{
  commands::container::{
    flatten_child_split_containers, flatten_split_container,
    set_focused_descendant,
  },
  models::{
    Container, DirectionContainer, SplitContainer, TilingContainer,
    TilingWindow, VoidSlot, Workspace,
  },
  traits::{
    CommonGetters, TilingDirectionGetters, TilingSizeGetters,
    MIN_TILING_SIZE,
  },
  user_config::UserConfig,
  wm_state::WmState,
};

/// Returns the first `VoidSlot` descendant within `workspace`, if any.
#[must_use]
pub fn first_void_slot(workspace: &Workspace) -> Option<VoidSlot> {
  workspace
    .descendants()
    .find_map(|container| container.as_void().cloned())
}

/// Finds a `VoidSlot` by ID within `workspace`.
#[must_use]
pub fn void_slot_by_id(
  workspace: &Workspace,
  void_id: Uuid,
) -> Option<VoidSlot> {
  workspace
    .descendants()
    .filter_map(|container| container.as_void().cloned())
    .find(|slot| slot.id() == void_id)
}

/// Replaces `old_slot` in-place with a detached `new_slot`, preserving
/// `tiling_size` and updating `child_focus_order` cleanly.
pub fn replace_tiling_slot(
  old_slot: &TilingContainer,
  new_slot: &TilingContainer,
) -> anyhow::Result<()> {
  if !new_slot.is_detached() {
    anyhow::bail!("Replacement tiling slot must be detached.");
  }

  let parent = old_slot.parent().context("Old slot has no parent.")?;
  let index = old_slot.index();
  let preserved_size = old_slot.tiling_size();

  let old_focus_index = parent
    .borrow_child_focus_order()
    .iter()
    .position(|id| *id == old_slot.id());

  if let Some(slot) = parent.borrow_children_mut().get_mut(index) {
    *slot = new_slot.clone().into();
  }
  *new_slot.borrow_parent_mut() = Some(parent.clone());
  *old_slot.borrow_parent_mut() = None;
  new_slot.set_tiling_size(preserved_size);

  let mut focus_order = parent.borrow_child_focus_order_mut();
  focus_order.retain(|id| *id != old_slot.id());
  if !new_slot.is_void() {
    match old_focus_index {
      Some(focus_idx) => {
        let clamped_idx = focus_idx.min(focus_order.len());
        focus_order.insert(clamped_idx, new_slot.id());
      }
      None => focus_order.push_back(new_slot.id()),
    }
  }

  Ok(())
}

/// Swaps two attached `TilingContainer`s in-place, exchanging their
/// parents, sibling indices, and `tiling_size` values.
pub fn swap_tiling_containers(
  first: &TilingContainer,
  second: &TilingContainer,
) -> anyhow::Result<()> {
  if first.id() == second.id() {
    return Ok(());
  }

  let parent_a = first.parent().context("First container has no parent.")?;
  let parent_b =
    second.parent().context("Second container has no parent.")?;
  let index_a = first.index();
  let index_b = second.index();
  let size_a = first.tiling_size();
  let size_b = second.tiling_size();

  let focus_a = parent_a
    .borrow_child_focus_order()
    .iter()
    .position(|id| *id == first.id());
  let focus_b = parent_b
    .borrow_child_focus_order()
    .iter()
    .position(|id| *id == second.id());

  if parent_a.id() == parent_b.id() {
    let mut children = parent_a.borrow_children_mut();
    if let Some(slot) = children.get_mut(index_a) {
      *slot = second.clone().into();
    }
    if let Some(slot) = children.get_mut(index_b) {
      *slot = first.clone().into();
    }
  } else {
    if let Some(slot) = parent_a.borrow_children_mut().get_mut(index_a) {
      *slot = second.clone().into();
    }
    if let Some(slot) = parent_b.borrow_children_mut().get_mut(index_b) {
      *slot = first.clone().into();
    }
    *first.borrow_parent_mut() = Some(parent_b.clone());
    *second.borrow_parent_mut() = Some(parent_a.clone());
  }

  first.set_tiling_size(size_b);
  second.set_tiling_size(size_a);

  if parent_a.id() == parent_b.id() {
    if !first.is_void() && !second.is_void() {
      let mut order = parent_a.borrow_child_focus_order_mut();
      if let (Some(idx_a), Some(idx_b)) = (focus_a, focus_b) {
        if let Some(slot) = order.get_mut(idx_a) {
          *slot = second.id();
        }
        if let Some(slot) = order.get_mut(idx_b) {
          *slot = first.id();
        }
      }
    }
  } else {
    {
      let mut order_a = parent_a.borrow_child_focus_order_mut();
      order_a.retain(|id| *id != first.id());
      if !second.is_void() {
        match focus_a {
          Some(idx) => {
            let clamped_idx = idx.min(order_a.len());
            order_a.insert(clamped_idx, second.id());
          }
          None => order_a.push_back(second.id()),
        }
      }
    }
    {
      let mut order_b = parent_b.borrow_child_focus_order_mut();
      order_b.retain(|id| *id != second.id());
      if !first.is_void() {
        match focus_b {
          Some(idx) => {
            let clamped_idx = idx.min(order_b.len());
            order_b.insert(clamped_idx, first.id());
          }
          None => order_b.push_back(first.id()),
        }
      }
    }
  }

  Ok(())
}

/// Detaches `container` from its parent, preferring non-void tiling
/// siblings when redistributing `container.tiling_size()`.
///
/// Flattens a single-child `SplitContainer` parent if left behind.
pub fn detach_tiling_preferring_non_void_siblings(
  container: &TilingContainer,
) -> anyhow::Result<Option<Container>> {
  let parent = container.parent().context("Container has no parent.")?;
  let freed_size = container.tiling_size();

  parent
    .borrow_children_mut()
    .retain(|child| child.id() != container.id());
  parent
    .borrow_child_focus_order_mut()
    .retain(|id| *id != container.id());
  *container.borrow_parent_mut() = None;

  let non_void_siblings: Vec<TilingContainer> = parent
    .tiling_children()
    .filter(|sibling| !sibling.is_void())
    .collect();

  let recipients = if non_void_siblings.is_empty() {
    parent.tiling_children().collect::<Vec<_>>()
  } else {
    non_void_siblings
  };

  if !recipients.is_empty() {
    let available_size: f32 = recipients
      .iter()
      .map(|sibling| sibling.tiling_size() - MIN_TILING_SIZE)
      .sum();

    if available_size > f32::EPSILON {
      for sibling in &recipients {
        let factor =
          (sibling.tiling_size() - MIN_TILING_SIZE) / available_size;
        let next_size = sibling.tiling_size() + factor * freed_size;
        sibling.set_tiling_size(next_size);
      }
    } else {
      #[allow(clippy::cast_precision_loss)]
      let even_delta = freed_size / recipients.len() as f32;
      for sibling in &recipients {
        sibling.set_tiling_size(sibling.tiling_size() + even_delta);
      }
    }
  }

  if let Some(split_parent) = parent.as_split().cloned() {
    if !split_parent.has_children() {
      let grandparent = split_parent.parent();
      if let Some(gp) = &grandparent {
        gp.borrow_children_mut()
          .retain(|child| child.id() != split_parent.id());
        gp.borrow_child_focus_order_mut()
          .retain(|id| *id != split_parent.id());
      }
      *split_parent.borrow_parent_mut() = None;
      return Ok(grandparent);
    }
    if split_parent.child_count() == 1 {
      let grandparent = split_parent.parent();
      flatten_split_container(split_parent)?;
      return Ok(grandparent);
    }
  }

  Ok(Some(parent))
}

/// Removes all `VoidSlot`s and any empty `SplitContainer`s from
/// `workspace`.
pub fn clear_workspace_voids(workspace: &Workspace) -> anyhow::Result<()> {
  let voids: Vec<VoidSlot> = workspace
    .descendants()
    .filter_map(|container| container.as_void().cloned())
    .collect();

  for void_slot in voids {
    if let Some(parent) = void_slot.parent() {
      parent
        .borrow_children_mut()
        .retain(|child| child.id() != void_slot.id());
      parent
        .borrow_child_focus_order_mut()
        .retain(|id| *id != void_slot.id());
      *void_slot.borrow_parent_mut() = None;
    }
  }

  loop {
    let empty_splits: Vec<SplitContainer> = workspace
      .descendants()
      .filter_map(|container| container.as_split().cloned())
      .filter(|split| !split.has_children())
      .collect();

    if empty_splits.is_empty() {
      break;
    }

    for split in empty_splits {
      if let Some(parent) = split.parent() {
        parent
          .borrow_children_mut()
          .retain(|child| child.id() != split.id());
        parent
          .borrow_child_focus_order_mut()
          .retain(|id| *id != split.id());
        *split.borrow_parent_mut() = None;
      }
    }
  }

  Ok(())
}

/// Merges adjacent `VoidSlot`s, removes empty splits, and flattens single
/// or same-direction child splits within `workspace`.
///
/// Clears all voids if no tiling windows remain in `workspace`.
pub fn normalize_workspace_voids(
  workspace: &Workspace,
) -> anyhow::Result<()> {
  if workspace.tiling_windows().next().is_none() {
    return clear_workspace_voids(workspace);
  }

  loop {
    let mut changed = false;

    let mut dir_containers: Vec<DirectionContainer> =
      vec![workspace.clone().into()];
    dir_containers.extend(
      workspace
        .descendants()
        .filter_map(|container| container.as_split().cloned())
        .map(DirectionContainer::from),
    );

    for dir_container in dir_containers {
      if dir_container.is_detached() {
        continue;
      }

      if merge_adjacent_voids(&dir_container) {
        changed = true;
      }

      if let Some(split) = dir_container.as_split().cloned() {
        if !split.has_children() {
          if let Some(parent) = split.parent() {
            parent
              .borrow_children_mut()
              .retain(|child| child.id() != split.id());
            parent
              .borrow_child_focus_order_mut()
              .retain(|id| *id != split.id());
            *split.borrow_parent_mut() = None;
            changed = true;
            continue;
          }
        } else if split.child_count() == 1 {
          flatten_split_container(split)?;
          changed = true;
          continue;
        }
      }

      let parent_dir = dir_container.tiling_direction();
      let tiling_children =
        dir_container.tiling_children().collect::<Vec<_>>();
      let single_split =
        tiling_children.len() == 1 && tiling_children[0].is_split();
      let same_dir_split = tiling_children.iter().any(|child| {
        child
          .as_split()
          .is_some_and(|split| split.tiling_direction() == parent_dir)
      });

      if single_split || same_dir_split {
        flatten_child_split_containers(&dir_container.into())?;
        changed = true;
      }
    }

    if !changed {
      break;
    }
  }

  Ok(())
}

fn merge_adjacent_voids(dir_container: &DirectionContainer) -> bool {
  let children: Vec<Container> =
    dir_container.children().into_iter().collect();
  let mut merged = false;
  let mut current_void: Option<VoidSlot> = None;

  for child in children {
    if let Some(void_slot) = child.as_void() {
      if let Some(anchor) = &current_void {
        anchor
          .set_tiling_size(anchor.tiling_size() + void_slot.tiling_size());
        dir_container
          .borrow_children_mut()
          .retain(|c| c.id() != void_slot.id());
        *void_slot.borrow_parent_mut() = None;
        merged = true;
      } else {
        current_void = Some(void_slot.clone());
      }
    } else if child.as_tiling_container().is_ok() {
      current_void = None;
    }
  }

  merged
}

/// Automatically maximizes the sole remaining tiling window in `workspace`
/// when no voids remain after a void-fill expansion.
///
/// Preserves focus on `preferred_front_id` when specified (e.g. a newly
/// maximized or dragged floating window).
pub fn auto_maximize_sole_window(
  workspace: &Workspace,
  preferred_front_id: Option<Uuid>,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<bool> {
  if first_void_slot(workspace).is_some() {
    return Ok(false);
  }

  let mut tiling_windows = workspace.tiling_windows();
  let Some(sole_window) = tiling_windows.next() else {
    return Ok(false);
  };
  if tiling_windows.next().is_some() {
    return Ok(false);
  }

  let was_sole_focused = sole_window.has_focus(None);
  let fullscreen_state = WindowState::Fullscreen(FullscreenStateConfig {
    maximized: true,
    ..config.value.window_behavior.state_defaults.fullscreen.clone()
  });

  let maximized = update_window_state(
    sole_window.into(),
    fullscreen_state,
    state,
    config,
  )?;

  if let Some(front_id) = preferred_front_id {
    if let Some(front_container) = state.container_by_id(front_id) {
      set_focused_descendant(&front_container, None);
    }
  } else if was_sole_focused {
    set_focused_descendant(&maximized.clone().into(), None);
  }

  state
    .pending_sync
    .queue_container_to_redraw(maximized)
    .queue_workspace_to_reorder(workspace.clone());

  Ok(true)
}

/// Detaches a `TilingWindow` from `workspace` according to the void
/// lifecycle rules:
/// - If it has vertical neighbors, those neighbors expand to fill its slot
///   (and auto-maximize if only one tiling window remains with no voids).
/// - Otherwise, its slot becomes a `VoidSlot` (cleared when no tiling
///   windows remain in `workspace`).
///
/// Returns the created `VoidSlot` ID when `preserve_origin_void` is `true`
/// and a void was left in place.
pub fn vacate_tiling_slot(
  window: &TilingWindow,
  workspace: &Workspace,
  preserve_origin_void: bool,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<Option<Uuid>> {
  let parent = window.parent().context("Window has no parent.")?;
  let has_vertical_neighbors =
    parent.as_direction_container().is_ok_and(|dir| {
      dir.tiling_direction() == TilingDirection::Vertical
        && parent
          .tiling_children()
          .any(|child| child.id() != window.id() && !child.is_void())
    });

  if has_vertical_neighbors {
    detach_tiling_preferring_non_void_siblings(&window.clone().into())?;
    normalize_workspace_voids(workspace)?;
    state
      .pending_sync
      .queue_containers_to_redraw(workspace.tiling_children());
    auto_maximize_sole_window(workspace, None, state, config)?;
    return Ok(None);
  }

  let void_slot = VoidSlot::new(config.value.gaps.clone());
  let void_id = void_slot.id();
  replace_tiling_slot(&window.clone().into(), &void_slot.into())?;

  if workspace.tiling_windows().next().is_none() {
    clear_workspace_voids(workspace)?;
    return Ok(None);
  }

  if !preserve_origin_void {
    normalize_workspace_voids(workspace)?;
  }

  state
    .pending_sync
    .queue_containers_to_redraw(workspace.tiling_children());

  Ok(Some(void_id))
}

/// Collapses an active drag's origin `VoidSlot` so its remaining neighbors
/// expand to fill the slot (and auto-maximize if one tiling window
/// remains).
pub fn collapse_origin_void(
  origin_void_id: Uuid,
  workspace: &Workspace,
  preferred_front_id: Option<Uuid>,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<bool> {
  let Some(void_slot) = void_slot_by_id(workspace, origin_void_id) else {
    return Ok(false);
  };

  let had_siblings = void_slot
    .tiling_siblings()
    .any(|sibling| !sibling.is_void());

  detach_tiling_preferring_non_void_siblings(&void_slot.into())?;
  normalize_workspace_voids(workspace)?;
  state
    .pending_sync
    .queue_containers_to_redraw(workspace.tiling_children());

  if had_siblings {
    auto_maximize_sole_window(
      workspace,
      preferred_front_id,
      state,
      config,
    )?;
  }

  Ok(true)
}

/// Swaps two sibling `TilingContainer`s inside the same `parent` without
/// exchanging their `tiling_size` values.
pub fn reorder_tiling_sibling(
  first: &TilingContainer,
  second: &TilingContainer,
) -> anyhow::Result<()> {
  if first.id() == second.id() {
    return Ok(());
  }

  let parent_a = first.parent().context("First container has no parent.")?;
  let parent_b =
    second.parent().context("Second container has no parent.")?;
  if parent_a.id() != parent_b.id() {
    anyhow::bail!("Containers must share the same parent to reorder.");
  }

  let index_a = first.index();
  let index_b = second.index();

  let focus_a = parent_a
    .borrow_child_focus_order()
    .iter()
    .position(|id| *id == first.id());
  let focus_b = parent_a
    .borrow_child_focus_order()
    .iter()
    .position(|id| *id == second.id());

  {
    let mut children = parent_a.borrow_children_mut();
    if let Some(slot) = children.get_mut(index_a) {
      *slot = second.clone().into();
    }
    if let Some(slot) = children.get_mut(index_b) {
      *slot = first.clone().into();
    }
  }

  if !first.is_void() && !second.is_void() {
    if let (Some(idx_a), Some(idx_b)) = (focus_a, focus_b) {
      let mut order = parent_a.borrow_child_focus_order_mut();
      if let Some(slot) = order.get_mut(idx_a) {
        *slot = second.id();
      }
      if let Some(slot) = order.get_mut(idx_b) {
        *slot = first.id();
      }
    }
  }

  Ok(())
}

/// Absorbs `void_slot` into `beneficiary` within `parent`, reclaiming the
/// removed inner gap into `beneficiary` while keeping all other siblings
/// at their exact pixel sizes.
pub fn absorb_void_into_sibling(
  void_slot: &VoidSlot,
  beneficiary: &TilingContainer,
  parent: &DirectionContainer,
  gap: i32,
  parent_axis_px: i32,
) -> anyhow::Result<()> {
  let children: Vec<TilingContainer> = parent.tiling_children().collect();
  if children.len() < 2 {
    return Ok(());
  }

  #[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
  )]
  let old_avail =
    (parent_axis_px - gap * (children.len() as i32 - 1)).max(1) as f32;
  #[allow(clippy::cast_precision_loss)]
  let gap_f = gap as f32;
  let new_avail = (old_avail + gap_f).max(1.0);

  let merged_size =
    ((beneficiary.tiling_size() + void_slot.tiling_size()) * old_avail
      + gap_f)
      / new_avail;

  for child in &children {
    if child.id() != void_slot.id() && child.id() != beneficiary.id() {
      child.set_tiling_size((child.tiling_size() * old_avail) / new_avail);
    }
  }
  beneficiary.set_tiling_size(merged_size);

  parent
    .borrow_children_mut()
    .retain(|c| c.id() != void_slot.id());
  parent
    .borrow_child_focus_order_mut()
    .retain(|id| *id != void_slot.id());
  *void_slot.borrow_parent_mut() = None;

  Ok(())
}

/// Inserts a new `VoidSlot` of `void_px` beside `target` in `parent`,
/// shrinking `target` by `void_px + gap` while preserving the pixel sizes
/// of all other siblings.
pub fn insert_void_preserving_siblings(
  parent: &DirectionContainer,
  target: &TilingContainer,
  insert_before: bool,
  void_px: i32,
  gap: i32,
  parent_axis_px: i32,
  gaps_config: GapsConfig,
) -> anyhow::Result<Option<VoidSlot>> {
  let children: Vec<TilingContainer> = parent.tiling_children().collect();
  if children.is_empty() {
    return Ok(None);
  }

  #[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap
  )]
  let sibling_count = children.len() as i32;
  let old_avail = (parent_axis_px - gap * (sibling_count - 1)).max(1);
  let new_avail = (parent_axis_px - gap * sibling_count).max(1);

  #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
  let target_old_px =
    (target.tiling_size() * old_avail as f32).round() as i32;
  let max_void_px = target_old_px - gap - 40;
  if max_void_px < 4 {
    return Ok(None);
  }

  let clamped_void_px = void_px.clamp(4, max_void_px);

  #[allow(clippy::cast_precision_loss)]
  let old_avail_f = old_avail as f32;
  #[allow(clippy::cast_precision_loss)]
  let new_avail_f = new_avail as f32;
  #[allow(clippy::cast_precision_loss)]
  let gap_f = gap as f32;
  #[allow(clippy::cast_precision_loss)]
  let void_f = clamped_void_px as f32;

  let target_new_f = target.tiling_size() * old_avail_f - gap_f - void_f;
  if target_new_f <= 0.0 {
    return Ok(None);
  }

  for child in &children {
    if child.id() != target.id() {
      let scaled = (child.tiling_size() * old_avail_f) / new_avail_f;
      child.set_tiling_size(scaled);
    }
  }

  target.set_tiling_size(target_new_f / new_avail_f);

  let void_slot = VoidSlot::new(gaps_config);
  void_slot.set_tiling_size(void_f / new_avail_f);

  let insert_idx = if insert_before {
    target.index()
  } else {
    target.index() + 1
  };
  let void_container: Container = void_slot.clone().into();
  let clamped_idx = insert_idx.min(parent.child_count());
  parent
    .borrow_children_mut()
    .insert(clamped_idx, void_container.clone());
  *void_container.borrow_parent_mut() = Some(parent.clone().into());

  Ok(Some(void_slot))
}
