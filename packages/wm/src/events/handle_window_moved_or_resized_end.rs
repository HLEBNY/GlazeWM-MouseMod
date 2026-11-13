use anyhow::Context;
use wm_common::{
  try_warn, ActiveDrag, ActiveDragOperation, FullscreenStateConfig,
  WindowState,
};
use wm_platform::LengthValue;

use crate::{
  commands::window::{
    collapse_origin_void, finalize_move_drag, finish_single_axis_drag,
    set_window_size, update_window_state,
  },
  events::{
    update_floating_window_position, update_live_resize_with_finalize,
  },
  models::{TilingWindow, WindowContainer},
  traits::{CommonGetters, WindowGetters},
  user_config::UserConfig,
  wm_state::WmState,
};

/// Handles the event for when a window is finished being moved or resized
/// by the user (e.g. via the window's drag handles).
#[allow(clippy::too_many_lines)]
pub fn handle_window_moved_or_resized_end(
  window: &WindowContainer,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let Some(active_drag) = window.active_drag() else {
    return Ok(());
  };

  state.dispatcher.hide_snap_overlay();

  match &window {
    WindowContainer::NonTilingWindow(window) => {
      let is_maximized = try_warn!(window.native().is_maximized());

      window.update_native_properties(|properties| {
        properties.is_maximized = is_maximized;
        if is_maximized {
          properties.pending_maximize_until = None;
        }
      });

      let nearest_monitor = state
        .nearest_monitor(&window.native())
        .context("Failed to get workspace of nearest monitor.")?;

      let should_fullscreen = window.should_fullscreen(
        &nearest_monitor
          .displayed_workspace()
          .context("No workspace.")?,
      )?;

      if is_maximized || should_fullscreen {
        let fullscreen_state = if let WindowState::Fullscreen(
          fullscreen_state,
        ) = window.state()
        {
          fullscreen_state
        } else {
          config
            .value
            .window_behavior
            .state_defaults
            .fullscreen
            .clone()
        };

        window.set_active_drag(None);

        let updated = update_window_state(
          window.clone().into(),
          WindowState::Fullscreen(FullscreenStateConfig {
            maximized: is_maximized,
            ..fullscreen_state
          }),
          state,
          config,
        )?;

        if let (Some(origin_void_id), Some(workspace)) =
          (active_drag.origin_void_id, updated.workspace())
        {
          collapse_origin_void(
            origin_void_id,
            &workspace,
            Some(updated.id()),
            state,
            config,
          )?;
        }

        state.clear_window_drag_state(updated.id());

        if is_maximized {
          // Dequeue the window from redraw if it's maximized, since the
          // window is already in the correct state.
          state
            .pending_sync
            .dequeue_container_from_redraw(updated);
        } else {
          state.pending_sync.queue_container_to_redraw(updated);
        }

        return Ok(());
      }

      if active_drag.operation == Some(ActiveDragOperation::Resize) {
        update_floating_window_position(
          window,
          window.native_properties().frame,
          &nearest_monitor,
          state,
        )?;
        window.set_active_drag(None);
      } else {
        finalize_move_drag(
          window,
          window.native_properties().frame,
          &active_drag,
          None,
          state,
          config,
        )?;
      }

      state.clear_window_drag_state(window.id());
    }
    WindowContainer::TilingWindow(window) => {
      if state.is_single_axis_drag_window(window.id()) {
        finish_single_axis_drag(None, state, config)?;
      } else {
        finish_tiling_window_drag(window, &active_drag, state)?;
      }
    }
  }

  Ok(())
}

fn finish_tiling_window_drag(
  window: &TilingWindow,
  active_drag: &ActiveDrag,
  state: &mut WmState,
) -> anyhow::Result<()> {
  tracing::info!(
    "Tiling window move/resize ended: {}",
    window.as_window_container()?
  );

  let frame = window.native_properties().frame;
  let is_resize =
    active_drag.operation == Some(ActiveDragOperation::Resize);
  let is_live_resize =
    is_resize && state.is_live_resize_window(window.id());

  let mut resized_workspace_redraw = false;

  if is_live_resize || (is_resize && frame != active_drag.initial_position)
  {
    let container: WindowContainer = window.clone().into();
    resized_workspace_redraw = update_live_resize_with_finalize(
      &container,
      &frame,
      &active_drag.initial_position,
      true,
      state,
    )?;
    state.clear_window_drag_state(window.id());
  } else if is_resize {
    state.clear_window_drag_state(window.id());

    // Update the window's size from its actual frame dimensions.
    set_window_size(
      window.clone().into(),
      Some(LengthValue::from_px(frame.width())),
      Some(LengthValue::from_px(frame.height())),
      state,
    )?;
  } else {
    state.clear_window_drag_state(window.id());
  }

  window.set_active_drag(None);

  if resized_workspace_redraw {
    if let Some(workspace) = window.workspace() {
      state
        .pending_sync
        .queue_containers_to_redraw(workspace.tiling_children());
    }
  }

  // Redraw the window so it snaps back when:
  // - It is the only tiling window in the workspace.
  // - The drag stayed below the floating movement threshold.
  // - Resizing did not change the container's tiling size.
  state.pending_sync.queue_container_to_redraw(window.clone());

  Ok(())
}

#[cfg(test)]
mod tests {
  use tokio::sync::mpsc;
  use wm_common::{ActiveDrag, ActiveDragOperation};
  use wm_platform::{Dispatcher, Rect};

  use super::finish_tiling_window_drag;
  use crate::{
    commands::container::{attach_container, set_focused_descendant},
    models::{Monitor, TilingWindow, Workspace},
    traits::{TilingSizeGetters, WindowGetters},
    user_config::UserConfig,
    wm_state::WmState,
  };

  #[test]
  fn aborted_min_size_move_drag_preserves_tiling_size() {
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

    let win = TilingWindow::mock()
      .tiling_size(0.25)
      .gaps_config(config.value.gaps.clone())
      .call();
    let sibling = TilingWindow::mock()
      .tiling_size(0.75)
      .gaps_config(config.value.gaps.clone())
      .call();
    attach_container(&win.clone().into(), &workspace.clone().into(), None)
      .unwrap();
    attach_container(&sibling.into(), &workspace.into(), None).unwrap();

    // Simulate OS min-size expansion during a sub-threshold Move click.
    win.update_native_properties(|props| {
      props.frame = Rect::from_xy(0, 0, 800, 1032);
    });
    let active_drag = ActiveDrag {
      operation: Some(ActiveDragOperation::Move),
      is_from_floating: false,
      initial_position: Rect::from_xy(0, 0, 480, 1032),
      origin_void_id: None,
    };
    win.set_active_drag(Some(active_drag.clone()));

    finish_tiling_window_drag(&win, &active_drag, &mut state).unwrap();

    assert!((win.tiling_size() - 0.25).abs() < 0.001);
    assert!(win.active_drag().is_none());
  }
}
