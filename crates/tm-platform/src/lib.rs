//! tm-platform — OS-specific collectors and actions behind clean traits.

pub mod actions;

#[cfg(target_os = "windows")]
pub mod win;

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(target_os = "macos")]
pub mod macos;

/// Build the platform's collector + action pair.
///
/// Returns `(collector, actions)`. Never fails: platforms degrade gracefully
/// and surface per-feature errors through their APIs at runtime.
pub fn create_stack() -> (
    Box<dyn tm_core::engine::SystemCollector>,
    Box<dyn actions::PlatformActions>,
) {
    #[cfg(target_os = "windows")]
    {
        let (c, a) = win::create();
        (Box::new(c), Box::new(a))
    }
    #[cfg(target_os = "linux")]
    {
        let (c, a) = linux::create();
        (Box::new(c), Box::new(a))
    }
    #[cfg(target_os = "macos")]
    {
        let (c, a) = macos::create();
        (Box::new(c), Box::new(a))
    }
}
