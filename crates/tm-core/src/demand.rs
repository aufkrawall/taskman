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

    /// Every provider at once.
    ///
    /// Not a UI state — no page wants all of this — but exactly what a
    /// diagnostic run must ask for. `--selfcheck` used to sample at
    /// [`Self::core`] and then report `"gpus":[]`, which reads as "this
    /// machine has no GPU" when it actually means "the GPU providers were
    /// never switched on". Keep this in sync when a bit is added; the unit
    /// test below pins that.
    pub fn all() -> Self {
        Self::core()
            .union(Self::DISK_RATE)
            .union(Self::PROCESS_NET)
            .union(Self::GPU_ADAPTER)
            .union(Self::PROCESS_GPU)
            .union(Self::PROCESS_GPU_MEMORY)
            .union(Self::CPU_SPEED)
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

    /// `all()` must cover every declared bit. A provider added without being
    /// listed there silently drops out of `--selfcheck`, which is exactly how
    /// the GPU providers went unexercised by the headless smoke test.
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
        ] {
            assert!(all.wants(bit), "all() is missing {name}");
        }
        // Every bit and nothing beyond the declared ones.
        assert_eq!(all.bits().count_ones(), 9);
        assert!(all.any_gpu());
    }
}
