//! RISC-V `virt` full-system machine support.
//!
//! The product machine is in [`virt`]. This module contains only the state
//! shared by the machine, its devices, and the full-system JIT dispatcher.

pub mod dtb;
pub mod rtc;
pub mod virt;
pub mod virtio;

/// Native WebSocket transport for the raw Ethernet relay.
#[cfg(not(target_arch = "wasm32"))]
pub mod ws;

use rv64_core::{Bus, Cpu, StopReason};

pub const RAM_BASE: u64 = 0x8000_0000;

/// Convert a guest-physical RAM range after proving that both endpoints fit
/// the host address width and the backing allocation.
pub fn checked_ram_range(
    ram_len: usize,
    ram_base: u64,
    addr: u64,
    len: usize,
) -> Option<core::ops::Range<usize>> {
    let offset = addr.checked_sub(ram_base)?;
    let end = offset.checked_add(u64::try_from(len).ok()?)?;
    if end > u64::try_from(ram_len).ok()? {
        return None;
    }
    Some(usize::try_from(offset).ok()?..usize::try_from(end).ok()?)
}

const JIT_PAGE_SHIFT: u32 = 12;
const JIT_PAGE_SIZE: usize = 1 << JIT_PAGE_SHIFT;
pub(crate) const INTERPRETER_SYNC_INTERVAL: u64 = 64;

#[derive(Clone, Copy, Default)]
struct JitPageFlags {
    marked: u64,
    dirty: u64,
}

/// Tracks RAM pages that back compiled code and pages written since the last
/// dispatcher drain.
///
/// Dirty flags make the hot store path idempotent. The dispatcher receives one
/// invalidation event for any number of writes to the same page between drains.
pub struct JitPageState {
    page_count: usize,
    flags: Vec<JitPageFlags>,
    dirty_pages: Vec<u64>,
    write_generations: Vec<u64>,
}

impl JitPageState {
    pub fn new(ram_bytes: usize) -> Self {
        let page_count = ram_bytes.div_ceil(JIT_PAGE_SIZE);
        let word_count = page_count.div_ceil(64);
        Self {
            page_count,
            flags: vec![JitPageFlags::default(); word_count],
            dirty_pages: Vec::new(),
            write_generations: vec![0; page_count],
        }
    }

    #[inline]
    fn page_for_address(&self, pa: u64) -> Option<usize> {
        let page = usize::try_from(pa.checked_sub(RAM_BASE)? >> JIT_PAGE_SHIFT).ok()?;
        (page < self.page_count).then_some(page)
    }

    #[inline]
    fn word_and_mask(&self, page: u64) -> Option<(usize, u64)> {
        let page = usize::try_from(page).ok()?;
        (page < self.page_count).then_some((page / 64, 1 << (page % 64)))
    }

    #[inline]
    pub fn mark_address(&mut self, pa: u64) -> bool {
        if let Some(page) = self.page_for_address(pa) {
            let mask = 1 << (page % 64);
            let flags = &mut self.flags[page / 64];
            let newly_marked = flags.marked & mask == 0;
            flags.marked |= mask;
            newly_marked
        } else {
            false
        }
    }

    #[inline]
    pub fn unmark_page(&mut self, page: u64) {
        if let Some((word, mask)) = self.word_and_mask(page) {
            self.flags[word].marked &= !mask;
        }
    }

    #[inline]
    pub fn page_marked(&self, page: u64) -> bool {
        self.word_and_mask(page)
            .is_some_and(|(word, mask)| self.flags[word].marked & mask != 0)
    }

    #[inline]
    pub fn address_marked(&self, pa: u64) -> bool {
        self.page_for_address(pa)
            .is_some_and(|page| self.flags[page / 64].marked & (1 << (page % 64)) != 0)
    }

    /// Return the write generation for a physical RAM page number.
    ///
    /// An asynchronous compiler compares this value at landing to reject code
    /// compiled from bytes that changed while compilation was in flight.
    #[inline]
    pub fn page_generation(&self, page: u64) -> Option<u64> {
        let page = usize::try_from(page).ok()?;
        self.write_generations.get(page).copied()
    }

    #[inline]
    pub fn note_store(&mut self, pa: u64) {
        if let Some(page) = self.page_for_address(pa) {
            self.note_page_write(page);
        }
    }

    /// Record a DMA write to every compiled-code page touched by the range.
    pub fn note_write(&mut self, pa: u64, len: usize) {
        if len == 0 {
            return;
        }
        let Some(last) = pa.checked_add(len as u64 - 1) else {
            return;
        };
        let (Some(first_page), Some(last_page)) =
            (self.page_for_address(pa), self.page_for_address(last))
        else {
            return;
        };
        for page in first_page..=last_page {
            self.note_page_write(page);
        }
    }

    #[inline]
    fn note_page_write(&mut self, page: usize) {
        let word = page / 64;
        let mask = 1 << (page % 64);
        let flags = &mut self.flags[word];
        if flags.marked & mask != 0 && flags.dirty & mask == 0 {
            self.write_generations[page] = self.write_generations[page].wrapping_add(1);
            flags.dirty |= mask;
            self.dirty_pages.push(page as u64);
        }
    }

    #[inline]
    pub fn has_dirty(&self) -> bool {
        !self.dirty_pages.is_empty()
    }

    #[inline]
    pub fn is_dirty(&self, page: u64) -> bool {
        self.word_and_mask(page)
            .is_some_and(|(word, mask)| self.flags[word].dirty & mask != 0)
    }

    /// Drain dirty physical RAM page numbers in first-write order.
    pub fn take_dirty(&mut self) -> Vec<u64> {
        let pages = core::mem::take(&mut self.dirty_pages);
        for &page in &pages {
            let (word, mask) = self
                .word_and_mask(page)
                .expect("dirty page must belong to guest RAM");
            self.flags[word].dirty &= !mask;
        }
        pages
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InterpreterStop {
    Cpu(StopReason),
    Compiled,
}

/// Result of one interpreter slice.
///
/// `idle` means that the hart stopped in WFI and the machine could not advance
/// a deterministic timer. The embedder must wait for time or external input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunSliceOutcome {
    pub retired: u64,
    pub idle: bool,
}

/// JIT routing queried by the decoded interpreter at basic-block boundaries.
///
/// `contains` is a side-effect-free lookahead used to stop a cached natural
/// block before a compiled successor. `observe` is called only for a PC that
/// execution actually reaches and may request a return to the tier-up dispatcher.
pub trait CodeDispatch {
    fn contains(&self, pc: u64) -> bool;
    fn observe(&mut self, pc: u64) -> bool;
}

/// Execute an interpreter slice, or stop after the first instruction whose
/// successor PC satisfies `compiled`.
#[inline]
pub(crate) fn run_cpu_until<B, F>(
    cpu: &mut Cpu,
    bus: &mut B,
    max_insns: u64,
    compiled: &mut Option<F>,
) -> InterpreterStop
where
    B: Bus,
    F: FnMut(u64) -> bool,
{
    let Some(compiled) = compiled.as_mut() else {
        return InterpreterStop::Cpu(cpu.run(bus, max_insns));
    };

    let start = cpu.insn_count;
    while cpu.insn_count - start < max_insns {
        let stop = cpu.run(bus, 1);
        if stop != StopReason::Budget {
            return InterpreterStop::Cpu(stop);
        }
        if compiled(cpu.pc) {
            return InterpreterStop::Compiled;
        }
    }
    InterpreterStop::Cpu(StopReason::Budget)
}

/// Current Unix-epoch time for native machine embeddings.
#[cfg(not(target_arch = "wasm32"))]
pub fn host_unix_time_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checked_ram_range_rejects_host_address_aliases() {
        assert_eq!(
            checked_ram_range(64, RAM_BASE, RAM_BASE + 4, 8),
            Some(4..12)
        );
        assert_eq!(
            checked_ram_range(64, RAM_BASE, RAM_BASE + (1u64 << 32) + 4, 8),
            None
        );
        assert_eq!(checked_ram_range(64, RAM_BASE, RAM_BASE + 60, 8), None);
    }

    #[test]
    fn jit_pages_deduplicate_writes_until_drain() {
        let mut state = JitPageState::new(3 * JIT_PAGE_SIZE);
        let code = RAM_BASE + JIT_PAGE_SIZE as u64;
        assert!(state.mark_address(code));
        assert!(!state.mark_address(code + 8));
        state.note_store(code + 4);
        state.note_store(code + 8);

        assert!(state.page_marked(1));
        assert_eq!(state.page_generation(1), Some(1));
        assert_eq!(state.take_dirty(), vec![1]);
        assert!(!state.has_dirty());

        state.note_store(code + 12);
        assert_eq!(state.page_generation(1), Some(2));
        assert_eq!(state.take_dirty(), vec![1]);
        state.unmark_page(1);
        state.note_store(code + 16);
        assert_eq!(state.page_generation(1), Some(2));
        assert!(!state.has_dirty());
    }

    #[test]
    fn jit_pages_track_each_page_of_a_dma_range() {
        let mut state = JitPageState::new(2 * JIT_PAGE_SIZE);
        state.mark_address(RAM_BASE);
        state.mark_address(RAM_BASE + JIT_PAGE_SIZE as u64);
        state.note_write(RAM_BASE + JIT_PAGE_SIZE as u64 - 2, 4);

        assert_eq!(state.page_generation(0), Some(1));
        assert_eq!(state.page_generation(1), Some(1));
        assert_eq!(state.take_dirty(), vec![0, 1]);
    }
}
