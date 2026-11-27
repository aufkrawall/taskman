//! TaskMan fork: KDE Plasma shell integration (`org_kde_plasma_shell`) so a
//! Wayland window hidden to the tray before any frame is presented does not
//! leave a ghost taskbar or switcher entry on KWin.

use sctk::globals::GlobalData;
use sctk::reexports::client::globals::{BindError, GlobalList};
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::{Connection, Dispatch, Proxy, QueueHandle, delegate_dispatch};
use wayland_protocols_plasma::plasma_shell::client::org_kde_plasma_shell::OrgKdePlasmaShell;
use wayland_protocols_plasma::plasma_shell::client::org_kde_plasma_surface::OrgKdePlasmaSurface;

use crate::platform_impl::wayland::state::WinitState;

/// KDE Plasma shell manager (`org_kde_plasma_shell`).
#[derive(Debug, Clone)]
pub struct PlasmaShell {
    shell: OrgKdePlasmaShell,
}

impl PlasmaShell {
    pub fn new(
        globals: &GlobalList,
        queue_handle: &QueueHandle<WinitState>,
    ) -> Result<Self, BindError> {
        // `set_skip_taskbar` requires v2; `set_skip_switcher` requires v5.
        let shell = globals.bind(queue_handle, 2..=8, GlobalData)?;
        Ok(Self { shell })
    }

    pub fn get_surface(
        &self,
        surface: &WlSurface,
        queue_handle: &QueueHandle<WinitState>,
    ) -> OrgKdePlasmaSurface {
        self.shell.get_surface(surface, queue_handle, ())
    }
}

impl Dispatch<OrgKdePlasmaShell, GlobalData, WinitState> for PlasmaShell {
    fn event(
        _: &mut WinitState,
        _: &OrgKdePlasmaShell,
        _: <OrgKdePlasmaShell as Proxy>::Event,
        _: &GlobalData,
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
        unreachable!("no events defined for org_kde_plasma_shell");
    }
}

impl Dispatch<OrgKdePlasmaSurface, (), WinitState> for PlasmaShell {
    fn event(
        _: &mut WinitState,
        _: &OrgKdePlasmaSurface,
        _: <OrgKdePlasmaSurface as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<WinitState>,
    ) {
    }
}

delegate_dispatch!(WinitState: [OrgKdePlasmaShell: GlobalData] => PlasmaShell);
delegate_dispatch!(WinitState: [OrgKdePlasmaSurface: ()] => PlasmaShell);
