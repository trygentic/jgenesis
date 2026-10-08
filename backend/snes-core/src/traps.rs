//! lttprs: routine traps.
//!
//! A trap is a 24-bit program address. When the main CPU is at an instruction boundary and its
//! program counter equals an armed trap, the emulator burns one idle cycle instead of fetching
//! the opcode and records the address as pending. The host (lttp-core) then takes the pending
//! address with `SnesEmulator::take_trap`, runs its Rust port of the routine against the
//! emulator's memory and registers, and moves PC off the trap (typically by emulating the
//! routine's RTS/RTL). To fall back to the original 65816 code instead, the host calls
//! `SnesEmulator::bypass_trap_once`.

use std::collections::VecDeque;

use bincode::{Decode, Encode};

const FILTER_BYTES: usize = 0x10000 / 8;

/// Flag on a stall cycle length: the original ended an instruction on this cycle, so the CPU
/// samples its interrupt lines here.
pub const STALL_INSTRUCTION_END: u8 = 0x80;

#[derive(Debug, Clone, Encode, Decode)]
pub struct TrapSet {
    /// Prefilter over the low 16 bits of the address, so the per-instruction check is one load.
    filter: Box<[u8; FILTER_BYTES]>,
    /// Sorted, deduplicated armed addresses.
    addresses: Vec<u32>,
    pending: Option<u32>,
    bypass: Option<u32>,
    /// Stall addresses and the CPU cycle lengths (master cycles) still to idle at each; one per
    /// thread that is parked (e.g. main thread and a thread an NMI switched to).
    stalls: Vec<(u32, VecDeque<u8>)>,
    /// Master cycles of the last tick if it was a CPU cycle (not DMA or DRAM refresh).
    pub(crate) last_cpu_cycle: Option<u8>,
}

impl Default for TrapSet {
    fn default() -> Self {
        Self {
            filter: Box::new([0; FILTER_BYTES]),
            addresses: Vec::new(),
            pending: None,
            bypass: None,
            stalls: Vec::new(),
            last_cpu_cycle: None,
        }
    }
}

impl TrapSet {
    pub fn add(&mut self, address: u32) {
        if let Err(idx) = self.addresses.binary_search(&address) {
            self.addresses.insert(idx, address);
        }
        self.rebuild_filter();
    }

    pub fn remove(&mut self, address: u32) {
        if let Ok(idx) = self.addresses.binary_search(&address) {
            self.addresses.remove(idx);
        }
        self.rebuild_filter();
    }

    pub fn clear(&mut self) {
        self.addresses.clear();
        self.pending = None;
        self.bypass = None;
        self.rebuild_filter();
    }

    #[must_use]
    pub fn addresses(&self) -> &[u32] {
        &self.addresses
    }

    pub fn bypass_once(&mut self, address: u32) {
        self.bypass = Some(address);
    }

    pub fn take_pending(&mut self) -> Option<u32> {
        self.pending.take()
    }

    pub fn set_stall(&mut self, pc: u32, cycles: impl IntoIterator<Item = u8>) {
        let cycles: VecDeque<u8> = cycles.into_iter().filter(|&c| c > 0).collect();
        self.stalls.retain(|(stall_pc, _)| *stall_pc != pc);
        if !cycles.is_empty() {
            self.stalls.push((pc, cycles));
        }
    }

    /// Next stall cycle length if the CPU is parked at a stall address (0 = not stalled).
    #[inline]
    #[must_use]
    pub fn next_stall_cycle(&self, pc: u32) -> u8 {
        if self.stalls.is_empty() {
            return 0;
        }
        self.stalls
            .iter()
            .find(|(stall_pc, _)| *stall_pc == pc)
            .and_then(|(_, cycles)| cycles.front().copied())
            .unwrap_or(0)
    }

    pub fn pop_stall_cycle(&mut self, pc: u32) {
        if let Some(idx) = self.stalls.iter().position(|(stall_pc, _)| *stall_pc == pc) {
            self.stalls[idx].1.pop_front();
            if self.stalls[idx].1.is_empty() {
                self.stalls.swap_remove(idx);
            }
        }
    }

    fn rebuild_filter(&mut self) {
        self.filter.fill(0);
        for &address in &self.addresses {
            let low = (address & 0xFFFF) as usize;
            self.filter[low >> 3] |= 1 << (low & 7);
        }
    }

    /// Returns true (and records the address as pending) if `address` is an armed trap.
    #[inline]
    pub fn check(&mut self, address: u32) -> bool {
        let low = (address & 0xFFFF) as usize;
        if self.filter[low >> 3] & (1 << (low & 7)) == 0 {
            return false;
        }
        if self.pending == Some(address) {
            // Host has not serviced this trap yet; keep the CPU parked.
            return true;
        }
        if self.bypass == Some(address) {
            self.bypass = None;
            return false;
        }
        if self.addresses.binary_search(&address).is_ok() {
            self.pending = Some(address);
            return true;
        }
        false
    }
}
