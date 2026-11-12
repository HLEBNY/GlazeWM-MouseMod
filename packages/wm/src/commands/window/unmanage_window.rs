use anyhow::Context;
use wm_common::{WindowState, WmEvent};

use super::vacate_tiling_slot;
use crate::{
  commands::container::{
    detach_container, flatten_child_split_containers,
    set_focused_descendant,
  },
  models::WindowContainer,
  traits::{CommonGetters, WindowGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

#[allow(clippy::needless_pass_by_value)]
pub fn unmanage_window(
  window: WindowContainer,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let workspace = window.workspace().context("No workspace.")?;
  let ancestors = window.ancestors().take(3).collect::<Vec<_>>();

  state.clear_window_drag_state(window.id());

  // Get container ID to switch focus to after the window has been removed.
  let focus_target_id = state
    .focus_target_after_removal(&window.clone())
    .map(|target| target.id());

  match &window {
    WindowContainer::TilingWindow(tiling_window) => {
      vacate_tiling_slot(tiling_window, &workspace, false, state, config)?;
    }
    WindowContainer::NonTilingWindow(_) => {
      detach_container(window.clone().into())?;

      for ancestor in ancestors.iter().rev() {
        flatten_child_split_containers(ancestor)?;
      }
    }
  }

  state.emit_event(WmEvent::WindowUnmanaged {
    unmanaged_id: window.id(),
    #[allow(clippy::cast_possible_wrap, clippy::unnecessary_cast)]
    unmanaged_handle: window.native().id().0 as isize,
  });

  // Reassign focus to suitable target (re-resolving by ID in case the
  // target auto-maximized from `TilingWindow` to `NonTilingWindow`).
  if let Some(focus_target) =
    focus_target_id.and_then(|id| state.container_by_id(id))
  {
    set_focused_descendant(&focus_target, None);
    state.pending_sync.queue_focus_change();
    state.unmanaged_or_minimized_timestamp =
      Some(std::time::Instant::now());
  }

  // Sibling containers need to be redrawn if the window was tiling.
  if window.state() == WindowState::Tiling {
    state
      .pending_sync
      .queue_containers_to_redraw(workspace.tiling_children());
  }

  Ok(())
}
