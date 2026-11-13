use anyhow::Context;
use tracing::{info, warn};
use wm_common::WindowState;

use super::{
  clear_workspace_voids, first_void_slot, normalize_workspace_voids,
  replace_tiling_slot, vacate_tiling_slot,
};
use crate::{
  commands::container::{
    attach_container, detach_container, move_container_within_tree,
    replace_container, resize_tiling_container, set_focused_descendant,
  },
  models::{Container, InsertionTarget, VoidSlot, WindowContainer},
  traits::{CommonGetters, TilingSizeGetters, WindowGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

/// Updates the state of a window.
///
/// Adds the window for redraw if there is a state change.
///
/// Returns the window after the state change.
pub fn update_window_state(
  window: WindowContainer,
  target_state: WindowState,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<WindowContainer> {
  if window.state() == target_state {
    return Ok(window);
  }

  info!("Updating window state: {:?}.", target_state);

  match target_state {
    WindowState::Tiling => set_tiling(&window, state, config),
    _ => set_non_tiling(window, target_state, state, config),
  }
}

/// Updates the state of a window to be `WindowState::Tiling`.
#[allow(clippy::too_many_lines)]
fn set_tiling(
  window: &WindowContainer,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<WindowContainer> {
  let window = window
    .as_non_tiling_window()
    .context("Invalid window state.")?
    .clone();

  let workspace =
    window.workspace().context("Window has no workspace.")?;
  let was_focused = window.has_focus(None);

  // Check whether insertion target is still valid.
  let insertion_target =
    window.insertion_target().filter(|insertion_target| {
      !insertion_target.target_parent.is_detached()
        && insertion_target
          .target_parent
          .workspace()
          .is_some_and(|target_ws| target_ws.id() == workspace.id())
    });

  // Check if the previous slot (or any void in the workspace) is an empty
  // `VoidSlot` ready to receive this window.
  let target_void = insertion_target
    .as_ref()
    .and_then(|target| {
      let children = target.target_parent.children();
      let candidate = children
        .get(target.target_index)
        .or_else(|| {
          target
            .target_index
            .checked_sub(1)
            .and_then(|i| children.get(i))
        });
      candidate.and_then(|c| c.as_void().cloned())
    })
    .or_else(|| first_void_slot(&workspace));

  let tiling_window = window.to_tiling(config.value.gaps.clone());
  tiling_window.update_native_properties(|properties| {
    properties.pending_maximize_until = None;
  });

  if let Some(void_slot) = target_void {
    detach_container(window.into())?;
    replace_tiling_slot(&void_slot.into(), &tiling_window.clone().into())?;
    normalize_workspace_voids(&workspace)?;

    if was_focused {
      set_focused_descendant(&tiling_window.clone().into(), None);
    }

    state
      .pending_sync
      .queue_containers_to_redraw(workspace.tiling_children())
      .queue_workspace_to_reorder(workspace);

    return Ok(tiling_window.into());
  }

  // Hotkey entry onto an empty workspace defaults to half-screen paired
  // with a `VoidSlot`.
  if workspace.tiling_children().next().is_none()
    && window.active_drag().is_none()
  {
    detach_container(window.into())?;
    let void_slot = VoidSlot::new(config.value.gaps.clone());
    attach_container(
      &tiling_window.clone().into(),
      &workspace.clone().into(),
      Some(0),
    )?;
    attach_container(
      &void_slot.into(),
      &workspace.clone().into(),
      Some(1),
    )?;

    if was_focused {
      set_focused_descendant(&tiling_window.clone().into(), None);
    }

    state
      .pending_sync
      .queue_containers_to_redraw(workspace.tiling_children())
      .queue_workspace_to_reorder(workspace);

    return Ok(tiling_window.into());
  }

  // Get the position in the tree to insert the new tiling window. This
  // will be the window's previous tiling position if it has one, or
  // instead beside the last focused tiling window in the workspace.
  let (target_parent, target_index) = insertion_target
    .as_ref()
    .map(|insertion_target| {
      (
        insertion_target.target_parent.clone(),
        insertion_target.target_index,
      )
    })
    // Fallback to the last focused tiling window within the workspace.
    .or_else(|| {
      let focused_window = workspace
        .descendant_focus_order()
        .find(Container::is_tiling_window)?;

      Some((focused_window.parent()?, focused_window.index() + 1))
    })
    // Default to inserting at the end of the workspace.
    .unwrap_or((workspace.clone().into(), workspace.child_count()));

  // Replace the original window with the created tiling window.
  replace_container(
    &tiling_window.clone().into(),
    &window.parent().context("No parent.")?,
    window.index(),
  )?;

  move_container_within_tree(
    &tiling_window.clone().into(),
    &target_parent,
    target_index,
    state,
  )?;

  #[allow(clippy::cast_precision_loss)]
  if let Some(insertion_target) = &insertion_target {
    let size_scale = (insertion_target.prev_sibling_count + 1) as f32
      / (tiling_window.tiling_siblings().count() + 1) as f32;

    // Scale the window's previous size based on the current number of
    // siblings. E.g. if the window was 0.5 with 1 sibling, and now has 2
    // siblings, scale to 0.5 * (2/3) to maintain proportional sizing.
    let target_size = insertion_target.prev_tiling_size * size_scale;
    resize_tiling_container(&tiling_window.clone().into(), target_size);
  }

  state
    .pending_sync
    .queue_containers_to_redraw(target_parent.tiling_children())
    .queue_workspace_to_reorder(workspace);

  Ok(tiling_window.into())
}

/// Updates the state of a window to be either `WindowState::Floating`,
/// `WindowState::Fullscreen`, or `WindowState::Minimized`.
#[allow(clippy::too_many_lines)]
fn set_non_tiling(
  window: WindowContainer,
  target_state: WindowState,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<WindowContainer> {
  let is_maximize_target = matches!(
    &target_state,
    WindowState::Fullscreen(fullscreen) if fullscreen.maximized
  );
  if !is_maximize_target {
    window.update_native_properties(|properties| {
      properties.pending_maximize_until = None;
    });
  }
  // A window can only be updated to a minimized state if it is
  // natively minimized.
  // TODO: Consider doing the same for maximized and fullscreen states.
  if target_state == WindowState::Minimized
    && !window.native_properties().is_minimized
  {
    info!("No window state update. Minimizing window.");

    // TODO: Instead of doing the platform call directly here, instead add
    // a `queue_state_change` method to `PendingSync`.
    if let Err(err) = window.native().minimize() {
      warn!("Failed to minimize window: {}", err);
    }

    return Ok(window);
  }

  let workspace = window.workspace().context("No workspace.")?;

  match window {
    WindowContainer::NonTilingWindow(window) => {
      let current_state = window.state();

      // Update the window's previous state if the discriminant changes.
      // TODO: Move out handling of active drag. Can then simplify calls to
      // `set_active_drag` in `handle_window_moved_or_resized_end`.
      if !current_state.is_same_state(&target_state)
        && window.active_drag().is_none()
      {
        window.set_prev_state(current_state);
        state.pending_sync.queue_workspace_to_reorder(workspace);
      }

      window.set_state(target_state);
      state.pending_sync.queue_container_to_redraw(window.clone());

      Ok(window.into())
    }
    WindowContainer::TilingWindow(window) => {
      let parent = window.parent().context("No parent")?;
      let was_focused = window.has_focus(None);
      let is_active_drag = window.active_drag().is_some();

      let non_tiling_window = window.to_non_tiling(
        target_state.clone(),
        Some(InsertionTarget {
          target_parent: parent,
          target_index: window.index(),
          prev_tiling_size: window.tiling_size(),
          prev_sibling_count: window.tiling_siblings().count(),
        }),
      );

      // Vacate the tiling slot BEFORE attaching `non_tiling_window`
      // (both share the same container ID).
      let origin_void_id = vacate_tiling_slot(
        &window,
        &workspace,
        is_active_drag,
        state,
        config,
      )?;

      if is_active_drag {
        if let Some(mut drag) = non_tiling_window.active_drag() {
          drag.origin_void_id = origin_void_id;
          non_tiling_window.set_active_drag(Some(drag));
        }
      }

      // Non-tiling windows should always be direct children of the
      // workspace.
      attach_container(
        &non_tiling_window.clone().into(),
        &workspace.clone().into(),
        Some(workspace.child_count()),
      )?;

      if was_focused {
        set_focused_descendant(&non_tiling_window.clone().into(), None);
      }

      if workspace.tiling_windows().next().is_none() {
        clear_workspace_voids(&workspace)?;
      }

      state
        .pending_sync
        .queue_container_to_redraw(non_tiling_window.clone())
        .queue_containers_to_redraw(workspace.tiling_children())
        .queue_workspace_to_reorder(workspace);

      Ok(non_tiling_window.into())
    }
  }
}
