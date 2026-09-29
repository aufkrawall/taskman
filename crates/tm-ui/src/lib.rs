//! Shared UI foundation: the Windows-11-Task-Manager theme palette
//! ([`theme`]) and the OS-native font setup ([`fonts`]).
//!
//! This crate exists so the task manager GUI and the setup installer render
//! from one source of truth: the installer must mirror the app's theming
//! exactly (proper Windows 11 dark and light mode), and duplicating the
//! palette or the text-weight tuning would let the two drift apart.

pub mod fonts;
pub mod theme;
