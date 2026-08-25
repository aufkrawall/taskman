//! Tiny lock helpers over `std::sync` primitives.
//!
//! All locks in this app are held for microseconds; contention is negligible,
//! so the standard library types are sufficient. Lock poisoning is irrelevant
//! here (release builds use `panic = "abort"`), so guards are recovered via
//! `into_inner()` instead of panicking on a poisoned lock.

use std::sync::{Mutex, MutexGuard, RwLock};

/// Lock a [`Mutex`], recovering from poisoning instead of panicking.
#[inline]
pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Take a read guard from an [`RwLock`], recovering from poisoning.
#[inline]
pub fn read<T>(rw: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    rw.read().unwrap_or_else(|e| e.into_inner())
}

/// Take a write guard from an [`RwLock`], recovering from poisoning.
#[inline]
pub fn write<T>(rw: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    rw.write().unwrap_or_else(|e| e.into_inner())
}
