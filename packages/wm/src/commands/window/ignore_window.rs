use anyhow::Context;
use wm_common::WindowState;

use super::vacate_tiling_slot;
use crate::{
  commands::container::{
    detach_container, flatten_child_split_containers,
  },
  models::WindowContainer,
  traits::{CommonGetters, WindowGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

#[allow(clippy::needless_pass_by_value)]
pub fn ignore_window(
  window: WindowContainer,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let workspace = window.workspace().context("No workspace.")?;
  let ancestors = window.ancestors().take(3).collect::<Vec<_>>();

  state.clear_window_drag_state(window.id());
  state.ignored_windows.push(window.native().clone());

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

  // Sibling containers need to be redrawn if the window was tiling.
  if window.state() == WindowState::Tiling {
    state
      .pending_sync
      .queue_containers_to_redraw(workspace.tiling_children());
  }

  Ok(())
}
