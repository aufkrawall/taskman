//! Demand model for expensive telemetry (implement.md §6.3).
//!
//! The UI derives which telemetry the *visible* surface actually needs
//! (active tab, visible columns, open dialogs) and ships a cheap atomic u64
//! bitmask to the engine. Expensive providers (PDH GPU groups, disk counters,
//! token security queries, per-process network) are only kept warm while
//! their bit is set — plus a keep-alive window so flipping tabs does not
//! constantly tear down and rebuild sessions.

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TelemetryDemand(u64);

impl TelemetryDemand {
    pub const CORE_PROCESS: Self = Self(1 << 0);
    pub const DISK_RATE: Self = Self(1 << 1);
    pub const NET_ADAPTER_RATE: Self = Self(1 << 2);
    /// ETW per-process network session (Processes / App History pages).
    pub const PROCESS_NET: Self = Self(1 << 3);
    pub const GPU_ADAPTER: Self = Self(1 << 4);
    pub const PROCESS_GPU: Self = Self(1 << 5);
    pub const PROCESS_GPU_MEMORY: Self = Self(1 << 6);
    pub const TOKEN_SECURITY: Self = Self(1 << 7);
    /// CPU current-speed PDH counter (Performance page only).
    pub const CPU_SPEED: Self = Self(1 << 8);
    /// ETW per-process disk trace (the Disk active time column on the
    /// Processes and Details pages).
    pub const PROCESS_DISK: Self = Self(1 << 9);
    /// Wi-Fi connection details (SSID, signal strength) on the Performance
    /// network card.
    ///
    /// PRIVACY: the underlying `WlanQueryInterface(current_connection)` is
    /// gated by Windows behind PRECISE-LOCATION CONSENT. Calling it without
    /// consent triggers the one-time system location permission prompt, and
    /// every call shows up in the "location in use" tray activity — a task
    /// manager has no business asking for the user's location. This bit is
    /// therefore demanded ONLY after the user explicitly opted in
    /// (`Settings.wifi_details`) AND while those rows are on screen; a
    /// hidden surface (tray) never sets it. Deliberately NOT part of
    /// [`Self::all()`]: a diagnostic run must not trigger a privacy prompt.
    pub const WIFI_DETAILS: Self = Self(1 << 10);

    /// Union.
    pub fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// True when any bit of `other` is demanded.
    pub fn wants(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// Any GPU-related demand (adapter page or per-process columns).
    pub fn any_gpu(self) -> bool {
        self.wants(Self::GPU_ADAPTER)
            || self.wants(Self::PROCESS_GPU)
            || self.wants(Self::PROCESS_GPU_MEMORY)
    }

    pub fn bits(self) -> u64 {
        self.0
    }

    pub fn from_bits(b: u64) -> Self {
        Self(b)
    }

    /// Baseline always needed by the Processes/Details core tables.
    pub fn core() -> Self {
        Self::CORE_PROCESS
            .union(Self::NET_ADAPTER_RATE)
            .union(Self::TOKEN_SECURITY)
    }

    /// What a window nobody can see is allowed to keep running.
    ///
    /// Every provider above [`Self::CORE_PROCESS`] costs the MACHINE, not
    /// just this process: `PROCESS_NET` and `PROCESS_DISK` are real-time
    /// kernel ETW sessions that make the kernel emit, buffer and deliver an
    /// event for every network datagram, every completed disk request and
    /// every thread start on the box — for every process, not only the ones
    /// on screen. Keeping those warm for a tray icon is not a cost this
    /// program is entitled to impose, so a hidden surface drops to the
    /// kernel process table alone.
    ///
    /// The visible consequence is that App history stops accumulating
    /// NETWORK bytes while the window is away (accumulated CPU time is a
    /// counter on the process itself and keeps working). That is a real gap,
    /// and it is the deliberate trade: a background task manager must be
    /// invisible to the rest of the system.
    pub fn hidden() -> Self {
        Self::CORE_PROCESS
    }

    /// Every provider at once — except the consent-gated ones.
    ///
    /// Not a UI state — no page wants all of this — but exactly what a
    /// diagnostic run must ask for. `--selfcheck` used to sample at
    /// [`Self::core`] and then report `"gpus":[]`, which reads as "this
    /// machine has no GPU" when it actually means "the GPU providers were
    /// never switched on". Keep this in sync when a bit is added; the unit
    /// test below pins that.
    ///
    /// [`Self::WIFI_DETAILS`] is the one documented exemption: its provider
    /// is gated by Windows behind precise-location consent, so a diagnostic
    /// run demanding it would surface the location permission prompt.
    pub fn all() -> Self {
        Self::core()
            .union(Self::DISK_RATE)
            .union(Self::PROCESS_NET)
            .union(Self::GPU_ADAPTER)
            .union(Self::PROCESS_GPU)
            .union(Self::PROCESS_GPU_MEMORY)
            .union(Self::CPU_SPEED)
            .union(Self::PROCESS_DISK)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bit_operations() {
        let d = TelemetryDemand::CORE_PROCESS;
        assert!(d.wants(TelemetryDemand::CORE_PROCESS));
        assert!(!d.any_gpu());

        let gpu = TelemetryDemand::GPU_ADAPTER.union(TelemetryDemand::PROCESS_GPU);
        assert!(gpu.any_gpu());
        assert!(!gpu.wants(TelemetryDemand::CORE_PROCESS));

        let both = d.union(gpu);
        assert!(both.wants(TelemetryDemand::CORE_PROCESS));
        assert_eq!(
            both.bits(),
            TelemetryDemand::CORE_PROCESS.bits()
                | TelemetryDemand::GPU_ADAPTER.bits()
                | TelemetryDemand::PROCESS_GPU.bits()
        );
        assert_eq!(TelemetryDemand::from_bits(both.bits()), both);
    }

    /// A hidden surface must never be able to ask for more than a visible
    /// one, and must never reach a provider that costs the whole machine.
    #[test]
    fn hidden_demand_is_a_subset_of_core() {
        let hidden = TelemetryDemand::hidden();
        assert_eq!(hidden.bits() & !TelemetryDemand::core().bits(), 0);
        for (name, bit) in [
            ("PROCESS_NET", TelemetryDemand::PROCESS_NET),
            ("PROCESS_DISK", TelemetryDemand::PROCESS_DISK),
            ("PROCESS_GPU", TelemetryDemand::PROCESS_GPU),
            ("PROCESS_GPU_MEMORY", TelemetryDemand::PROCESS_GPU_MEMORY),
            ("GPU_ADAPTER", TelemetryDemand::GPU_ADAPTER),
            ("DISK_RATE", TelemetryDemand::DISK_RATE),
            ("CPU_SPEED", TelemetryDemand::CPU_SPEED),
            ("WIFI_DETAILS", TelemetryDemand::WIFI_DETAILS),
        ] {
            assert!(!hidden.wants(bit), "hidden() must not keep {name} warm");
        }
        // ...but it still has to sample processes, or App history and the
        // Performance graphs would simply stop.
        assert!(hidden.wants(TelemetryDemand::CORE_PROCESS));
    }

    /// `all()` must cover every declared bit. A provider added without being
    /// listed there silently drops out of `--selfcheck`, which is exactly how
    /// the GPU providers went unexercised by the headless smoke test.
    ///
    /// The one documented exemption is `WIFI_DETAILS`, and the reason is a
    /// privacy contract: its provider is `WlanQueryInterface`, which Windows
    /// gates behind precise-location consent. A diagnostic run must never
    /// surface the location permission prompt, so the bit is opt-in-only and
    /// must stay out of `all()` (and out of `core()`/`hidden()` — a baseline
    /// or tray demand may not reach it either).
    #[test]
    fn all_contains_every_declared_bit() {
        let all = TelemetryDemand::all();
        for (name, bit) in [
            ("CORE_PROCESS", TelemetryDemand::CORE_PROCESS),
            ("DISK_RATE", TelemetryDemand::DISK_RATE),
            ("NET_ADAPTER_RATE", TelemetryDemand::NET_ADAPTER_RATE),
            ("PROCESS_NET", TelemetryDemand::PROCESS_NET),
            ("GPU_ADAPTER", TelemetryDemand::GPU_ADAPTER),
            ("PROCESS_GPU", TelemetryDemand::PROCESS_GPU),
            ("PROCESS_GPU_MEMORY", TelemetryDemand::PROCESS_GPU_MEMORY),
            ("TOKEN_SECURITY", TelemetryDemand::TOKEN_SECURITY),
            ("CPU_SPEED", TelemetryDemand::CPU_SPEED),
            ("PROCESS_DISK", TelemetryDemand::PROCESS_DISK),
        ] {
            assert!(all.wants(bit), "all() is missing {name}");
        }
        // Every exercised bit and nothing beyond the declared ones (the
        // 11th declared bit is the consent-gated WIFI_DETAILS below).
        assert_eq!(all.bits().count_ones(), 10);
        assert!(all.any_gpu());
        let wifi = TelemetryDemand::WIFI_DETAILS;
        assert!(
            !all.wants(wifi),
            "a diagnostic run must not trigger the location prompt"
        );
        assert!(!TelemetryDemand::core().wants(wifi));
        assert!(!TelemetryDemand::hidden().wants(wifi));
        // The bit is real and independent of every other one.
        assert_eq!(wifi.bits().count_ones(), 1);
        assert_eq!(wifi.union(TelemetryDemand::core()).bits().count_ones(), 4);
    }
}
