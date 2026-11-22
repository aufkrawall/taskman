//! System file dialogs.
//!
//! Both pickers (`Create dump` on Details/Processes and the Run dialog's
//! `Browse...`) run on a worker thread: the system dialog blocks until the user
//! answers, and on the UI thread that starves `WM_PAINT` for as long as it is
//! open. The price is that the dialog is no longer modal by construction, so it
//! must be given an explicit owner — an unowned `IFileDialog` can end up behind
//! the main window, which looks exactly like a frozen UI.

/// The owner of a system dialog: the main window, as published by this
/// process's primary instance.
///
/// `None` when the handle is not known yet (before the first frame). The caller
/// then shows the dialog unowned rather than not at all.
#[cfg(target_os = "windows")]
pub(crate) fn main_window_parent() -> Option<DialogParent> {
    let hwnd = tm_platform::win::instance::published_window();
    (hwnd != 0).then_some(DialogParent(hwnd))
}

/// A raw window handle wrapper, so `rfd` can pass it to `IFileDialog::Show`.
#[cfg(target_os = "windows")]
pub(crate) struct DialogParent(isize);

#[cfg(target_os = "windows")]
impl raw_window_handle::HasWindowHandle for DialogParent {
    fn window_handle(
        &self,
    ) -> Result<raw_window_handle::WindowHandle<'_>, raw_window_handle::HandleError> {
        let Some(hwnd) = std::num::NonZeroIsize::new(self.0) else {
            return Err(raw_window_handle::HandleError::Unavailable);
        };
        // SAFETY: `self.0` is this process's own top-level window, published
        // once it was created and never reused for anything else. The dialog
        // only reads the handle while it is open, which is strictly inside the
        // lifetime of the window it belongs to.
        Ok(unsafe {
            raw_window_handle::WindowHandle::borrow_raw(raw_window_handle::RawWindowHandle::Win32(
                raw_window_handle::Win32WindowHandle::new(hwnd),
            ))
        })
    }
}

/// Required by `rfd::FileDialog::set_parent`; a Win32 dialog only uses the
/// window handle, and there is no display handle to name on Windows.
#[cfg(target_os = "windows")]
impl raw_window_handle::HasDisplayHandle for DialogParent {
    fn display_handle(
        &self,
    ) -> Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
        Err(raw_window_handle::HandleError::NotSupported)
    }
}
