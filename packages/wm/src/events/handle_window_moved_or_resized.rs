#[cfg(target_os = "windows")]
use std::time::Duration;

use anyhow::Context;
use wm_common::{
  try_warn, ActiveDrag, ActiveDragOperation, DisplayState,
  FloatingStateConfig, FullscreenStateConfig, HideMethod, TilingDirection,
  WindowState,
};
#[cfg(target_os = "macos")]
use wm_platform::{LengthValue, RectDelta};
#[cfg(target_os = "windows")]
use wm_platform::{NativeWindowWindowsExt, WindowHitTest};
use wm_platform::{MouseButton, NativeWindow, Point, Rect};

use crate::{
  commands::{
    container::{
      flatten_split_container, move_container_within_tree,
      wrap_in_split_container,
    },
    window::{
      absorb_void_into_sibling, finish_single_axis_drag,
      insert_void_preserving_siblings, normalize_workspace_voids,
      try_start_single_axis_drag, update_drag_snap_preview,
      update_window_state,
    },
  },
  events::handle_window_moved_or_resized_end,
  models::{
    DirectionContainer, Monitor, NonTilingWindow, SplitContainer,
    TilingContainer, TilingWindow, VoidSlot, WindowContainer,
  },
  traits::{
    CommonGetters, PositionGetters, TilingDirectionGetters,
    TilingSizeGetters, WindowGetters, MIN_TILING_SIZE,
  },
  user_config::UserConfig,
  wm_state::{LiveResizeState, WmState},
};

#[allow(clippy::too_many_lines)]
pub fn handle_window_moved_or_resized(
  native_window: &NativeWindow,
  // LINT: `is_interactive_start` is only used on Windows.
  #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
  is_interactive_start: bool,
  // LINT: `is_interactive_end` is only used on Windows.
  #[cfg_attr(not(target_os = "windows"), allow(unused_variables))]
  is_interactive_end: bool,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let found_window = state.window_from_native(native_window);

  if let Some(window) = found_window {
    let old_frame_position = window.native_properties().frame;
    let frame_position = try_warn!(window.native().frame());

    window.update_native_properties(|properties| {
      properties.frame = frame_position.clone();
    });

    // Handle windows that are actively being dragged.
    if !state.is_paused && window.active_drag().is_some() {
      let is_drag_end = {
        // On Windows, the drag operation has ended when
        // `is_interactive_end` is `true`. This corresponds to a
        // `EVENT_SYSTEM_MOVESIZEEND` event, which is unavailable on macOS.
        #[cfg(target_os = "windows")]
        {
          is_interactive_end
        }
        // On macOS, the drag operation has ended when the mouse button is
        // no longer down. This is a fallback mechanism since for macOS,
        // `is_interactive_end` is always `false`. The `MouseEvent` handler
        // also catches `MouseButtonUp` events, but this provides
        // additional safety.
        // TODO: Can probably remove this check and rely 100% on the mouse
        // event handler.
        #[cfg(target_os = "macos")]
        {
          !state.dispatcher.is_mouse_down(&MouseButton::Left)
        }
      };

      #[cfg(target_os = "windows")]
      {
        let is_maximized = try_warn!(window.native().is_maximized());
        window.update_native_properties(|properties| {
          properties.is_maximized = is_maximized;
          if is_maximized {
            properties.pending_maximize_until = None;
          }
        });

        if !is_maximized {
          let shadow_borders = try_warn!(window.native().shadow_borders());
          window.update_native_properties(|properties| {
            properties.shadow_borders = shadow_borders;
          });
        }
      }

      if state.is_single_axis_drag_window(window.id()) {
        if is_drag_end {
          if state.dispatcher.has_event_loop()
            && state.dispatcher.is_mouse_down(&MouseButton::Left)
          {
            #[cfg(target_os = "windows")]
            if let Some(drag) = &state.active_single_axis_drag {
              let outer_rect = drag.initial_outer_rect.clone();
              let native = window.native();
              if native.is_maximized().unwrap_or(false) {
                let _ = native.restore(Some(&outer_rect));
              }
              drop(native);
              window.update_native_properties(|props| {
                props.is_maximized = false;
              });
            }
            return Ok(());
          }

          return finish_single_axis_drag(None, state, config);
        }

        return Ok(());
      }

      if is_drag_end {
        return handle_window_moved_or_resized_end(&window, state, config);
      }

      return update_drag_state(
        &window,
        &frame_position,
        None,
        state,
        config,
      );
    }

    let old_is_maximized = window.native_properties().is_maximized;
    let is_maximized = try_warn!(window.native().is_maximized());

    // Ignore duplicate move/resize events. Window position changes can
    // trigger multiple events. For example, restoring from maximized can
    // trigger as many as 4 identical events on Windows.
    if old_frame_position == frame_position
      && old_is_maximized == is_maximized
      && !is_interactive_start
    {
      return Ok(());
    }

    window.update_native_properties(|properties| {
      properties.is_maximized = is_maximized;
      if is_maximized {
        properties.pending_maximize_until = None;
      }
    });

    // If the window is not maximized, update its cached shadow borders.
    // Maximized windows temporarily have 0 shadow borders, in which case
    // we should use its previous value for redraws.
    #[cfg(target_os = "windows")]
    {
      let shadow_borders = try_warn!(window.native().shadow_borders());
      if !is_maximized {
        window.update_native_properties(|properties| {
          properties.shadow_borders = shadow_borders;
        });
      }
    }

    let is_minimized = try_warn!(window.native().is_minimized());

    // Ignore events for minimized windows. Let them be handled by the
    // `PlatformEvent::WindowMinimized` event handler instead.
    if is_minimized {
      return Ok(());
    }

    // Detect whether the window is starting to be interactively moved or
    // resized by the user (e.g. via the window's drag handles).
    let is_drag_start = !state.is_paused && {
      #[cfg(target_os = "windows")]
      {
        // Drag events can be valid for all window states apart from
        // minimized.
        is_interactive_start
          && !matches!(window.state(), WindowState::Minimized)
      }
      #[cfg(target_os = "macos")]
      {
        // Drag events are never valid for minimized or maximized windows.
        let is_valid_state = !matches!(
          window.state(),
          WindowState::Fullscreen(FullscreenStateConfig {
            maximized: true,
            ..
          }) | WindowState::Minimized
        );

        let is_dragging_other_window =
          state.windows().iter().any(|w| w.active_drag().is_some());

        let is_left_click =
          state.dispatcher.is_mouse_down(&MouseButton::Left);

        // Only consider the window to be dragging if:
        //  1. The window is not minimized or maximized.
        //  2. No other window is being dragged.
        //  3. Left-click is down.
        //  4. The cursor is within 40px margin around the window's frame.
        if is_valid_state && !is_dragging_other_window && is_left_click {
          // The window frame can lag behind the cursor when moving or
          // resizing quickly, so allow for a bit of leeway.
          let frame_to_check = frame_position.apply_delta(
            &RectDelta::new(
              LengthValue::from_px(40),
              LengthValue::from_px(40),
              LengthValue::from_px(40),
              LengthValue::from_px(40),
            ),
            None,
          );

          // TODO: Might be more robust to also check if the window under
          // the cursor (i.e. via `dispatcher.window_from_point`) is not a
          // different window.
          let cursor_position = state.dispatcher.cursor_position()?;
          frame_to_check.contains_point(&cursor_position)
        } else {
          false
        }
      }
    };

    if is_drag_start {
      tracing::info!("Window started dragging: {window}");
      window.update_native_properties(|properties| {
        properties.pending_maximize_until = None;
      });

      let is_from_floating = matches!(
        window.state(),
        WindowState::Floating(_)
      ) || (matches!(window.state(), WindowState::Fullscreen(_))
        && !matches!(window.prev_state(), Some(WindowState::Tiling)));

      let is_maximized_origin = old_is_maximized
        || matches!(
          window.state(),
          WindowState::Fullscreen(FullscreenStateConfig {
            maximized: true,
            ..
          })
        );

      #[cfg(target_os = "windows")]
      let cached_hit = state.last_left_button_down.take().filter(|hit| {
        hit.window_id == window.id()
          && hit.timestamp.elapsed() < Duration::from_secs(2)
      });

      #[cfg(target_os = "windows")]
      let initial_operation = if is_maximized_origin {
        Some(ActiveDragOperation::Move)
      } else {
        cached_hit.as_ref().and_then(|hit| match hit.hit_test {
          WindowHitTest::Caption => Some(ActiveDragOperation::Move),
          WindowHitTest::ResizeBorder => {
            Some(ActiveDragOperation::Resize)
          }
          WindowHitTest::Client if !hit.is_near_resize_border => {
            Some(ActiveDragOperation::Move)
          }
          WindowHitTest::Client | WindowHitTest::Other => None,
        })
      };

      #[cfg(not(target_os = "windows"))]
      let initial_operation = if is_maximized_origin {
        Some(ActiveDragOperation::Move)
      } else {
        None
      };

      #[cfg(target_os = "windows")]
      let cached_cursor = cached_hit.map(|hit| hit.cursor);
      #[cfg(not(target_os = "windows"))]
      let cached_cursor: Option<Point> = None;

      let start_cursor = cached_cursor.or_else(|| {
        if state.dispatcher.has_event_loop()
          && state.dispatcher.is_mouse_down(&MouseButton::Left)
        {
          state.dispatcher.cursor_position().ok()
        } else {
          None
        }
      });

      state.active_drag_start_cursor =
        start_cursor.clone().map(|cursor| (window.id(), cursor));

      window.set_active_drag(Some(ActiveDrag {
        operation: initial_operation,
        is_from_floating,
        #[cfg(target_os = "windows")]
        initial_position: old_frame_position.clone(),
        // The updated frame position is used here instead of the initial
        // frame position due to a quirk on macOS. When we resize an
        // AXUIElement to a value outside the allowed min/max width &
        // height, macOS doesn't actually apply that size. However, it
        // still reports the value we attempted to set until a subsequent
        // `WindowEvent::MovedOrResized` event.
        #[cfg(target_os = "macos")]
        initial_position: frame_position.clone(),
        origin_void_id: None,
      }));

      if initial_operation == Some(ActiveDragOperation::Move) {
        if let WindowContainer::TilingWindow(tiling_win) = &window {
          if try_start_single_axis_drag(
            tiling_win,
            start_cursor.as_ref(),
            state,
          )? {
            return Ok(());
          }
        }
      }

      #[cfg(target_os = "windows")]
      update_drag_state(&window, &frame_position, None, state, config)?;

      return Ok(());
    }

    let nearest_monitor = state
      .nearest_monitor(&window.native())
      .context("No nearest monitor.")?;

    // For `HideMethod::PlaceInCorner`, hiding/showing is implemented by
    // repositioning the window. Since the OS won't emit real
    // shown/hidden events in this mode, update `DisplayState` based on
    // whether the window has been moved to the monitor's bottom corner.
    if config.value.general.hide_method == HideMethod::PlaceInCorner {
      let is_in_corner = is_in_corner(
        &frame_position,
        &nearest_monitor.native_properties().working_area,
      );

      // TODO: Consider redrawing if hidden and should be shown, or if
      // shown and should be hidden.
      // TODO: It can be valid for a floating window to be in the corner,
      // in which case, it currently doesn't get updated to
      // `DisplayState::Shown`.
      let display_state = match (window.display_state(), is_in_corner) {
        (DisplayState::Hiding, true) => DisplayState::Hidden,
        (DisplayState::Showing, false) => DisplayState::Shown,
        _ => window.display_state(),
      };

      if display_state != window.display_state() {
        window.set_display_state(display_state);
        return Ok(());
      }
    }

    let pending_until = window.native_properties().pending_maximize_until;
    let is_pending_max =
      pending_until.is_some_and(|until| std::time::Instant::now() < until);
    if !is_maximized
      && is_pending_max
      && matches!(
        window.state(),
        WindowState::Fullscreen(FullscreenStateConfig {
          maximized: true,
          ..
        })
      )
    {
      return Ok(());
    }

    let should_fullscreen = {
      let workspace = nearest_monitor
        .displayed_workspace()
        .context("No workspace.")?;

      let should_fullscreen = window.should_fullscreen(&workspace)?;

      match window.state() {
        // Override the fullscreen check for when an app self-exits
        // fullscreen (e.g. Chrome via F11) and restores its window to
        // a position that exactly covers the workspace rect.
        WindowState::Fullscreen(fullscreen)
          if !fullscreen.maximized && should_fullscreen =>
        {
          let workspace_rect = workspace.max_workspace_rect()?;

          let old_frame = old_frame_position
            .apply_delta(&window.border_delta().inverse(), None);
          let new_frame = frame_position
            .apply_delta(&window.border_delta().inverse(), None);

          let old_exceeded =
            old_frame.inset(1).contains_rect(&workspace_rect);
          let new_exceeds =
            new_frame.inset(1).contains_rect(&workspace_rect);

          // The window should no longer be fullscreen if the old frame
          // exceeded the workspace bounds (app was in OS fullscreen), but
          // the new frame no longer does. Configs with 0px outer gaps
          // always use the `should_fullscreen` check, since the old frame
          // will never exceed the workspace bounds.
          if old_exceeded && !new_exceeds {
            false
          } else {
            should_fullscreen
          }
        }
        _ => should_fullscreen,
      }
    };

    // Handle a window being maximized or entering fullscreen.
    if is_maximized || should_fullscreen {
      let is_same_state = is_maximized
        && matches!(
          window.state(),
          WindowState::Fullscreen(FullscreenStateConfig {
            maximized: true,
            ..
          })
        )
        || should_fullscreen
          && matches!(
            window.state(),
            WindowState::Fullscreen(FullscreenStateConfig {
              maximized: false,
              ..
            })
          );

      // Ignore if there's no state change.
      if is_same_state {
        return Ok(());
      }

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

      let window = update_window_state(
        window.clone(),
        WindowState::Fullscreen(FullscreenStateConfig {
          maximized: is_maximized,
          ..fullscreen_state
        }),
        state,
        config,
      )?;

      if is_maximized {
        // Dequeue the window from redraw if it's maximized, since the
        // window is already in the correct state.
        state
          .pending_sync
          .dequeue_container_from_redraw(window.clone());
      }

      // TODO: Handle a fullscreen window being moved from one monitor to
      // another.

      return Ok(());
    }

    match window.state() {
      WindowState::Fullscreen(_) => {
        // Window is no longer maximized/fullscreen and should be restored.
        tracing::info!("Restoring window from fullscreen: {window}");

        update_window_state(
          window.clone(),
          window.toggled_state(window.state(), config),
          state,
          config,
        )?;
      }
      WindowState::Floating(_) => {
        if let WindowContainer::NonTilingWindow(window) = window {
          update_floating_window_position(
            &window,
            frame_position,
            &nearest_monitor,
            state,
          )?;
        }
      }
      _ => {}
    }
  }

  Ok(())
}

// TODO: Move to shared location. `handle_window_moved_or_resized_end.rs`
// also uses this.
pub fn update_floating_window_position(
  window: &NonTilingWindow,
  frame_position: Rect,
  nearest_monitor: &Monitor,
  state: &mut WmState,
) -> anyhow::Result<()> {
  tracing::info!(
    "Updating floating window position: {}",
    window.as_window_container()?
  );

  // Update state with the new location of the floating window.
  window.set_floating_placement(frame_position);
  window.set_has_custom_floating_placement(true);

  let monitor = window.monitor().context("No monitor.")?;

  // Update the window's workspace if it goes out of bounds of its
  // current workspace.
  if monitor.id() != nearest_monitor.id() {
    let updated_workspace = nearest_monitor
      .displayed_workspace()
      .context("Failed to get workspace of nearest monitor.")?;

    tracing::info!(
      "Floating window moved to new workspace: {updated_workspace}",
    );

    window.set_insertion_target(None);

    move_container_within_tree(
      &window.clone().into(),
      &updated_workspace.clone().into(),
      updated_workspace.child_count(),
      state,
    )?;
  }

  Ok(())
}

/// Updates the window operation based on changes in frame position.
///
/// This function determines whether a window is being moved or resized and
/// updates its operation state accordingly. If the window is being moved,
/// it's set to floating mode.
fn frame_displacement_distance(frame: &Rect, initial: &Rect) -> f32 {
  if frame.width() == initial.width() && frame.height() == initial.height()
  {
    frame.center_point().distance_between(&initial.center_point())
  } else {
    Point { x: frame.left, y: frame.top }.distance_between(&Point {
      x: initial.left,
      y: initial.top,
    })
  }
}

#[allow(clippy::too_many_lines)]
fn update_drag_state(
  window: &WindowContainer,
  frame_position: &Rect,
  cursor_override: Option<&Point>,
  state: &mut WmState,
  config: &UserConfig,
) -> anyhow::Result<()> {
  let Some(active_drag) = window.active_drag() else {
    return Ok(());
  };

  // Ignore if the window position has not changed yet.
  if *frame_position == active_drag.initial_position {
    return Ok(());
  }

  // Determine the drag operation if not already set.
  let is_move = if let Some(operation) = active_drag.operation {
    matches!(operation, ActiveDragOperation::Move)
  } else {
    let initial = &active_drag.initial_position;
    let same_size = frame_position.height() == initial.height()
      && frame_position.width() == initial.width();
    let translated_x = frame_position.left != initial.left
      && frame_position.right != initial.right
      && (frame_position.left - initial.left).signum()
        == (frame_position.right - initial.right).signum()
      && frame_position.height() == initial.height();
    let translated_y = frame_position.top != initial.top
      && frame_position.bottom != initial.bottom
      && (frame_position.top - initial.top).signum()
        == (frame_position.bottom - initial.bottom).signum()
      && frame_position.width() == initial.width();

    let is_move = (*frame_position != *initial && same_size)
      || translated_x
      || translated_y;

    let operation = if is_move {
      ActiveDragOperation::Move
    } else {
      ActiveDragOperation::Resize
    };

    window.set_active_drag(Some(ActiveDrag {
      operation: Some(operation),
      ..active_drag.clone()
    }));

    if is_move && state.dispatcher.has_event_loop() {
      if let WindowContainer::TilingWindow(tiling_win) = window {
        let start_cursor = state
          .active_drag_start_cursor
          .as_ref()
          .filter(|(id, _)| *id == window.id())
          .map(|(_, cursor)| cursor.clone());
        if try_start_single_axis_drag(
          tiling_win,
          start_cursor.as_ref(),
          state,
        )? {
          return Ok(());
        }
      }
    }

    is_move
  };

  // Transition window to be floating while it's being dragged, but only
  // after it has been moved at least 10px from its initial position. The
  // 10px threshold is to account for small movements that may be
  // accidental.
  let active_window = if is_move
    && !matches!(window.state(), WindowState::Floating(_))
  {
    let move_distance = match &state.active_drag_start_cursor {
      Some((drag_window_id, start_cursor))
        if *drag_window_id == window.id() =>
      {
        if let Some(cursor) = cursor_override {
          cursor.distance_between(start_cursor)
        } else if state.dispatcher.has_event_loop() {
          state.dispatcher.cursor_position().map_or_else(
            |_| {
              frame_displacement_distance(
                frame_position,
                &active_drag.initial_position,
              )
            },
            |cursor| cursor.distance_between(start_cursor),
          )
        } else {
          frame_displacement_distance(
            frame_position,
            &active_drag.initial_position,
          )
        }
      }
      _ => frame_displacement_distance(
        frame_position,
        &active_drag.initial_position,
      ),
    };

    // Dragging operations on a maximized window can only occur on Windows.
    // The OS immediately restores it while it's being dragged, so we need
    // to update state accordingly without a redraw.
    let is_maximized = matches!(
      window.state(),
      WindowState::Fullscreen(FullscreenStateConfig {
        maximized: true,
        ..
      })
    );

    if move_distance >= 10.0 || is_maximized {
      let parent = window.parent().context("No parent")?;

      let is_fullscreen =
        matches!(window.state(), WindowState::Fullscreen(_))
          && !is_maximized;

      let updated = update_window_state(
        window.clone(),
        WindowState::Floating(FloatingStateConfig {
          centered: false,
          ..config.value.window_behavior.state_defaults.floating
        }),
        state,
        config,
      )?;

      // `update_window_state` automatically adds the window for redraw,
      // which we don't want in this case. However, for fullscreen windows,
      // we do actually want it to be resized initially so that it's
      // easier to move around while dragging.
      if !is_fullscreen {
        state
          .pending_sync
          .dequeue_container_from_redraw(updated.clone());
      }

      // Flatten the parent split container if it only contains the window.
      if let Some(split_parent) = parent.as_split() {
        if !split_parent.is_detached() && split_parent.child_count() == 1 {
          flatten_split_container(split_parent.clone())?;

          state
            .pending_sync
            .queue_containers_to_redraw(updated.tiling_siblings());
        }
      }

      updated
    } else {
      window.clone()
    }
  } else {
    window.clone()
  };

  if is_move && matches!(active_window.state(), WindowState::Floating(_)) {
    update_drag_snap_preview(&active_window, cursor_override, state)?;
  } else if !is_move {
    update_live_resize(
      &active_window,
      frame_position,
      &active_drag.initial_position,
      state,
    )?;
  }

  Ok(())
}

const MIN_VOID_REMAINING_PX: i32 = 4;
const VOID_PUSH_THROUGH_PX: i32 = -4;
const VOID_SNAP_RELEASE_PX: i32 = 20;
const OUTER_VOID_CREATE_MIN_PX: i32 = 16;

/// Applies real-time proportional resize updates to adjacent tiling
/// containers and queues neighboring windows for immediate redraw while
/// leaving the actively dragged window untouched.
pub fn update_live_resize(
  window: &WindowContainer,
  frame_position: &Rect,
  initial_position: &Rect,
  state: &mut WmState,
) -> anyhow::Result<()> {
  update_live_resize_with_finalize(
    window,
    frame_position,
    initial_position,
    false,
    state,
  )?;
  Ok(())
}

/// Applies live resize updates across neighboring containers, adjacent
/// `VoidSlot` absorption, and outer-edge `VoidSlot` creation. When
/// `is_final` is `true`, voids within snap tolerance absorb flush to the
/// screen edge or neighbor.
#[allow(clippy::too_many_lines)]
pub fn update_live_resize_with_finalize(
  window: &WindowContainer,
  frame_position: &Rect,
  initial_position: &Rect,
  is_final: bool,
  state: &mut WmState,
) -> anyhow::Result<bool> {
  let Some(tiling_window) = window.as_tiling_window() else {
    return Ok(false);
  };

  let live_state = match &mut state.live_resize_state {
    Some(existing) if existing.window_id == tiling_window.id() => {
      if !existing.dragged_left && !existing.dragged_right {
        existing.dragged_left =
          frame_position.left != initial_position.left;
        existing.dragged_right =
          frame_position.right != initial_position.right;
      }
      if !existing.dragged_top && !existing.dragged_bottom {
        existing.dragged_top = frame_position.top != initial_position.top;
        existing.dragged_bottom =
          frame_position.bottom != initial_position.bottom;
      }
      existing
    }
    slot => {
      *slot = Some(LiveResizeState {
        window_id: tiling_window.id(),
        dragged_left: frame_position.left != initial_position.left,
        dragged_right: frame_position.right != initial_position.right,
        dragged_top: frame_position.top != initial_position.top,
        dragged_bottom: frame_position.bottom != initial_position.bottom,
      });
      slot.as_mut().context("Missing live resize state.")?
    }
  };

  let edges = [
    (true, true, live_state.dragged_left && !live_state.dragged_right),
    (true, false, live_state.dragged_right && !live_state.dragged_left),
    (false, true, live_state.dragged_top && !live_state.dragged_bottom),
    (false, false, live_state.dragged_bottom && !live_state.dragged_top),
  ];

  let visual_frame =
    frame_position.apply_delta(&window.border_delta().inverse(), None);
  let mut mutated = false;

  for (is_horizontal, is_prev_edge, is_active) in edges {
    if !is_active {
      continue;
    }

    if let Some((container, neighbor, parent)) = tiling_window
      .resizable_container_and_neighbor(is_horizontal, is_prev_edge)?
    {
      if let Some(void_neighbor) = neighbor.as_void().cloned() {
        let (changed, pushed_through) = resize_into_void_neighbor(
          tiling_window,
          &container,
          &void_neighbor,
          &parent,
          &visual_frame,
          is_horizontal,
          is_prev_edge,
          is_final,
          state,
        )?;
        mutated |= changed;

        if pushed_through {
          if let Some((next_c, next_n, next_p)) = tiling_window
            .resizable_container_and_neighbor(is_horizontal, is_prev_edge)?
          {
            if !next_n.is_void() {
              mutated |= resize_real_neighbor(
                tiling_window,
                &next_c,
                &next_n,
                &next_p,
                &visual_frame,
                is_horizontal,
                is_prev_edge,
                state,
              )?;
            }
          }
        }
      } else {
        mutated |= resize_real_neighbor(
          tiling_window,
          &container,
          &neighbor,
          &parent,
          &visual_frame,
          is_horizontal,
          is_prev_edge,
          state,
        )?;
      }
    } else {
      mutated |= try_create_outer_edge_void(
        tiling_window,
        &visual_frame,
        is_horizontal,
        is_prev_edge,
        is_final,
        state,
      )?;
    }
  }

  if !is_final {
    state.pending_sync.dequeue_container_from_redraw(window.clone());
  }

  Ok(mutated)
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn resize_into_void_neighbor(
  tiling_window: &TilingWindow,
  container: &TilingContainer,
  void_neighbor: &VoidSlot,
  parent: &DirectionContainer,
  visual_frame: &Rect,
  is_horizontal: bool,
  is_prev_edge: bool,
  is_final: bool,
  state: &mut WmState,
) -> anyhow::Result<(bool, bool)> {
  let curr_rect = container.to_rect()?;
  let void_rect = void_neighbor.to_rect()?;
  let parent_rect = parent.to_rect()?;
  let (h_gap, v_gap) = container.inner_gaps()?;
  let gap = if is_horizontal { h_gap } else { v_gap };
  let parent_len = if is_horizontal {
    parent_rect.width()
  } else {
    parent_rect.height()
  };

  #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
  let sibling_count = container.tiling_siblings().count() as i32;
  let available_px = (parent_len - gap * sibling_count).max(1);

  let dragged_coord =
    resize_edge_coord(visual_frame, is_horizontal, is_prev_edge);
  let far_edge =
    resize_edge_coord(&void_rect, is_horizontal, is_prev_edge);
  let remaining_px = if is_prev_edge {
    dragged_coord - far_edge
  } else {
    far_edge - dragged_coord
  };

  let pushed_through = remaining_px <= VOID_PUSH_THROUGH_PX;
  let should_absorb =
    pushed_through || (is_final && remaining_px <= VOID_SNAP_RELEASE_PX);

  if should_absorb {
    absorb_void_into_sibling(
      void_neighbor,
      container,
      parent,
      gap,
      parent_len,
    )?;
    if let Some(workspace) = tiling_window.workspace() {
      normalize_workspace_voids(&workspace)?;
      let dragged_id = tiling_window.id();
      let others = workspace
        .tiling_windows()
        .filter(|w| w.id() != dragged_id);
      state.pending_sync.queue_containers_to_redraw(others);
    }
    return Ok((true, pushed_through));
  }

  let delta_px = resize_edge_delta_px(
    &curr_rect,
    visual_frame,
    is_horizontal,
    is_prev_edge,
  );
  if delta_px == 0 {
    return Ok((false, false));
  }

  #[allow(clippy::cast_precision_loss)]
  let available_f = available_px as f32;
  #[allow(clippy::cast_precision_loss)]
  let delta_size = delta_px as f32 / available_f;
  let total_pair = container.tiling_size() + void_neighbor.tiling_size();
  let min_container = (36.0 / available_f).max(MIN_TILING_SIZE);
  #[allow(clippy::cast_precision_loss)]
  let min_void = (MIN_VOID_REMAINING_PX as f32 / available_f).max(0.001);

  if total_pair <= min_container + min_void {
    return Ok((false, false));
  }

  let max_container = total_pair - min_void;
  let new_size =
    (container.tiling_size() + delta_size)
      .clamp(min_container, max_container);
  let new_void_size = total_pair - new_size;

  if (new_size - container.tiling_size()).abs() > 0.0005 {
    container.set_tiling_size(new_size);
    void_neighbor.set_tiling_size(new_void_size);

    let dragged_id = tiling_window.id();
    let siblings = parent
      .descendants()
      .filter(|c| c.is_tiling_window() && c.id() != dragged_id);
    state.pending_sync.queue_containers_to_redraw(siblings);
    return Ok((true, false));
  }

  Ok((false, false))
}

#[allow(clippy::too_many_arguments)]
fn resize_real_neighbor(
  tiling_window: &TilingWindow,
  container: &TilingContainer,
  neighbor: &TilingContainer,
  parent: &DirectionContainer,
  visual_frame: &Rect,
  is_horizontal: bool,
  is_prev_edge: bool,
  state: &mut WmState,
) -> anyhow::Result<bool> {
  let curr_rect = container.to_rect()?;
  let delta_px = resize_edge_delta_px(
    &curr_rect,
    visual_frame,
    is_horizontal,
    is_prev_edge,
  );
  if delta_px == 0 {
    return Ok(false);
  }

  let (horizontal_gap, vertical_gap) = container.inner_gaps()?;
  let parent_rect = parent.to_rect()?;
  #[allow(clippy::cast_possible_wrap, clippy::cast_possible_truncation)]
  let sibling_count = container.tiling_siblings().count() as i32;
  let available_px = if is_horizontal {
    parent_rect.width() - horizontal_gap * sibling_count
  } else {
    parent_rect.height() - vertical_gap * sibling_count
  };

  if available_px <= 0 {
    return Ok(false);
  }

  #[allow(clippy::cast_precision_loss)]
  let delta_size = delta_px as f32 / available_px as f32;
  let total_pair = container.tiling_size() + neighbor.tiling_size();
  if total_pair <= MIN_TILING_SIZE * 2.0 {
    return Ok(false);
  }

  let new_size = (container.tiling_size() + delta_size)
    .clamp(MIN_TILING_SIZE, total_pair - MIN_TILING_SIZE);
  let new_neighbor_size = total_pair - new_size;

  if (new_size - container.tiling_size()).abs() > 0.0005 {
    container.set_tiling_size(new_size);
    neighbor.set_tiling_size(new_neighbor_size);

    let dragged_id = tiling_window.id();
    let siblings = parent
      .descendants()
      .filter(|c| c.is_tiling_window() && c.id() != dragged_id);
    state.pending_sync.queue_containers_to_redraw(siblings);
    return Ok(true);
  }

  Ok(false)
}

#[allow(clippy::too_many_lines)]
fn try_create_outer_edge_void(
  tiling_window: &TilingWindow,
  visual_frame: &Rect,
  is_horizontal: bool,
  is_prev_edge: bool,
  is_final: bool,
  state: &mut WmState,
) -> anyhow::Result<bool> {
  let Some(workspace) = tiling_window.workspace() else {
    return Ok(false);
  };

  let ws_rect = workspace.multi_window_workspace_rect()?;
  let (h_gap, v_gap) = workspace.workspace_inner_gaps()?;
  let gap = if is_horizontal { h_gap } else { v_gap };

  let curr_rect = tiling_window.to_rect()?;
  let curr_edge =
    resize_edge_coord(&curr_rect, is_horizontal, is_prev_edge);
  let ws_edge = resize_edge_coord(&ws_rect, is_horizontal, is_prev_edge);

  if (curr_edge - ws_edge).abs() > 16 {
    return Ok(false);
  }

  let dragged_coord =
    resize_edge_coord(visual_frame, is_horizontal, is_prev_edge);
  let inward_px = if is_prev_edge {
    dragged_coord - ws_edge
  } else {
    ws_edge - dragged_coord
  };

  let min_create = (gap + 12).max(OUTER_VOID_CREATE_MIN_PX);
  if inward_px < min_create {
    return Ok(is_final);
  }

  let target_direction = if is_horizontal {
    TilingDirection::Horizontal
  } else {
    TilingDirection::Vertical
  };

  let parent = tiling_window
    .direction_container()
    .context("Tiling window has no direction parent.")?;
  let gaps_config = workspace.gaps_config();

  if parent.tiling_direction() != target_direction {
    if parent.as_workspace().is_some()
      && parent.tiling_children().count() == 1
    {
      workspace.set_tiling_direction(target_direction);
    } else {
      let split =
        SplitContainer::new(target_direction, gaps_config.clone());
      wrap_in_split_container(
        &split,
        &parent.clone().into(),
        &[tiling_window.clone().into()],
      )?;
    }
  }

  let new_parent = tiling_window
    .direction_container()
    .context("Tiling window has no direction parent after wrap.")?;
  let parent_axis_px = if new_parent.as_workspace().is_some() {
    if is_horizontal {
      ws_rect.width()
    } else {
      ws_rect.height()
    }
  } else {
    let parent_rect = new_parent.to_rect()?;
    if is_horizontal {
      parent_rect.width()
    } else {
      parent_rect.height()
    }
  };

  let void_px = (inward_px - gap).max(4);
  let inserted = insert_void_preserving_siblings(
    &new_parent,
    &tiling_window.clone().into(),
    is_prev_edge,
    void_px,
    gap,
    parent_axis_px,
    gaps_config,
  )?;

  if inserted.is_some() {
    let dragged_id = tiling_window.id();
    let others = workspace
      .tiling_windows()
      .filter(|win| win.id() != dragged_id);
    state.pending_sync.queue_containers_to_redraw(others);
  }

  Ok(inserted.is_some())
}

const fn resize_edge_coord(
  rect: &Rect,
  is_horizontal: bool,
  is_prev_edge: bool,
) -> i32 {
  match (is_horizontal, is_prev_edge) {
    (true, true) => rect.left,
    (true, false) => rect.right,
    (false, true) => rect.top,
    (false, false) => rect.bottom,
  }
}

const fn resize_edge_delta_px(
  curr_rect: &Rect,
  visual_frame: &Rect,
  is_horizontal: bool,
  is_prev_edge: bool,
) -> i32 {
  match (is_horizontal, is_prev_edge) {
    (true, true) => curr_rect.left - visual_frame.left,
    (true, false) => visual_frame.right - curr_rect.right,
    (false, true) => curr_rect.top - visual_frame.top,
    (false, false) => visual_frame.bottom - curr_rect.bottom,
  }
}

/// Gets whether the window is in the corner of the monitor.
fn is_in_corner(window_frame: &Rect, monitor_rect: &Rect) -> bool {
  // Visible portion of the window used when positioning windows in the
  // monitor's corner. See `platform_sync` for how hidden windows are
  // positioned.
  const VISIBLE_SLIVER_PX: i32 = 1;

  // Allow 1px of leeway.
  let is_left_corner =
    (window_frame.right - VISIBLE_SLIVER_PX - monitor_rect.left).abs()
      <= 1;

  // Allow 1px of leeway.
  let is_right_corner =
    (window_frame.x() + VISIBLE_SLIVER_PX - monitor_rect.right).abs() <= 1;

  // On macOS, the window's title bar is prevented from being positioned
  // outside of monitor's working area, so we need to allow ~55px of
  // vertical leeway. Title bar height varies, but can be up to 52px.
  // TODO: See if possible to make this dynamic based on the window's title
  // bar height.
  let is_bottom_of_monitor =
    (window_frame.y() - monitor_rect.bottom).abs() <= 55;

  (is_left_corner || is_right_corner) && is_bottom_of_monitor
}

#[cfg(test)]
mod tests {
  use wm_platform::Rect;

  use super::is_in_corner;

  #[test]
  fn matches_corner_positions() {
    let monitor = Rect::from_xy(0, 0, 1920, 1080);

    let frame_in_right_corner = Rect::from_xy(1919, 1050, 600, 600);
    assert!(is_in_corner(&frame_in_right_corner, &monitor));

    let frame_in_left_corner = Rect::from_xy(-599, 1050, 600, 600);
    assert!(is_in_corner(&frame_in_left_corner, &monitor));
  }

  #[test]
  fn does_not_match_non_corner_positions() {
    let monitor = Rect::from_xy(0, 0, 1920, 1080);
    let frame = Rect::from_xy(100, 100, 800, 600);

    assert!(!is_in_corner(&frame, &monitor));
  }

  #[test]
  fn min_size_constrained_tiling_drag_transitions_to_floating() {
    use tokio::sync::mpsc;
    use wm_common::{ActiveDrag, ActiveDragOperation, WindowState};
    use wm_platform::{Dispatcher, Point};

    use super::update_drag_state;
    use crate::{
      commands::container::{attach_container, set_focused_descendant},
      models::{Monitor, TilingWindow, Workspace},
      traits::{TilingSizeGetters, WindowGetters},
      user_config::UserConfig,
      wm_state::WmState,
    };

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
    attach_container(&sibling.into(), &workspace.clone().into(), None)
      .unwrap();

    // Tiled slot is 480px wide, below a hypothetical 800px minimum width.
    let tile_rect = Rect::from_xy(0, 0, 480, 1032);
    win.set_active_drag(Some(ActiveDrag {
      operation: Some(ActiveDragOperation::Move),
      is_from_floating: false,
      initial_position: tile_rect,
      origin_void_id: None,
    }));
    state.active_drag_start_cursor =
      Some((win.id(), Point { x: 240, y: 20 }));

    // OS enforces minimum width (480 -> 800) before 10px cursor travel.
    let expanded_rect = Rect::from_xy(0, 0, 800, 1032);
    let small_move_cursor = Point { x: 245, y: 22 };
    update_drag_state(
      &win.clone().into(),
      &expanded_rect,
      Some(&small_move_cursor),
      &mut state,
      &config,
    )
    .unwrap();

    // Remain Tiling below 10px cursor movement without false resize.
    assert!(matches!(win.state(), WindowState::Tiling));
    assert!(state.live_resize_state.is_none());

    // Once cursor travels >= 10px, window transitions to Floating cleanly.
    let drag_out_cursor = Point { x: 270, y: 35 };
    let moved_rect = Rect::from_xy(30, 15, 800, 1032);
    update_drag_state(
      &win.clone().into(),
      &moved_rect,
      Some(&drag_out_cursor),
      &mut state,
      &config,
    )
    .unwrap();

    let updated = state.window_by_id(win.id()).unwrap();
    assert!(matches!(updated.state(), WindowState::Floating(_)));
    assert!(state.live_resize_state.is_none());
  }

  #[test]
  fn maximized_origin_move_drag_ignores_restored_size_shrink() {
    use tokio::sync::mpsc;
    use wm_common::{
      ActiveDrag, ActiveDragOperation, FullscreenStateConfig, WindowState,
    };
    use wm_platform::{Dispatcher, Point};

    use super::update_drag_state;
    use crate::{
      commands::container::{attach_container, set_focused_descendant},
      models::{Monitor, NonTilingWindow, Workspace},
      traits::WindowGetters,
      user_config::UserConfig,
      wm_state::WmState,
    };

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

    let win = NonTilingWindow::mock()
      .state(WindowState::Fullscreen(FullscreenStateConfig {
        maximized: true,
        ..Default::default()
      }))
      .call();
    attach_container(&win.clone().into(), &workspace.into(), None)
      .unwrap();

    win.set_active_drag(Some(ActiveDrag {
      operation: Some(ActiveDragOperation::Move),
      is_from_floating: true,
      initial_position: Rect::from_xy(0, 0, 1920, 1032),
      origin_void_id: None,
    }));
    state.active_drag_start_cursor =
      Some((win.id(), Point { x: 960, y: 15 }));

    let restored_rect = Rect::from_xy(360, 4, 1200, 800);
    let drag_cursor = Point { x: 960, y: 19 };
    update_drag_state(
      &win.clone().into(),
      &restored_rect,
      Some(&drag_cursor),
      &mut state,
      &config,
    )
    .unwrap();

    let updated = state.window_by_id(win.id()).unwrap();
    assert!(matches!(updated.state(), WindowState::Floating(_)));
    assert_eq!(
      updated.active_drag().and_then(|d| d.operation),
      Some(ActiveDragOperation::Move)
    );
  }

  #[test]
  fn live_resize_absorbs_adjacent_void_on_release_and_push_through() {
    use tokio::sync::mpsc;
    use wm_platform::Dispatcher;

    use super::{update_live_resize, update_live_resize_with_finalize};
    use crate::{
      commands::container::{attach_container, set_focused_descendant},
      models::{Container, Monitor, TilingWindow, VoidSlot, Workspace},
      traits::{CommonGetters, PositionGetters, TilingSizeGetters},
      user_config::UserConfig,
      wm_state::WmState,
    };

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

    let win_a = TilingWindow::mock()
      .gaps_config(config.value.gaps.clone())
      .call();
    let void_mid = VoidSlot::new(config.value.gaps.clone());
    let win_b = TilingWindow::mock()
      .gaps_config(config.value.gaps.clone())
      .call();
    let ws_c: Container = workspace.clone().into();
    attach_container(&win_a.clone().into(), &ws_c, None).unwrap();
    attach_container(&void_mid.clone().into(), &ws_c, None).unwrap();
    attach_container(&win_b.clone().into(), &ws_c, None).unwrap();
    win_a.set_tiling_size(0.3);
    void_mid.set_tiling_size(0.4);
    win_b.set_tiling_size(0.3);

    let initial = win_a.to_rect().unwrap();
    let b_initial = win_b.to_rect().unwrap();

    // Drag right edge across most of the void without touching B yet.
    let mid_frame = Rect::from_ltrb(
      initial.left,
      initial.top,
      b_initial.left - 30,
      initial.bottom,
    );
    update_live_resize(
      &win_a.clone().into(),
      &mid_frame,
      &initial,
      &mut state,
    )
    .unwrap();
    assert_eq!(workspace.tiling_children().count(), 3);
    assert_eq!(win_b.to_rect().unwrap(), b_initial);

    // Push past the void into B: absorbs void and starts shrinking B.
    let pushed_frame = Rect::from_ltrb(
      initial.left,
      initial.top,
      b_initial.left + 120,
      initial.bottom,
    );
    update_live_resize_with_finalize(
      &win_a.clone().into(),
      &pushed_frame,
      &initial,
      false,
      &mut state,
    )
    .unwrap();

    assert_eq!(workspace.tiling_children().count(), 2);
    assert!(win_b.to_rect().unwrap().width() < b_initial.width());
  }

  #[test]
  fn live_resize_outer_edge_creates_void_and_snaps_back() {
    use tokio::sync::mpsc;
    use wm_platform::Dispatcher;

    use super::update_live_resize_with_finalize;
    use crate::{
      commands::container::{attach_container, set_focused_descendant},
      models::{Container, Monitor, TilingWindow, Workspace},
      traits::{CommonGetters, PositionGetters, TilingSizeGetters},
      user_config::UserConfig,
      wm_state::WmState,
    };

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

    let win_a = TilingWindow::mock()
      .tiling_size(0.5)
      .gaps_config(config.value.gaps.clone())
      .call();
    let win_b = TilingWindow::mock()
      .tiling_size(0.5)
      .gaps_config(config.value.gaps.clone())
      .call();
    let ws_c: Container = workspace.clone().into();
    attach_container(&win_a.clone().into(), &ws_c, None).unwrap();
    attach_container(&win_b.clone().into(), &ws_c, None).unwrap();

    let initial = win_a.to_rect().unwrap();
    let b_initial = win_b.to_rect().unwrap();

    // Drag A's outer left edge inward -> creates VoidSlot on left edge
    // while leaving B untouched.
    let inward_frame = Rect::from_ltrb(
      initial.left + 200,
      initial.top,
      initial.right,
      initial.bottom,
    );
    update_live_resize_with_finalize(
      &win_a.clone().into(),
      &inward_frame,
      &initial,
      false,
      &mut state,
    )
    .unwrap();

    let children: Vec<_> = workspace.tiling_children().collect();
    let sum_mid: f32 = children.iter().map(|c| c.tiling_size()).sum();
    assert_eq!(children.len(), 3);
    assert!(children[0].is_void());
    assert_eq!(children[1].id(), win_a.id());
    assert_eq!(win_b.to_rect().unwrap(), b_initial);
    assert!((sum_mid - 1.0).abs() < 0.001);

    // Drag back close to the left screen edge and release -> void absorbs.
    let back_frame = Rect::from_ltrb(
      initial.left + 10,
      initial.top,
      initial.right,
      initial.bottom,
    );
    update_live_resize_with_finalize(
      &win_a.clone().into(),
      &back_frame,
      &initial,
      true,
      &mut state,
    )
    .unwrap();

    let sum_end: f32 =
      workspace.tiling_children().map(|c| c.tiling_size()).sum();
    assert_eq!(workspace.tiling_children().count(), 2);
    assert!((sum_end - 1.0).abs() < 0.001);
    assert!(win_a.to_rect().unwrap().left <= initial.left + 2);
    assert_eq!(win_b.to_rect().unwrap(), b_initial);
  }
}
