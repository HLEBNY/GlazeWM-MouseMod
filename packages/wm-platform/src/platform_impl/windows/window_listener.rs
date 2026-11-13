use std::sync::{Mutex, OnceLock};

use tokio::sync::mpsc;
use windows::Win32::{
  Foundation::HWND,
  UI::{
    Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK},
    WindowsAndMessaging::{
      GetAncestor, EVENT_OBJECT_CLOAKED, EVENT_OBJECT_DESTROY,
      EVENT_OBJECT_HIDE, EVENT_OBJECT_LOCATIONCHANGE,
      EVENT_OBJECT_NAMECHANGE, EVENT_OBJECT_SHOW, EVENT_OBJECT_UNCLOAKED,
      EVENT_SYSTEM_CAPTUREEND, EVENT_SYSTEM_CAPTURESTART,
      EVENT_SYSTEM_DRAGDROPSTART, EVENT_SYSTEM_FOREGROUND,
      EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART,
      EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MOVESIZESTART, GA_ROOT,
      OBJID_CLIENT, OBJID_WINDOW, WINEVENT_OUTOFCONTEXT,
      WINEVENT_SKIPOWNPROCESS,
    },
  },
};

use super::NativeWindow;
use crate::{Dispatcher, WindowEvent, WindowId};

const CAPTURE_RING_LEN: usize = 16;

struct CaptureHistory {
  seq: u64,
  active_capture_root: isize,
  capture_starts: [(u64, isize); CAPTURE_RING_LEN],
  capture_start_idx: usize,
  capture_ends: [(u64, isize); CAPTURE_RING_LEN],
  capture_end_idx: usize,
  dragdrop_starts: [(u64, isize); CAPTURE_RING_LEN],
  dragdrop_idx: usize,
}

impl CaptureHistory {
  const fn new() -> Self {
    Self {
      seq: 0,
      active_capture_root: 0,
      capture_starts: [(0, 0); CAPTURE_RING_LEN],
      capture_start_idx: 0,
      capture_ends: [(0, 0); CAPTURE_RING_LEN],
      capture_end_idx: 0,
      dragdrop_starts: [(0, 0); CAPTURE_RING_LEN],
      dragdrop_idx: 0,
    }
  }
  fn next_seq(&mut self) -> u64 {
    self.seq = self.seq.wrapping_add(1);
    self.seq
  }

  fn record_capture_start(&mut self, root_id: isize) {
    let seq = self.next_seq();
    self.active_capture_root = root_id;
    self.capture_starts[self.capture_start_idx] = (seq, root_id);
    self.capture_start_idx =
      (self.capture_start_idx + 1) % CAPTURE_RING_LEN;
  }

  fn record_capture_end(&mut self, root_id: isize) {
    let seq = self.next_seq();
    if self.active_capture_root == root_id {
      self.active_capture_root = 0;
    }
    self.capture_ends[self.capture_end_idx] = (seq, root_id);
    self.capture_end_idx = (self.capture_end_idx + 1) % CAPTURE_RING_LEN;
  }

  fn record_dragdrop_start(&mut self, root_id: isize) {
    let seq = self.next_seq();
    self.dragdrop_starts[self.dragdrop_idx] = (seq, root_id);
    self.dragdrop_idx = (self.dragdrop_idx + 1) % CAPTURE_RING_LEN;
  }
}

static CAPTURE_HISTORY: Mutex<CaptureHistory> =
  Mutex::new(CaptureHistory::new());

/// Snapshots the monotonic capture/drag-drop sequence counter and whether
/// `window_handle` currently owns mouse capture.
pub(crate) fn capture_snapshot(window_handle: isize) -> (u64, bool) {
  let history = CAPTURE_HISTORY
    .lock()
    .unwrap_or_else(std::sync::PoisonError::into_inner);
  (history.seq, history.active_capture_root == window_handle)
}

/// Returns `(had_capture_end_cycle, had_dragdrop_start)` for
/// `window_handle` since `since_seq`.
pub(crate) fn capture_cycle_since(
  window_handle: isize,
  since_seq: u64,
  started_captured: bool,
) -> (bool, bool) {
  let history = CAPTURE_HISTORY
    .lock()
    .unwrap_or_else(std::sync::PoisonError::into_inner);

  let had_start = started_captured
    || history
      .capture_starts
      .iter()
      .any(|&(seq, root)| root == window_handle && seq > since_seq);
  let had_end = history
    .capture_ends
    .iter()
    .any(|&(seq, root)| root == window_handle && seq > since_seq);
  let had_dragdrop = history
    .dragdrop_starts
    .iter()
    .any(|&(seq, root)| root == window_handle && seq > since_seq);

  (had_start && had_end, had_dragdrop)
}

thread_local! {
  /// Sender for window events. For use with hook procedure.
  static EVENT_TX: OnceLock<mpsc::UnboundedSender<WindowEvent>> = const { OnceLock::new() };
}

/// Platform-specific implementation of [`WindowEventNotification`].
#[derive(Clone, Debug)]
pub struct WindowEventNotificationInner;

/// Platform-specific implementation of [`WindowListener`].
#[derive(Debug)]
pub(crate) struct WindowListener {
  hook_handles: Vec<HWINEVENTHOOK>,
}

impl WindowListener {
  /// Implements [`WindowListener::new`].
  pub(crate) fn new(
    event_tx: mpsc::UnboundedSender<WindowEvent>,
    dispatcher: &Dispatcher,
  ) -> crate::Result<Self> {
    let hook_handles = dispatcher.dispatch_sync(move || {
      EVENT_TX.with(|lock| lock.set(event_tx)).map_err(|_| {
        crate::Error::Platform(
          "Window event sender already set.".to_string(),
        )
      })?;

      Self::hook_win_events()
    })??;

    Ok(Self { hook_handles })
  }

  /// Implements [`WindowListener::terminate`].
  pub(crate) fn terminate(&mut self) {
    for handle in self.hook_handles.drain(..) {
      let _ = unsafe { UnhookWinEvent(handle) };
    }
  }

  /// Creates several window event hooks via `SetWinEventHook`.
  ///
  /// Separate hooks are created per event range, which is more performant
  /// than a single hook covering all events.
  fn hook_win_events() -> crate::Result<Vec<HWINEVENTHOOK>> {
    let event_ranges = [
      (EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE),
      (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND),
      (EVENT_SYSTEM_CAPTURESTART, EVENT_SYSTEM_MOVESIZEEND),
      (EVENT_SYSTEM_DRAGDROPSTART, EVENT_SYSTEM_DRAGDROPSTART),
      (EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND),
      (EVENT_OBJECT_LOCATIONCHANGE, EVENT_OBJECT_NAMECHANGE),
      (EVENT_OBJECT_CLOAKED, EVENT_OBJECT_UNCLOAKED),
    ];

    event_ranges
      .iter()
      .try_fold(Vec::new(), |mut handles, (min, max)| {
        // Create a window hook for the event range.
        let hook_handle = unsafe {
          SetWinEventHook(
            *min,
            *max,
            None,
            Some(Self::window_event_proc),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
          )
        };

        if hook_handle.is_invalid() {
          return Err(crate::Error::Platform(
            "Failed to set window event hook.".to_string(),
          ));
        }

        handles.push(hook_handle);
        Ok(handles)
      })
  }

  /// Callback passed to `SetWinEventHook`.
  ///
  /// This function is called on selected window events, and forwards them
  /// through an MPSC channel.
  extern "system" fn window_event_proc(
    _hook: HWINEVENTHOOK,
    event_type: u32,
    handle: HWND,
    id_object: i32,
    id_child: i32,
    _event_thread: u32,
    _event_time: u32,
  ) {
    if matches!(
      event_type,
      EVENT_SYSTEM_CAPTURESTART
        | EVENT_SYSTEM_CAPTUREEND
        | EVENT_SYSTEM_DRAGDROPSTART
    ) {
      let valid_obj =
        id_object == OBJID_WINDOW.0 || id_object == OBJID_CLIENT.0;
      if handle != HWND(0) && id_child == 0 && valid_obj {
        let root = unsafe { GetAncestor(handle, GA_ROOT) };
        let root_id = if root.0 != 0 { root.0 } else { handle.0 };
        let mut history = CAPTURE_HISTORY
          .lock()
          .unwrap_or_else(std::sync::PoisonError::into_inner);
        match event_type {
          EVENT_SYSTEM_CAPTURESTART => {
            history.record_capture_start(root_id);
          }
          EVENT_SYSTEM_CAPTUREEND => history.record_capture_end(root_id),
          EVENT_SYSTEM_DRAGDROPSTART => {
            history.record_dragdrop_start(root_id);
          }
          _ => {}
        }
      }
      return;
    }

    // Check whether the event is associated with a window object rather
    // than a UI control.
    let is_window_event =
      id_object == OBJID_WINDOW.0 && id_child == 0 && handle != HWND(0);

    if !is_window_event {
      return;
    }

    let Some(event_tx) = EVENT_TX.with(|lock| lock.get().cloned()) else {
      return;
    };

    let notification = crate::WindowEventNotification(None);

    let event = match event_type {
      EVENT_OBJECT_DESTROY => WindowEvent::Destroyed {
        window_id: WindowId(handle.0),
        notification,
      },
      EVENT_SYSTEM_FOREGROUND => WindowEvent::Focused {
        window: NativeWindow::new(handle.0).into(),
        notification,
      },
      EVENT_OBJECT_HIDE | EVENT_OBJECT_CLOAKED => WindowEvent::Hidden {
        window: NativeWindow::new(handle.0).into(),
        notification,
      },
      EVENT_OBJECT_LOCATIONCHANGE => WindowEvent::MovedOrResized {
        window: NativeWindow::new(handle.0).into(),
        is_interactive_start: false,
        is_interactive_end: false,
        notification,
      },
      EVENT_SYSTEM_MINIMIZESTART => WindowEvent::Minimized {
        window: NativeWindow::new(handle.0).into(),
        notification,
      },
      EVENT_SYSTEM_MINIMIZEEND => WindowEvent::MinimizeEnded {
        window: NativeWindow::new(handle.0).into(),
        notification,
      },
      EVENT_SYSTEM_MOVESIZESTART => WindowEvent::MovedOrResized {
        window: NativeWindow::new(handle.0).into(),
        is_interactive_start: true,
        is_interactive_end: false,
        notification,
      },
      EVENT_SYSTEM_MOVESIZEEND => WindowEvent::MovedOrResized {
        window: NativeWindow::new(handle.0).into(),
        is_interactive_start: false,
        is_interactive_end: true,
        notification,
      },
      EVENT_OBJECT_SHOW | EVENT_OBJECT_UNCLOAKED => WindowEvent::Shown {
        window: NativeWindow::new(handle.0).into(),
        notification,
      },
      EVENT_OBJECT_NAMECHANGE => WindowEvent::TitleChanged {
        window: NativeWindow::new(handle.0).into(),
        notification,
      },
      _ => return,
    };

    if let Err(err) = event_tx.send(event) {
      tracing::warn!("Failed to send window event: {}.", err);
    }
  }
}

impl Drop for WindowListener {
  fn drop(&mut self) {
    self.terminate();
  }
}
