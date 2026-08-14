//! RISC-V `virt` machine for OpenSBI and current Linux kernels.
//!
//! ```text
//! 0x0010_0000  sifive,test   (poweroff/reboot)
//! 0x0200_0000  CLINT         (msip, mtimecmp, mtime)
//! 0x0c00_0000  PLIC          (full: priorities, enables, thresholds, claim)
//! 0x1000_0000  UART          (ns16550 — OpenSBI + kernel console)
//! 0x1000_1000  virtio-mmio   (0x1000 apart; blk)
//! 0x8000_0000  RAM           (OpenSBI at base, kernel at +2 MiB, DTB above)
//! ```
//!
//! Boot: the hart resets in M-mode at RAM_BASE (OpenSBI fw_jump) with
//! a0=hartid, a1=dtb; OpenSBI sets up the SBI and drops to the S-mode kernel.

use crate::dtb::Fdt;
use crate::rtc::GoldfishRtc;
use crate::virtio::{Backend, VirtioDev};
use crate::{
    checked_ram_range, run_cpu_until, CodeDispatch, InterpreterStop, JitPageState, RunSliceOutcome,
    INTERPRETER_SYNC_INTERVAL,
};
use rv64_core::cpu::{DecodedInsn, DecodedRunOutcome};
use rv64_core::csr::{Mode, IRQ_MEIP, IRQ_MSIP, IRQ_MTIP, IRQ_SEIP, IRQ_SSIP, IRQ_STIP};
use rv64_core::{Bus, Cpu, Exception, StopReason};

pub use crate::RAM_BASE;

fn plic_dbg() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("RV_PLIC_DEBUG").is_ok())
}

pub const TEST_BASE: u64 = 0x0010_0000;
pub const CLINT_BASE: u64 = 0x0200_0000;
pub const CLINT_SIZE: u64 = 0x1_0000;
pub const PLIC_BASE: u64 = 0x0c00_0000;
pub const PLIC_SIZE: u64 = 0x0400_0000;
pub const UART_BASE: u64 = 0x1000_0000;
pub const UART_SIZE: u64 = 0x100;
pub const GOLDFISH_RTC_BASE: u64 = 0x0010_1000;
pub const GOLDFISH_RTC_SIZE: u64 = 0x1000;
pub const VIRTIO_BASE: u64 = 0x1000_1000;
pub const VIRTIO_SIZE: u64 = 0x1000;
pub const VIRTIO_COUNT: u64 = 8;
/// 10 MHz architected timer, matching the DTB timebase.
pub const RTC_FREQ: u64 = 10_000_000;

const KERNEL_OFFSET: u64 = 0x20_0000; // kernel Image at RAM_BASE + 2 MiB
const TOP_LAYOUT_MARGIN: u64 = 0x20_0000;
const FW_DYNAMIC_INFO_SIZE: u64 = 0x1000;

const CODE_CACHE_PAGES: usize = 256;
const CODE_PAGE_HALFWORDS: usize = 0x1000 / 2;
const DECODED_BLOCK_MAX: usize = 64;
const DECODED_PAGE_ARENA_MAX: usize = CODE_PAGE_HALFWORDS * 4;

#[derive(Clone, Copy, Default)]
struct CachedBlock {
    offset_plus_one: u32,
    len: u8,
}

impl CachedBlock {
    fn range(self) -> Option<core::ops::Range<usize>> {
        let start = usize::try_from(self.offset_plus_one).ok()?.checked_sub(1)?;
        Some(start..start + usize::from(self.len))
    }
}

struct DecodedCodePage {
    physical_page: u64,
    icache_gen: u64,
    blocks: Box<[CachedBlock]>,
    instructions: Vec<DecodedInsn>,
}

impl DecodedCodePage {
    fn new(physical_page: u64, icache_gen: u64) -> Self {
        Self {
            physical_page,
            icache_gen,
            blocks: vec![CachedBlock::default(); CODE_PAGE_HALFWORDS].into_boxed_slice(),
            instructions: Vec::new(),
        }
    }

    fn reset(&mut self, physical_page: u64, icache_gen: u64) {
        self.physical_page = physical_page;
        self.icache_gen = icache_gen;
        self.blocks.fill(CachedBlock::default());
        self.instructions.clear();
    }

    fn block<'a>(&'a mut self, ram: &[u8], page_offset: usize) -> Option<&'a [DecodedInsn]> {
        let block_slot = page_offset / 2;
        if let Some(range) = self.blocks[block_slot].range() {
            return self.instructions.get(range);
        }

        if self.instructions.len() + DECODED_BLOCK_MAX > DECODED_PAGE_ARENA_MAX {
            self.blocks.fill(CachedBlock::default());
            self.instructions.clear();
        }
        self.instructions.reserve(DECODED_BLOCK_MAX);

        let page_range = checked_ram_range(ram.len(), RAM_BASE, self.physical_page, 0x1000)?;
        let page = &ram[page_range];
        let arena_start = self.instructions.len();
        let mut instruction_slots = [0usize; DECODED_BLOCK_MAX];
        let mut offset = page_offset;

        while self.instructions.len() - arena_start < DECODED_BLOCK_MAX {
            let lo = u16::from_le_bytes(page.get(offset..offset + 2)?.try_into().ok()?);
            let word = if lo & 3 == 3 {
                let Some(bytes) = page.get(offset..offset + 4) else {
                    break;
                };
                u32::from_le_bytes(bytes.try_into().ok()?)
            } else {
                u32::from(lo)
            };
            let decoded = DecodedInsn::from_word(word);
            instruction_slots[self.instructions.len() - arena_start] = offset / 2;
            self.instructions.push(decoded);
            offset += decoded.byte_len() as usize;
            if decoded.ends_basic_block() || offset == 0x1000 {
                break;
            }
        }

        let block_len = self.instructions.len() - arena_start;
        if block_len == 0 {
            return None;
        }
        let offset_plus_one = u32::try_from(arena_start + 1).ok()?;
        for (index, &slot) in instruction_slots[..block_len].iter().enumerate() {
            self.blocks[slot] = CachedBlock {
                offset_plus_one: offset_plus_one + index as u32,
                len: (block_len - index) as u8,
            };
        }
        self.instructions.get(arena_start..arena_start + block_len)
    }
}

/// Fixed-size L1 for the interpreter's decoded instruction pages. Each page
/// owns compact natural basic blocks, so a cache hit lends the CPU a persistent
/// decoded slice without rebuilding a temporary block.
struct CodePageCache {
    pages: Vec<Option<DecodedCodePage>>,
}

impl CodePageCache {
    fn new() -> Self {
        let mut pages = Vec::with_capacity(CODE_CACHE_PAGES);
        pages.resize_with(CODE_CACHE_PAGES, || None);
        Self { pages }
    }

    #[inline]
    fn slot(physical_page: u64) -> usize {
        let page = physical_page >> 12;
        (page ^ (page >> 9)) as usize & (CODE_CACHE_PAGES - 1)
    }

    fn block<'a>(
        &'a mut self,
        ram: &[u8],
        physical: u64,
        icache_gen: u64,
    ) -> Option<&'a [DecodedInsn]> {
        let physical_page = physical & !0xfff;
        let page_offset = usize::try_from(physical & 0xfff).ok()?;
        if page_offset & 1 != 0 || page_offset > 0xffe {
            return None;
        }
        let slot = Self::slot(physical_page);
        let replace = self.pages[slot].as_ref().is_none_or(|page| {
            page.physical_page != physical_page || page.icache_gen != icache_gen
        });
        if replace {
            if let Some(page) = self.pages[slot].as_mut() {
                page.reset(physical_page, icache_gen);
            } else {
                self.pages[slot] = Some(DecodedCodePage::new(physical_page, icache_gen));
            }
        }
        let page = self.pages[slot]
            .as_mut()
            .expect("decoded page was installed");
        page.block(ram, page_offset)
    }
}

trait InterpreterBackend {
    fn run(
        &mut self,
        cpu: &mut Cpu,
        bus: &mut VirtBus,
        cache: &mut CodePageCache,
        max_insns: u64,
    ) -> InterpreterStop;
    fn should_stop(&mut self, pc: u64) -> bool;
    fn needs_periodic_sync(&self) -> bool;
}

struct LegacyInterpreter<F> {
    compiled: Option<F>,
}

impl<F> InterpreterBackend for LegacyInterpreter<F>
where
    F: FnMut(u64) -> bool,
{
    #[inline]
    fn run(
        &mut self,
        cpu: &mut Cpu,
        bus: &mut VirtBus,
        _cache: &mut CodePageCache,
        max_insns: u64,
    ) -> InterpreterStop {
        run_cpu_until(cpu, bus, max_insns, &mut self.compiled)
    }

    #[inline]
    fn should_stop(&mut self, pc: u64) -> bool {
        self.compiled.as_mut().is_some_and(|compiled| compiled(pc))
    }

    #[inline]
    fn needs_periodic_sync(&self) -> bool {
        self.compiled.is_some()
    }
}

struct CachedInterpreter<'a, D> {
    dispatch: &'a mut D,
    stop_capable: bool,
}

impl<D: CodeDispatch> InterpreterBackend for CachedInterpreter<'_, D> {
    #[inline]
    fn run(
        &mut self,
        cpu: &mut Cpu,
        bus: &mut VirtBus,
        cache: &mut CodePageCache,
        max_insns: u64,
    ) -> InterpreterStop {
        run_cached_cpu(cpu, bus, cache, max_insns, self.dispatch, self.stop_capable)
    }

    #[inline]
    fn should_stop(&mut self, pc: u64) -> bool {
        self.dispatch.observe(pc)
    }

    #[inline]
    fn needs_periodic_sync(&self) -> bool {
        self.stop_capable
    }
}

fn run_cached_cpu<D: CodeDispatch>(
    cpu: &mut Cpu,
    bus: &mut VirtBus,
    cache: &mut CodePageCache,
    max_insns: u64,
    dispatch: &mut D,
    stop_capable: bool,
) -> InterpreterStop {
    let start = cpu.insn_count;
    let mut stalled_traps = 0u64;
    let mut first_block = true;
    loop {
        if !first_block && dispatch.observe(cpu.pc) {
            return InterpreterStop::Compiled;
        }
        first_block = false;

        let retired = cpu.insn_count - start;
        if retired >= max_insns || stalled_traps >= max_insns {
            return InterpreterStop::Cpu(StopReason::Budget);
        }

        let remaining = max_insns - retired;
        let pc = cpu.pc;
        let Some(physical) = cpu.jit_probe_fetch(bus, pc) else {
            if let Some(stop) = run_uncached_one(cpu, bus, &mut stalled_traps) {
                return stop;
            }
            continue;
        };
        if physical & 0xfff != pc & 0xfff
            || checked_ram_range(bus.ram.len(), RAM_BASE, physical & !0xfff, 0x1000).is_none()
        {
            if let Some(stop) = run_uncached_one(cpu, bus, &mut stalled_traps) {
                return stop;
            }
            continue;
        }

        let Some(block) = cache.block(&bus.ram, physical, cpu.icache_gen) else {
            if let Some(stop) = run_uncached_one(cpu, bus, &mut stalled_traps) {
                return stop;
            }
            continue;
        };
        let mut count = block
            .len()
            .min(usize::try_from(remaining).unwrap_or(usize::MAX));
        if stop_capable {
            let mut next_pc = pc;
            for (index, decoded) in block[..count].iter().enumerate() {
                next_pc = next_pc.wrapping_add(decoded.byte_len());
                if dispatch.contains(next_pc) {
                    count = index + 1;
                    break;
                }
            }
        }
        let before = cpu.insn_count;
        match cpu.run_decoded(bus, &block[..count]) {
            DecodedRunOutcome::Stop(StopReason::Budget) => {
                stalled_traps = 0;
            }
            DecodedRunOutcome::Stop(stop) => return InterpreterStop::Cpu(stop),
            DecodedRunOutcome::Trapped => {
                if cpu.insn_count == before {
                    stalled_traps = stalled_traps.saturating_add(1);
                } else {
                    stalled_traps = 0;
                }
            }
        }
    }
}

#[inline]
fn run_uncached_one(
    cpu: &mut Cpu,
    bus: &mut VirtBus,
    stalled_traps: &mut u64,
) -> Option<InterpreterStop> {
    let before = cpu.insn_count;
    let stop = cpu.run(bus, 1);
    if cpu.insn_count == before {
        *stalled_traps = stalled_traps.saturating_add(1);
    } else {
        *stalled_traps = 0;
    }
    (stop != StopReason::Budget).then_some(InterpreterStop::Cpu(stop))
}

// Interrupt source numbers (PLIC). Source 0 = "no interrupt".
const UART_IRQ: u32 = 10;
const GOLDFISH_RTC_IRQ: u32 = 11;
const VIRTIO_IRQ_BASE: u32 = 1; // virtio dev i -> source (1 + i)

const PLIC_SOURCES: usize = 32; // one u32 bitmask is enough
const PLIC_CONTEXTS: usize = 2; // ctx0 = hart0 M-ext, ctx1 = hart0 S-ext

/// Full SiFive/QEMU-style PLIC.
struct Plic {
    priority: [u32; PLIC_SOURCES],
    pending: u32, // level-driven by device lines (recomputed)
    enable: [u32; PLIC_CONTEXTS],
    threshold: [u32; PLIC_CONTEXTS],
    claimed: u32, // in-service (claimed, awaiting complete)
}

impl Plic {
    fn new() -> Plic {
        Plic {
            priority: [0; PLIC_SOURCES],
            pending: 0,
            enable: [0; PLIC_CONTEXTS],
            threshold: [0; PLIC_CONTEXTS],
            claimed: 0,
        }
    }

    /// Best claimable source for a context (highest priority > threshold,
    /// enabled, pending, not already in-service). 0 if none.
    fn best(&self, ctx: usize) -> u32 {
        let elig = self.pending & self.enable[ctx] & !self.claimed;
        let mut best_id = 0u32;
        let mut best_pri = 0u32;
        let mut m = elig;
        while m != 0 {
            let i = m.trailing_zeros();
            m &= m - 1;
            let pri = self.priority[i as usize];
            if pri > self.threshold[ctx] && pri >= best_pri {
                best_pri = pri;
                best_id = i;
            }
        }
        best_id
    }

    /// Does context `ctx` have a deliverable external interrupt?
    fn pending_ctx(&self, ctx: usize) -> bool {
        self.best(ctx) != 0
    }

    fn read(&mut self, off: u64) -> u32 {
        match off {
            // source priorities: 0x0 + id*4  (id 1..)
            0x0000..=0x0fff => {
                let id = (off / 4) as usize;
                self.priority.get(id).copied().unwrap_or(0)
            }
            // pending bits: 0x1000 (32 sources in first word)
            0x1000 => self.pending,
            0x1004 => 0,
            // enables: 0x2000 + ctx*0x80
            _ if (0x2000..0x2000 + (PLIC_CONTEXTS as u64) * 0x80).contains(&off) => {
                let ctx = ((off - 0x2000) / 0x80) as usize;
                if (off - 0x2000).is_multiple_of(0x80) {
                    self.enable[ctx]
                } else {
                    0
                }
            }
            // per-context: threshold at 0x200000 + ctx*0x1000, claim at +4
            _ if off >= 0x20_0000 => {
                let ctx = ((off - 0x20_0000) / 0x1000) as usize;
                let reg = (off - 0x20_0000) % 0x1000;
                if ctx >= PLIC_CONTEXTS {
                    return 0;
                }
                match reg {
                    0x0 => self.threshold[ctx],
                    0x4 => {
                        // claim: return best, mark in-service, clear pending
                        let id = self.best(ctx);
                        if id != 0 {
                            if plic_dbg() {
                                eprintln!("[plic] claim[ctx{ctx}] -> src={id}");
                            }
                            self.claimed |= 1 << id;
                        }
                        id
                    }
                    _ => 0,
                }
            }
            _ => 0,
        }
    }

    fn write(&mut self, off: u64, val: u32) {
        match off {
            0x0000..=0x0fff => {
                let id = (off / 4) as usize;
                if id != 0 {
                    if plic_dbg() {
                        eprintln!("[plic] priority[{id}]={val}");
                    }
                    if let Some(p) = self.priority.get_mut(id) {
                        *p = val;
                    }
                }
            }
            _ if (0x2000..0x2000 + (PLIC_CONTEXTS as u64) * 0x80).contains(&off) => {
                let ctx = ((off - 0x2000) / 0x80) as usize;
                if (off - 0x2000).is_multiple_of(0x80) {
                    if plic_dbg() {
                        eprintln!("[plic] enable[ctx{ctx}]={val:#x}");
                    }
                    self.enable[ctx] = val & !1; // source 0 never enabled
                }
            }
            _ if off >= 0x20_0000 => {
                let ctx = ((off - 0x20_0000) / 0x1000) as usize;
                let reg = (off - 0x20_0000) % 0x1000;
                if ctx >= PLIC_CONTEXTS {
                    return;
                }
                match reg {
                    0x0 => {
                        if plic_dbg() {
                            eprintln!("[plic] threshold[ctx{ctx}]={val}");
                        }
                        self.threshold[ctx] = val;
                    }
                    0x4 => {
                        // complete: clear in-service for this source
                        if plic_dbg() {
                            eprintln!("[plic] complete[ctx{ctx}] src={val}");
                        }
                        if (val as usize) < PLIC_SOURCES {
                            self.claimed &= !(1 << val);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
}

/// Minimal ns16550 (8250) UART: enough for OpenSBI + kernel console.
struct Uart {
    ier: u8, // interrupt enable
    lcr: u8,
    mcr: u8,
    scr: u8,
    rx: std::collections::VecDeque<u8>,
    tx_out: Vec<u8>,
    /// THR-empty interrupt pending (transmit is instant, so this tracks the
    /// 8250 THRE interrupt: armed when TX ints are enabled or a byte is sent,
    /// cleared when the guest reads IIR).
    thre_ip: bool,
}

impl Uart {
    fn new() -> Uart {
        Uart {
            ier: 0,
            lcr: 0,
            mcr: 0,
            scr: 0,
            rx: Default::default(),
            tx_out: Vec::new(),
            thre_ip: false,
        }
    }
    // Register offsets (byte): 0 RBR/THR/DLL, 1 IER/DLM, 2 IIR/FCR, 3 LCR,
    // 4 MCR, 5 LSR, 6 MSR, 7 SCR. DLAB (LCR bit7) selects divisor latches.
    fn read(&mut self, off: u64) -> u8 {
        let dlab = self.lcr & 0x80 != 0;
        match off {
            0 if !dlab => self.rx.pop_front().unwrap_or(0), // RBR
            0 => 0,                                         // DLL
            1 if !dlab => self.ier,
            1 => 0, // DLM
            2 => {
                // IIR, highest-priority pending source first (FIFO bits 0xc0):
                // RX data available (0x04) outranks THR-empty (0x02). Reading
                // IIR acknowledges (clears) a pending THRE interrupt.
                if self.ier & 1 != 0 && !self.rx.is_empty() {
                    0xc4
                } else if self.thre_ip {
                    self.thre_ip = false;
                    0xc2
                } else {
                    0xc1
                }
            }
            3 => self.lcr,
            4 => self.mcr,
            5 => {
                // LSR: THR empty + TX empty always; DR if rx data.
                let mut lsr = 0x60;
                if !self.rx.is_empty() {
                    lsr |= 0x01;
                }
                lsr
            }
            6 => 0xb0, // MSR: DCD+DSR+CTS
            7 => self.scr,
            _ => 0,
        }
    }
    fn write(&mut self, off: u64, val: u8) {
        let dlab = self.lcr & 0x80 != 0;
        match off {
            0 if !dlab => {
                self.tx_out.push(val); // THR — transmitted instantly
                                       // THR is now empty again: re-arm the THR-empty interrupt so
                                       // interrupt-driven TX keeps flowing.
                if self.ier & 2 != 0 {
                    self.thre_ip = true;
                }
            }
            1 if !dlab => {
                let was = self.ier;
                self.ier = val;
                // Enabling the THR-empty interrupt raises it immediately (our
                // THR is always empty). Without this, the 8250 driver's
                // interrupt-driven TX / tty drain (e.g. bash's tcsetattr on the
                // console) blocks forever waiting for a THRE IRQ.
                if val & 2 != 0 && was & 2 == 0 {
                    self.thre_ip = true;
                }
            }
            3 => self.lcr = val,
            4 => self.mcr = val,
            7 => self.scr = val,
            _ => {}
        }
    }
    /// UART interrupt line: RX-data (if enabled) or THR-empty (if enabled).
    fn irq(&self) -> bool {
        (self.ier & 1 != 0 && !self.rx.is_empty()) || self.thre_ip
    }
}

pub struct VirtBus {
    pub ram: Vec<u8>,
    // CLINT
    pub mtime: u64,
    pub mtimecmp: u64,
    pub msip: bool,
    pub rtc: GoldfishRtc,
    plic: Plic,
    uart: Uart,
    pub virtio: Vec<VirtioDev>,
    pub power_off: bool,
    direct_sbi: bool,
    pub jit: JitPageState,
}

impl VirtBus {
    fn refresh_plic(&mut self) {
        // Recompute level-triggered pending bits from device lines.
        let mut p = 0u32;
        if self.uart.irq() {
            p |= 1 << UART_IRQ;
        }
        if self.rtc.irq() {
            p |= 1 << GOLDFISH_RTC_IRQ;
        }
        for (i, d) in self.virtio.iter().enumerate() {
            if d.irq_pending() {
                p |= 1 << (VIRTIO_IRQ_BASE + i as u32);
            }
        }
        // Keep already-claimed-but-still-asserted lines out of `pending`
        // (they re-assert after `complete`); level sources naturally re-set.
        self.plic.pending = p;
    }

    /// Drain UART output (host console).
    pub fn uart_take(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.uart.tx_out)
    }
    /// Feed console input to the guest UART.
    pub fn uart_input(&mut self, bytes: &[u8]) {
        self.uart.rx.extend(bytes.iter().copied());
    }

    pub fn jit_mark_page(&mut self, pa: u64) {
        self.jit.mark_address(pa);
    }
    pub fn jit_page_marked(&self, page: u64) -> bool {
        self.jit.page_marked(page)
    }
    pub fn jit_unmark_page(&mut self, page: u64) {
        self.jit.unmark_page(page);
    }
    pub fn jit_take_dirty(&mut self) -> Vec<u64> {
        self.jit.take_dirty()
    }
    pub fn jit_has_dirty(&self) -> bool {
        self.jit.has_dirty()
    }
    pub fn jit_page_dirty(&self, page: u64) -> bool {
        self.jit.is_dirty(page)
    }
    pub fn jit_page_generation(&self, page: u64) -> Option<u64> {
        self.jit.page_generation(page)
    }
    #[inline]
    fn jit_check_store(&mut self, addr: u64) {
        self.jit.note_store(addr);
    }

    #[inline]
    fn ram_slice(&mut self, addr: u64, len: usize) -> Option<&mut [u8]> {
        let range = checked_ram_range(self.ram.len(), RAM_BASE, addr, len)?;
        Some(&mut self.ram[range])
    }

    fn mmio_read(&mut self, addr: u64, size: u32) -> Option<u64> {
        match addr {
            _ if (TEST_BASE..TEST_BASE + 0x1000).contains(&addr) => Some(0),
            _ if (CLINT_BASE..CLINT_BASE + CLINT_SIZE).contains(&addr) => {
                Some(match addr - CLINT_BASE {
                    0x0 => self.msip as u64,
                    0x4000 => self.mtimecmp,
                    0xbff8 => self.mtime,
                    _ => 0,
                })
            }
            _ if (PLIC_BASE..PLIC_BASE + PLIC_SIZE).contains(&addr) => {
                Some(self.plic.read(addr - PLIC_BASE) as u64)
            }
            _ if (UART_BASE..UART_BASE + UART_SIZE).contains(&addr) => {
                Some(self.uart.read(addr - UART_BASE) as u64)
            }
            _ if (GOLDFISH_RTC_BASE..GOLDFISH_RTC_BASE + GOLDFISH_RTC_SIZE).contains(&addr) => {
                Some(self.rtc.read(addr - GOLDFISH_RTC_BASE) as u64)
            }
            _ if (VIRTIO_BASE..VIRTIO_BASE + VIRTIO_COUNT * VIRTIO_SIZE).contains(&addr) => {
                let i = ((addr - VIRTIO_BASE) / VIRTIO_SIZE) as usize;
                let off = (addr - VIRTIO_BASE) % VIRTIO_SIZE;
                self.virtio
                    .get_mut(i)
                    .map(|d| d.read_sized(off, size) as u64)
            }
            _ => None,
        }
    }

    fn mmio_write(&mut self, addr: u64, val: u64, _size: u32) -> bool {
        match addr {
            _ if (TEST_BASE..TEST_BASE + 0x1000).contains(&addr) => {
                // sifive,test: 0x5555 poweroff, 0x7777 reboot, 0x3333|.. fail
                let v = val as u32 & 0xffff;
                if v == 0x5555 || v == 0x7777 || (val as u32 & 0xffff) == 0x3333 {
                    self.power_off = true;
                }
                true
            }
            _ if (CLINT_BASE..CLINT_BASE + CLINT_SIZE).contains(&addr) => {
                match addr - CLINT_BASE {
                    0x0 => self.msip = val & 1 != 0,
                    0x4000 => self.mtimecmp = val,
                    _ => {}
                }
                true
            }
            _ if (PLIC_BASE..PLIC_BASE + PLIC_SIZE).contains(&addr) => {
                self.plic.write(addr - PLIC_BASE, val as u32);
                true
            }
            _ if (UART_BASE..UART_BASE + UART_SIZE).contains(&addr) => {
                self.uart.write(addr - UART_BASE, val as u8);
                true
            }
            _ if (GOLDFISH_RTC_BASE..GOLDFISH_RTC_BASE + GOLDFISH_RTC_SIZE).contains(&addr) => {
                self.rtc.write(addr - GOLDFISH_RTC_BASE, val as u32);
                true
            }
            _ if (VIRTIO_BASE..VIRTIO_BASE + VIRTIO_COUNT * VIRTIO_SIZE).contains(&addr) => {
                let i = ((addr - VIRTIO_BASE) / VIRTIO_SIZE) as usize;
                let off = (addr - VIRTIO_BASE) % VIRTIO_SIZE;
                if i < self.virtio.len() {
                    if let Some(q) = self.virtio[i].write(off, val as u32) {
                        let mut dev = self.virtio.remove(i);
                        dev.process(q as usize, &mut self.ram, RAM_BASE, &mut self.jit);
                        self.virtio.insert(i, dev);
                    }
                }
                true
            }
            _ => false,
        }
    }

    /// Poll every ready virtqueue, servicing any buffers the guest made
    /// available. QueueNotify normally drives this, but polling each slice
    /// recovers from any missed notification (a synchronous device model can
    /// otherwise lose a wakeup and hang the guest waiting on completed I/O).
    fn poll_virtio(&mut self) {
        for i in 0..self.virtio.len() {
            let mut dev = self.virtio.remove(i);
            for qi in 0..2 {
                dev.process(qi, &mut self.ram, RAM_BASE, &mut self.jit);
            }
            self.virtio.insert(i, dev);
        }
    }
}

macro_rules! virt_rw {
    ($rd:ident, $wr:ident, $ty:ty, $n:expr) => {
        fn $rd(&mut self, addr: u64) -> Result<$ty, Exception> {
            if let Some(s) = self.ram_slice(addr, $n) {
                let b: [u8; $n] = (&*s).try_into().unwrap();
                return Ok(<$ty>::from_le_bytes(b));
            }
            if let Some(v) = self.mmio_read(addr, $n) {
                return Ok(v as $ty);
            }
            Err(Exception::LoadAccessFault { addr })
        }
        fn $wr(&mut self, addr: u64, val: $ty) -> Result<(), Exception> {
            self.jit_check_store(addr);
            if let Some(s) = self.ram_slice(addr, $n) {
                s.copy_from_slice(&val.to_le_bytes());
                return Ok(());
            }
            if self.mmio_write(addr, val as u64, $n) {
                return Ok(());
            }
            Err(Exception::StoreAccessFault { addr })
        }
    };
}

impl Bus for VirtBus {
    virt_rw!(read8, write8, u8, 1);
    virt_rw!(read16, write16, u16, 2);
    virt_rw!(read32, write32, u32, 4);
    virt_rw!(read64, write64, u64, 8);

    #[inline]
    fn fetch32_if_safe(&mut self, addr: u64) -> Option<u32> {
        let bytes: [u8; 4] = (&*self.ram_slice(addr, 4)?).try_into().unwrap();
        Some(u32::from_le_bytes(bytes))
    }

    fn irq_lines(&mut self) -> u64 {
        self.refresh_plic();
        let mut lines = 0u64;
        if !self.direct_sbi && self.mtime >= self.mtimecmp {
            lines |= IRQ_MTIP;
        }
        if self.msip {
            lines |= IRQ_MSIP;
        }
        if self.plic.pending_ctx(0) {
            lines |= IRQ_MEIP;
        }
        if self.plic.pending_ctx(1) {
            lines |= IRQ_SEIP;
        }
        lines
    }

    fn jit_fast_off(&self, va: u64, pa: u64, store: bool) -> Option<i64> {
        if pa < RAM_BASE || (pa | 0xfff) >= RAM_BASE + self.ram.len() as u64 {
            return None;
        }
        if store && self.jit.address_marked(pa) {
            return None;
        }
        Some(self.ram.as_ptr() as i64 + (pa as i64 - RAM_BASE as i64) - va as i64)
    }
}

pub struct VirtImages<'a> {
    pub opensbi: &'a [u8],
    pub kernel: &'a [u8],
    pub cmdline: &'a str,
    pub initrd: Option<&'a [u8]>,
    pub disk: Option<Vec<u8>>,
    /// Size of a native disk image. When present, the block device keeps the
    /// image outside guest RAM and exposes requests through the host ABI.
    pub external_disk_size: Option<u64>,
    /// MAC address for a layer-2 virtio-net device.
    pub net: Option<[u8; 6]>,
}

pub struct VirtMachine {
    pub cpu: Cpu,
    pub bus: VirtBus,
    code_cache: CodePageCache,
    pub insns_per_tick: u64,
    /// Guest timer ticks accrued while the hart was halted in WFI. Kept
    /// separate from `insn_count` so idle time advances the guest clock
    /// without inflating the (real) retired-instruction count.
    pub idle_ticks: u64,
    /// Host-monotonic timer ticks elapsed since boot. When present, browser
    /// execution follows wall time instead of fast-forwarding every WFI.
    pub realtime_ticks: Option<u64>,
    pub power_off: bool,
    pub dtb: Vec<u8>,
    pub unsupported_sbi: Option<(u64, u64)>,
    /// Diagnostic call counts: total, BASE, TIME, IPI, RFENCE, HSM, SRST,
    /// and legacy/other. These do not participate in guest behavior.
    pub sbi_calls: [u64; 8],
}

impl VirtMachine {
    pub fn new(ram_bytes: u64, images: VirtImages) -> VirtMachine {
        Self::new_inner(ram_bytes, images, false)
    }

    pub fn new_direct(ram_bytes: u64, images: VirtImages) -> VirtMachine {
        Self::new_inner(ram_bytes, images, true)
    }

    fn new_inner(ram_bytes: u64, images: VirtImages, direct_sbi: bool) -> VirtMachine {
        let minimum_ram = KERNEL_OFFSET
            .checked_add(images.kernel.len() as u64)
            .and_then(|end| end.checked_add(TOP_LAYOUT_MARGIN + FW_DYNAMIC_INFO_SIZE))
            .expect("kernel image is too large");
        assert!(
            ram_bytes >= minimum_ram,
            "guest RAM must fit the kernel and top-of-RAM boot data"
        );
        let ram_size = ram_bytes;
        let mut ram = vec![0u8; ram_size as usize];

        // OpenSBI at RAM base.
        ram[..images.opensbi.len()].copy_from_slice(images.opensbi);
        // Kernel Image at RAM_BASE + 2 MiB.
        let kbase = KERNEL_OFFSET as usize;
        ram[kbase..kbase + images.kernel.len()].copy_from_slice(images.kernel);
        let kend = kbase + images.kernel.len();

        let _ = kend;
        let n_virtio = (images.disk.is_some() || images.external_disk_size.is_some()) as usize
            + images.net.is_some() as usize;
        // Place initrd + DTB near the TOP of RAM (as QEMU/U-Boot do) so the
        // kernel's early allocations near the Image don't clobber them.
        // Layout from the top down: [DTB][initrd][fw_dynamic_info], each
        // aligned, leaving a small margin below the very top.
        let ram_top = ram_size as usize;
        let dtb = build_virt_fdt(ram_size, images.cmdline, 0, 0, n_virtio);
        // Reserve DTB just below the top (2 MiB margin, page aligned).
        let dtb_off = ((ram_top - TOP_LAYOUT_MARGIN as usize).saturating_sub(dtb.len())) & !0xfff;

        // initrd below the DTB (1 MiB aligned).
        let mut initrd_start = 0u64;
        let mut initrd_end = 0u64;
        let mut below = dtb_off;
        if let Some(ir) = images.initrd {
            let s = (below.saturating_sub(ir.len())) & !0xfffff;
            ram[s..s + ir.len()].copy_from_slice(ir);
            initrd_start = RAM_BASE + s as u64;
            initrd_end = initrd_start + ir.len() as u64;
            below = s;
        }

        // fw_dynamic_info struct below the initrd (page aligned).
        let dyn_off = (below.saturating_sub(FW_DYNAMIC_INFO_SIZE as usize)) & !0xfff;

        // Rebuild the DTB now that initrd addresses are known, then place it.
        let dtb = build_virt_fdt(ram_size, images.cmdline, initrd_start, initrd_end, n_virtio);
        ram[dtb_off..dtb_off + dtb.len()].copy_from_slice(&dtb);
        let dtb_addr = RAM_BASE + dtb_off as u64;

        // Slot i takes PLIC source VIRTIO_IRQ_BASE + i, matching the DTB above.
        let mut virtio = Vec::new();
        if let Some(disk) = images.disk {
            virtio.push(VirtioDev::new(Backend::Block { disk }));
        } else if let Some(size) = images.external_disk_size {
            virtio.push(VirtioDev::new(Backend::ExternalBlock {
                size,
                pending: None,
            }));
        }
        if let Some(mac) = images.net {
            virtio.push(VirtioDev::new(Backend::Net {
                mac,
                inbox: Vec::new(),
                outbox: Vec::new(),
            }));
        }

        // fw_dynamic_info struct (OpenSBI reads it from a2). fw_jump.bin bakes
        // the FDT/next-stage addresses at build time and ignores a1, which
        // makes the kernel fault on a bogus DTB pointer; fw_dynamic forwards the
        // real DTB in a1 and jumps to the address we specify here.
        {
            let info: [u64; 6] = [
                0x4942_534f,              // magic "OSBI"
                2,                        // version
                RAM_BASE + KERNEL_OFFSET, // next_addr = kernel Image
                1,                        // next_mode = PRV_S
                0,                        // options
                0,                        // boot_hart = 0
            ];
            for (i, v) in info.iter().enumerate() {
                ram[dyn_off + i * 8..dyn_off + i * 8 + 8].copy_from_slice(&v.to_le_bytes());
            }
        }
        let dyn_addr = RAM_BASE + dyn_off as u64;

        // Enter OpenSBI (fw_dynamic) in M-mode: pc=RAM_BASE, a0=hartid, a1=dtb,
        // a2=&fw_dynamic_info.
        let mut cpu = Cpu::new();
        cpu.enable_system(0);
        cpu.pc = if direct_sbi {
            RAM_BASE + KERNEL_OFFSET
        } else {
            RAM_BASE
        };
        cpu.x[10] = 0; // a0 = hartid
        cpu.x[11] = dtb_addr; // a1 = dtb
        cpu.x[12] = dyn_addr; // a2 = fw_dynamic_info
        if direct_sbi {
            let sys = cpu.sys.as_mut().unwrap();
            sys.mode = Mode::Supervisor;
            sys.medeleg = 0xb109;
            sys.mideleg = IRQ_SSIP | IRQ_STIP | IRQ_SEIP;
            sys.mcounteren = 0x7;
            sys.satp = 0;
            cpu.refresh_jit_tlb_context();
            cpu.enable_host_sbi();
        }

        VirtMachine {
            cpu,
            bus: VirtBus {
                ram,
                mtime: 0,
                mtimecmp: u64::MAX,
                msip: false,
                rtc: GoldfishRtc::new(),
                plic: Plic::new(),
                uart: Uart::new(),
                virtio,
                power_off: false,
                direct_sbi,
                jit: JitPageState::new(ram_size as usize),
            },
            code_cache: CodePageCache::new(),
            insns_per_tick: 100,
            idle_ticks: 0,
            realtime_ticks: None,
            power_off: false,
            dtb,
            unsupported_sbi: None,
            sbi_calls: [0; 8],
        }
    }

    pub fn console_output(&mut self) -> Vec<u8> {
        self.bus.uart_take()
    }
    pub fn console_input(&mut self, bytes: &[u8]) {
        self.bus.uart_input(bytes);
    }

    /// Deliver an inbound Ethernet frame to the guest's NIC. `poll_virtio`
    /// already runs every slice here, so it lands as soon as the guest has a
    /// buffer posted.
    pub fn net_input(&mut self, frame: &[u8]) {
        if let Some(dev) = self.bus.virtio.iter_mut().find(|d| d.device_id() == 1) {
            dev.net_input(frame);
        }
    }

    /// Collect the Ethernet frames the guest has transmitted.
    pub fn net_take_output(&mut self) -> Vec<Vec<u8>> {
        self.bus
            .virtio
            .iter_mut()
            .find(|d| d.device_id() == 1)
            .map(|d| d.net_take_output())
            .unwrap_or_default()
    }

    pub fn pending_block_request(&self) -> Option<crate::virtio::PendingBlockRequest> {
        self.bus
            .virtio
            .iter()
            .find_map(|dev| dev.pending_block_request())
    }

    pub fn complete_block_request(&mut self, id: u64, data: &[u8], ok: bool) -> bool {
        let Some(dev) = self
            .bus
            .virtio
            .iter_mut()
            .find(|dev| dev.pending_block_request().is_some())
        else {
            return false;
        };
        let result = dev.complete_block_request(
            id,
            data,
            ok,
            &mut self.bus.ram,
            RAM_BASE,
            &mut self.bus.jit,
        );
        if result {
            self.sync_devices();
        }
        result
    }

    pub fn pending_block_request_len(&self) -> Option<u64> {
        self.pending_block_request().map(|request| request.len())
    }

    /// Supply the RTC's Unix-epoch time from the embedding host.
    pub fn set_rtc_unix_ns(&mut self, ns: u64) {
        self.bus.rtc.set_host_time_ns(ns);
    }

    /// Enable/advance realtime clocking by a host-monotonic duration.
    pub fn advance_realtime_ns(&mut self, ns: u64) {
        let ticks = ((ns as u128 * RTC_FREQ as u128) / 1_000_000_000) as u64;
        self.realtime_ticks = Some(self.realtime_ticks.unwrap_or(0).saturating_add(ticks));
    }

    pub fn sync_devices(&mut self) {
        let instruction_time = self.cpu.insn_count / self.insns_per_tick + self.idle_ticks;
        let visible_time = self.cpu.sys.as_ref().map_or(self.bus.mtime, |sys| {
            self.cpu
                .insn_count
                .checked_div(sys.time_scale)
                .map_or(sys.mtime, |time| time.wrapping_add(sys.time_offset))
        });
        self.bus.mtime = match self.realtime_ticks {
            // Preserve the value already visible through rdtime. Re-anchoring
            // only to the last host sample would move the guest clock backward
            // after instruction-based interpolation between host samples. If
            // interpolation ran ahead, freeze it below until wall time catches
            // up instead of carrying the speculative lead into every slice.
            Some(wall) => self.bus.mtime.max(wall).max(visible_time),
            None => instruction_time,
        };
        if let Some(sys) = self.cpu.sys.as_mut() {
            sys.mtime = self.bus.mtime;
            if self.bus.direct_sbi {
                if self.bus.mtime >= self.bus.mtimecmp {
                    sys.mip |= IRQ_STIP;
                } else {
                    sys.mip &= !IRQ_STIP;
                }
            }
            let instruction_ticks = self.cpu.insn_count / self.insns_per_tick;
            let wall_caught_up = self
                .realtime_ticks
                .is_none_or(|wall| self.bus.mtime <= wall);
            if wall_caught_up {
                if let Some(offset) = self.bus.mtime.checked_sub(instruction_ticks) {
                    // Let rdtime advance between host samples so a short
                    // busy-wait loop can make progress without a JS call.
                    sys.time_scale = self.insns_per_tick;
                    sys.time_offset = offset;
                } else {
                    sys.time_scale = 0;
                    sys.time_offset = 0;
                }
            } else {
                // The guest already observed a speculative value ahead of the
                // host clock. A zero scale makes TIME read sys.mtime verbatim;
                // the next host sample can re-enable interpolation.
                sys.time_scale = 0;
                sys.time_offset = 0;
            }
        }
    }

    /// Poll DMA-capable devices and synchronize interrupt/time state before a
    /// full-system JIT dispatch quantum.
    pub fn sync_jit_devices(&mut self) {
        self.bus.poll_virtio();
        self.sync_devices();
        self.power_off |= self.bus.power_off;
    }

    fn service_sbi(&mut self) {
        const NOT_SUPPORTED: u64 = (-2i64) as u64;
        const INVALID_PARAM: u64 = (-3i64) as u64;
        const ALREADY_STARTED: u64 = (-7i64) as u64;
        const BASE: u64 = 0x10;
        const TIME: u64 = 0x5449_4d45;
        const IPI: u64 = 0x0073_5049;
        const RFENCE: u64 = 0x5246_4e43;
        const HSM: u64 = 0x48_53_4d;
        const SRST: u64 = 0x5352_5354;

        let ext = self.cpu.x[17];
        let function = self.cpu.x[16];
        let arg0 = self.cpu.x[10];
        let arg1 = self.cpu.x[11];
        self.sbi_calls[0] += 1;
        let bucket = match ext {
            BASE => 1,
            TIME => 2,
            IPI => 3,
            RFENCE => 4,
            HSM => 5,
            SRST => 6,
            _ => 7,
        };
        self.sbi_calls[bucket] += 1;
        let mut error = 0;
        let mut value = 0;
        match (ext, function) {
            (BASE, 0) => value = 2 << 24,     // SBI 2.0
            (BASE, 1) => value = 0x5256_3634, // "RV64"
            (BASE, 2) => value = 1,
            (BASE, 3) => {
                value = matches!(arg0, BASE | TIME | IPI | RFENCE | HSM | SRST) as u64;
            }
            (BASE, 4 | 5) => value = 0,
            (BASE, 6) => value = 1,
            (TIME, 0) => {
                self.bus.mtimecmp = arg0;
                // TIME.set_timer replaces the previous event. If STIP from
                // that event remains asserted until the outer slice ends,
                // Linux immediately re-enters its timer handler and rearms
                // again, producing a timer-interrupt storm. OpenSBI drops its
                // MTIP-derived line as soon as mtimecmp moves into the future;
                // direct SBI must provide the same level-triggered behavior.
                if let Some(sys) = self.cpu.sys.as_mut() {
                    if self.bus.mtime >= self.bus.mtimecmp {
                        sys.mip |= IRQ_STIP;
                    } else {
                        sys.mip &= !IRQ_STIP;
                    }
                }
            }
            (IPI, 0) => {
                if arg1 != 0 && arg1 != u64::MAX {
                    error = INVALID_PARAM;
                }
            }
            (RFENCE, 0) => {
                self.cpu.icache_gen = self.cpu.icache_gen.wrapping_add(1);
            }
            (RFENCE, 1..=6) => {}
            (HSM, 0) => error = ALREADY_STARTED,
            (HSM, 1) => error = INVALID_PARAM,
            (HSM, 2) if arg0 == 0 => value = 0, // STARTED
            (HSM, 2) => error = INVALID_PARAM,
            (HSM, 3) => error = NOT_SUPPORTED,
            (SRST, 0) if arg0 <= 2 => {
                self.bus.power_off = true;
                self.power_off = true;
            }
            (SRST, 0) => error = INVALID_PARAM,
            // Legacy SBI 0.1, useful for older kernels.
            (0, _) => {
                self.bus.mtimecmp = arg0;
                if let Some(sys) = self.cpu.sys.as_mut() {
                    if self.bus.mtime >= self.bus.mtimecmp {
                        sys.mip |= IRQ_STIP;
                    } else {
                        sys.mip &= !IRQ_STIP;
                    }
                }
            }
            (1, _) => self.bus.uart.write(0, arg0 as u8),
            (2, _) => value = u64::MAX,
            (5, _) => {
                self.cpu.icache_gen = self.cpu.icache_gen.wrapping_add(1);
            }
            (3..=4 | 6..=7, _) => {}
            (8, _) => {
                self.bus.power_off = true;
                self.power_off = true;
            }
            _ => {
                error = NOT_SUPPORTED;
                self.unsupported_sbi = Some((ext, function));
            }
        }
        self.cpu.x[10] = error;
        self.cpu.x[11] = value;
    }

    /// Read a u16 from guest RAM (little-endian), for ring inspection.
    fn ram_u16(&self, pa: u64) -> u16 {
        checked_ram_range(self.bus.ram.len(), RAM_BASE, pa, 2)
            .map(|range| &self.bus.ram[range])
            .map(|s| u16::from_le_bytes([s[0], s[1]]))
            .unwrap_or(0)
    }

    /// One-line dump of interrupt-delivery state, for diagnosing idle hangs.
    pub fn debug_irq_state(&self) -> String {
        let p = &self.bus.plic;
        let vio: Vec<String> = self
            .bus
            .virtio
            .iter()
            .enumerate()
            .map(|(i, d)| {
                // For queue 0: avail.idx (guest submitted), my last_avail
                // (serviced), used.idx (device published), all so we can see
                // whether there's outstanding I/O the guest is blocked on.
                let ring = d.queue_debug(0).map(|(_r, _n, avail, used, last_avail)| {
                    let avail_idx = self.ram_u16(avail + 2);
                    let used_idx = self.ram_u16(used + 2);
                    format!(" q0[avail.idx={avail_idx} serviced={last_avail} used.idx={used_idx} outstanding={}]",
                        avail_idx.wrapping_sub(last_avail))
                }).unwrap_or_default();
                format!("v{i}.irq={}{ring}", d.irq_pending())
            })
            .collect();
        let (mip, mie) = self
            .cpu
            .sys
            .as_ref()
            .map(|s| (s.mip, s.mie))
            .unwrap_or((0, 0));
        let (sepc, scause) = self
            .cpu
            .sys
            .as_ref()
            .map(|s| (s.sepc, s.scause))
            .unwrap_or((0, 0));
        format!(
            "plic{{pend={:#x} claimed={:#x} best1={}}} {} mip={:#x} mie={:#x} mtime={} mtcmp={} timer_future={} sepc={:#x} scause={:#x}",
            p.pending, p.claimed, p.best(1),
            vio.join(","), mip, mie, self.bus.mtime, self.bus.mtimecmp,
            self.bus.mtimecmp > self.bus.mtime, sepc, scause,
        )
    }

    pub fn run_slice(&mut self, max_insns: u64) -> u64 {
        self.run_slice_outcome(max_insns).retired
    }

    pub fn run_slice_outcome(&mut self, max_insns: u64) -> RunSliceOutcome {
        self.run_slice_inner::<fn(u64) -> bool>(max_insns, None)
    }

    /// Run through the shared decoded-page cache. `dispatch` overlays compiled
    /// entries on the same PC stream and observes only successors that execution
    /// actually reaches.
    pub fn run_cached_slice_outcome<D: CodeDispatch>(
        &mut self,
        max_insns: u64,
        dispatch: &mut D,
        stop_capable: bool,
    ) -> RunSliceOutcome {
        let mut backend = CachedInterpreter {
            dispatch,
            stop_capable,
        };
        self.run_slice_backend(max_insns, &mut backend)
    }

    /// Interpret up to `max_insns`, but return as soon as an executed
    /// instruction reaches a successor PC for which `compiled` returns true.
    /// Direct SBI calls remain host-serviced and do not escape this method.
    pub fn run_slice_until(&mut self, max_insns: u64, compiled: impl FnMut(u64) -> bool) -> u64 {
        self.run_slice_until_outcome(max_insns, compiled).retired
    }

    pub fn run_slice_until_outcome(
        &mut self,
        max_insns: u64,
        compiled: impl FnMut(u64) -> bool,
    ) -> RunSliceOutcome {
        self.run_slice_inner(max_insns, Some(compiled))
    }

    #[inline]
    fn run_slice_inner<F>(&mut self, max_insns: u64, compiled: Option<F>) -> RunSliceOutcome
    where
        F: FnMut(u64) -> bool,
    {
        let mut backend = LegacyInterpreter { compiled };
        self.run_slice_backend(max_insns, &mut backend)
    }

    #[inline]
    fn run_slice_backend(
        &mut self,
        max_insns: u64,
        backend: &mut impl InterpreterBackend,
    ) -> RunSliceOutcome {
        let start = self.cpu.insn_count;
        self.bus.poll_virtio();
        self.sync_devices();
        if self.pending_block_request().is_some() {
            return RunSliceOutcome {
                retired: 0,
                idle: false,
            };
        }
        let mut remaining = max_insns;
        let mut stop = InterpreterStop::Cpu(StopReason::Budget);
        while remaining != 0 && !self.power_off {
            let chunk = if backend.needs_periodic_sync() || self.has_external_disk() {
                remaining.min(INTERPRETER_SYNC_INTERVAL)
            } else {
                remaining
            };
            let before = self.cpu.insn_count;
            stop = backend.run(&mut self.cpu, &mut self.bus, &mut self.code_cache, chunk);
            let retired = self.cpu.insn_count - before;
            remaining = remaining.saturating_sub(retired);

            match stop {
                InterpreterStop::Cpu(StopReason::Ecall) if self.bus.direct_sbi => {
                    self.service_sbi();
                    if self.unsupported_sbi.is_some() {
                        self.power_off = true;
                        break;
                    }
                    if self.power_off {
                        break;
                    }
                    if backend.should_stop(self.cpu.pc) {
                        stop = InterpreterStop::Compiled;
                        break;
                    }
                    continue;
                }
                InterpreterStop::Cpu(StopReason::Budget) if remaining != 0 => {
                    self.sync_devices();
                    if self.pending_block_request().is_some() {
                        break;
                    }
                    if retired == 0 {
                        break;
                    }
                    if !backend.needs_periodic_sync() && !self.has_external_disk() {
                        break;
                    }
                    continue;
                }
                _ => break,
            }
        }
        let mut idle = false;
        if stop == InterpreterStop::Cpu(StopReason::Wfi) {
            // Halted: fast-forward the guest clock to the next timer
            // deadline via `idle_ticks` (not `insn_count`, which must
            // stay a true retired-instruction count for budgets/perf).
            let next = self.bus.mtimecmp;
            if self.realtime_ticks.is_none() && next != u64::MAX && next > self.bus.mtime {
                self.idle_ticks += next - self.bus.mtime;
            } else {
                idle = true;
            }
        }
        self.sync_devices();
        self.power_off |= self.bus.power_off;
        RunSliceOutcome {
            retired: self.cpu.insn_count - start,
            idle,
        }
    }

    fn has_external_disk(&self) -> bool {
        self.bus
            .virtio
            .iter()
            .any(|device| matches!(device.backend, Backend::ExternalBlock { .. }))
    }
}

fn build_virt_fdt(
    ram_size: u64,
    cmdline: &str,
    initrd_start: u64,
    initrd_end: u64,
    n_virtio: usize,
) -> Vec<u8> {
    let mut f = Fdt::new();
    let intc_phandle = 1u32;
    let plic_phandle = 2u32;

    f.begin_node("");
    f.prop_u32("#address-cells", 2);
    f.prop_u32("#size-cells", 2);
    f.prop_str("compatible", "riscv-virtio");
    f.prop_str("model", "riscv-virtio,qemu");

    f.begin_node("chosen");
    f.prop_str("bootargs", cmdline);
    f.prop_str("stdout-path", "/soc/serial@10000000");
    // Seed the kernel CRNG from the DTB (as QEMU/U-Boot do). Without this the
    // guest starves for entropy — jitterentropy can't init on our too-regular
    // cycle counter, so every getrandom() blocks and boot stalls ~30s/step.
    // CONFIG_RANDOM_TRUST_BOOTLOADER credits this as full entropy.
    let mut seed = [0u8; 64];
    for (i, b) in seed.iter_mut().enumerate() {
        // Deterministic but well-mixed; a fixed seed is fine (the kernel only
        // needs unpredictable-to-the-guest bytes to initialize the CRNG).
        *b = ((i as u32).wrapping_mul(0x9e37_79b1) >> 13) as u8 ^ (i as u8).wrapping_mul(31);
    }
    f.prop("rng-seed", &seed);
    if initrd_end > initrd_start {
        f.prop("linux,initrd-start", &initrd_start.to_be_bytes());
        f.prop("linux,initrd-end", &initrd_end.to_be_bytes());
    }
    f.end_node();

    f.begin_node("cpus");
    f.prop_u32("#address-cells", 1);
    f.prop_u32("#size-cells", 0);
    f.prop_u32("timebase-frequency", RTC_FREQ as u32);
    f.begin_node("cpu@0");
    f.prop_str("device_type", "cpu");
    f.prop_u32("reg", 0);
    f.prop_str("status", "okay");
    f.prop_str("compatible", "riscv");
    f.prop_str("riscv,isa-base", "rv64i");
    f.prop_strs(
        "riscv,isa-extensions",
        &["i", "m", "a", "f", "d", "c", "zicntr", "zicsr", "zifencei"],
    );
    // OpenSBI 1.4 (the firmware pinned by this repository) still discovers
    // hart capabilities from the legacy string. Linux uses the structured
    // properties above, so the optimized kernel needs no ISA fallback.
    f.prop_str("riscv,isa", "rv64imafdc");
    f.prop_str("mmu-type", "riscv,sv48");
    f.begin_node("interrupt-controller");
    f.prop_u32("#interrupt-cells", 1);
    f.prop("interrupt-controller", &[]);
    f.prop_str("compatible", "riscv,cpu-intc");
    f.prop_u32("phandle", intc_phandle);
    f.end_node();
    f.end_node(); // cpu@0
    f.end_node(); // cpus

    f.begin_node(&format!("memory@{RAM_BASE:x}"));
    f.prop_str("device_type", "memory");
    f.prop_u64_pair("reg", RAM_BASE, ram_size);
    f.end_node();

    f.begin_node("soc");
    f.prop_u32("#address-cells", 2);
    f.prop_u32("#size-cells", 2);
    f.prop_str("compatible", "simple-bus");
    f.prop("ranges", &[]);

    // test finisher
    f.begin_node(&format!("test@{TEST_BASE:x}"));
    f.prop_strs("compatible", &["sifive,test1", "sifive,test0", "syscon"]);
    f.prop_u64_pair("reg", TEST_BASE, 0x1000);
    let syscon_ph = 5u32;
    f.prop_u32("phandle", syscon_ph);
    f.end_node();
    f.begin_node("poweroff");
    f.prop_str("compatible", "syscon-poweroff");
    f.prop_u32("regmap", syscon_ph);
    f.prop_u32("offset", 0);
    f.prop_u32("value", 0x5555);
    f.end_node();
    f.begin_node("reboot");
    f.prop_str("compatible", "syscon-reboot");
    f.prop_u32("regmap", syscon_ph);
    f.prop_u32("offset", 0);
    f.prop_u32("value", 0x7777);
    f.end_node();

    // UART (ns16550)
    f.begin_node(&format!("serial@{UART_BASE:x}"));
    f.prop_str("compatible", "ns16550a");
    f.prop_u64_pair("reg", UART_BASE, UART_SIZE);
    f.prop_u32("clock-frequency", 3_686_400);
    f.prop_u32s("interrupts-extended", &[plic_phandle, UART_IRQ]);
    f.end_node();

    f.begin_node(&format!("rtc@{GOLDFISH_RTC_BASE:x}"));
    f.prop_str("compatible", "google,goldfish-rtc");
    f.prop_u64_pair("reg", GOLDFISH_RTC_BASE, GOLDFISH_RTC_SIZE);
    f.prop_u32s("interrupts-extended", &[plic_phandle, GOLDFISH_RTC_IRQ]);
    f.end_node();

    // CLINT
    f.begin_node(&format!("clint@{CLINT_BASE:x}"));
    f.prop_strs("compatible", &["sifive,clint0", "riscv,clint0"]);
    f.prop_u64_pair("reg", CLINT_BASE, CLINT_SIZE);
    f.prop_u32s("interrupts-extended", &[intc_phandle, 3, intc_phandle, 7]);
    f.end_node();

    // PLIC
    f.begin_node(&format!("plic@{PLIC_BASE:x}"));
    f.prop_strs("compatible", &["sifive,plic-1.0.0", "riscv,plic0"]);
    f.prop_u32("#interrupt-cells", 1);
    f.prop("interrupt-controller", &[]);
    f.prop_u64_pair("reg", PLIC_BASE, PLIC_SIZE);
    f.prop_u32("riscv,ndev", (PLIC_SOURCES - 1) as u32);
    // contexts: hart0 M-ext (11) then hart0 S-ext (9)
    f.prop_u32s("interrupts-extended", &[intc_phandle, 11, intc_phandle, 9]);
    f.prop_u32("phandle", plic_phandle);
    f.end_node();

    // virtio-mmio slots
    for i in 0..n_virtio {
        let base = VIRTIO_BASE + (i as u64) * VIRTIO_SIZE;
        f.begin_node(&format!("virtio_mmio@{base:x}"));
        f.prop_str("compatible", "virtio,mmio");
        f.prop_u64_pair("reg", base, VIRTIO_SIZE);
        f.prop_u32s(
            "interrupts-extended",
            &[plic_phandle, VIRTIO_IRQ_BASE + i as u32],
        );
        f.end_node();
    }

    f.end_node(); // soc
    f.end_node(); // root
    f.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    #[test]
    fn virt_fdt_uses_structured_isa_properties() {
        let fdt = build_virt_fdt(512 << 20, "console=ttyS0", 0, 0, 2);
        assert!(contains_bytes(&fdt, b"riscv,isa-base\0"));
        assert!(contains_bytes(&fdt, b"riscv,isa-extensions\0"));
        assert!(contains_bytes(
            &fdt,
            b"i\0m\0a\0f\0d\0c\0zicntr\0zicsr\0zifencei\0",
        ));
    }

    fn direct_machine() -> VirtMachine {
        VirtMachine::new_direct(
            8 << 20,
            VirtImages {
                opensbi: &[],
                kernel: &[],
                cmdline: "console=ttyS0",
                initrd: None,
                disk: None,
                external_disk_size: None,
                net: None,
            },
        )
    }

    fn write_program(machine: &mut VirtMachine, instructions: &[u32]) {
        let start = KERNEL_OFFSET as usize;
        for (index, instruction) in instructions.iter().enumerate() {
            let offset = start + index * 4;
            machine.bus.ram[offset..offset + 4].copy_from_slice(&instruction.to_le_bytes());
        }
    }

    #[derive(Default)]
    struct TestDispatch {
        compiled: Option<u64>,
        observed: Vec<u64>,
    }

    impl CodeDispatch for TestDispatch {
        fn contains(&self, pc: u64) -> bool {
            self.compiled == Some(pc)
        }

        fn observe(&mut self, pc: u64) -> bool {
            self.observed.push(pc);
            self.contains(pc)
        }
    }

    #[test]
    fn decoded_cache_stops_at_a_compiled_basic_block_entry() {
        let mut machine = direct_machine();
        let start = machine.cpu.pc;
        write_program(&mut machine, &[0x0012_8293, 0x0012_8293, 0x0012_8293]);
        let mut dispatch = TestDispatch {
            compiled: Some(start + 8),
            observed: Vec::new(),
        };

        let outcome = machine.run_cached_slice_outcome(10, &mut dispatch, true);

        assert_eq!(outcome.retired, 2);
        assert_eq!(machine.cpu.pc, start + 8);
        assert_eq!(machine.cpu.x[5], 2);
        assert_eq!(dispatch.observed, vec![start + 8]);
    }

    #[test]
    fn fence_i_invalidates_decoded_instruction_pages() {
        let mut machine = direct_machine();
        let start = machine.cpu.pc;
        write_program(&mut machine, &[0x0010_0293, 0x0000_100f]);
        let mut dispatch = TestDispatch::default();

        machine.run_cached_slice_outcome(1, &mut dispatch, false);
        assert_eq!(machine.cpu.x[5], 1);

        write_program(&mut machine, &[0x0020_0293, 0x0000_100f]);
        machine.cpu.pc = start;
        machine.cpu.x[5] = 0;
        machine.run_cached_slice_outcome(1, &mut dispatch, false);
        assert_eq!(
            machine.cpu.x[5], 1,
            "cached bytes remain valid before FENCE.I"
        );

        machine.cpu.pc = start + 4;
        machine.run_cached_slice_outcome(1, &mut dispatch, false);
        machine.cpu.pc = start;
        machine.cpu.x[5] = 0;
        machine.run_cached_slice_outcome(1, &mut dispatch, false);
        assert_eq!(machine.cpu.x[5], 2);
    }

    #[test]
    fn plain_fence_does_not_invalidate_decoded_instruction_pages() {
        let mut machine = direct_machine();
        write_program(&mut machine, &[0x0000_000f]);
        let generation = machine.cpu.icache_gen;
        let mut dispatch = TestDispatch::default();

        machine.run_cached_slice_outcome(1, &mut dispatch, false);

        assert_eq!(machine.cpu.icache_gen, generation);
    }

    #[test]
    fn cached_execution_leaves_on_taken_branches() {
        let mut machine = direct_machine();
        // addi x5,x0,1; beq x5,x5,+8; addi x6,x0,99; addi x7,x0,7
        write_program(
            &mut machine,
            &[0x0010_0293, 0x0052_8463, 0x0630_0313, 0x0070_0393],
        );
        let mut dispatch = TestDispatch::default();

        machine.run_cached_slice_outcome(3, &mut dispatch, false);

        assert_eq!(machine.cpu.x[6], 0);
        assert_eq!(machine.cpu.x[7], 7);
    }

    #[test]
    fn trap_at_the_sequential_pc_rechecks_jit_dispatch() {
        let mut machine = direct_machine();
        let start = machine.cpu.pc;
        // ld x5,0(x1); addi x6,x0,99. x1=0 causes a load access fault.
        write_program(&mut machine, &[0x0000_b283, 0x0630_0313]);
        let sys = machine.cpu.sys.as_mut().unwrap();
        sys.medeleg |= 1 << 5;
        sys.stvec = start + 4;
        let mut dispatch = TestDispatch {
            compiled: Some(start + 4),
            observed: Vec::new(),
        };

        machine.run_cached_slice_outcome(1, &mut dispatch, false);

        assert_eq!(machine.cpu.insn_count, 0);
        assert_eq!(machine.cpu.x[6], 0);
        assert_eq!(machine.cpu.pc, start + 4);
        assert_eq!(dispatch.observed, vec![start + 4]);
    }

    #[test]
    fn cached_execution_leaves_on_interrupts() {
        let mut machine = direct_machine();
        let start = machine.cpu.pc;
        write_program(&mut machine, &[0x0630_0293]); // addi x5,x0,99
        let handler = start + 0x100;
        let offset = (KERNEL_OFFSET + 0x100) as usize;
        machine.bus.ram[offset..offset + 4].copy_from_slice(&0x0070_0313u32.to_le_bytes());
        let sys = machine.cpu.sys.as_mut().unwrap();
        sys.stvec = handler;
        sys.mideleg |= IRQ_SSIP;
        sys.mie |= IRQ_SSIP;
        sys.mip |= IRQ_SSIP;
        sys.mstatus |= 1 << 1; // SIE
        let mut dispatch = TestDispatch::default();

        machine.run_cached_slice_outcome(1, &mut dispatch, false);

        assert_eq!(machine.cpu.x[5], 0);
        assert_eq!(machine.cpu.x[6], 7);
        assert_eq!(machine.cpu.pc, handler + 4);
    }

    #[test]
    fn full_instruction_at_page_end_uses_precise_fetch_fallback() {
        let mut machine = direct_machine();
        let page_end = machine.cpu.pc + 0xffe;
        let offset = (KERNEL_OFFSET + 0xffe) as usize;
        machine.bus.ram[offset..offset + 4].copy_from_slice(&0x0070_0293u32.to_le_bytes());
        machine.cpu.pc = page_end;
        let mut dispatch = TestDispatch::default();

        let outcome = machine.run_cached_slice_outcome(1, &mut dispatch, false);

        assert_eq!(outcome.retired, 1);
        assert_eq!(machine.cpu.x[5], 7);
        assert_eq!(machine.cpu.pc, page_end + 4);
    }

    #[test]
    fn zero_retirement_trap_loop_exhausts_the_attempt_budget() {
        let mut machine = direct_machine();
        let start = machine.cpu.pc;
        write_program(&mut machine, &[0x0000_b283]); // ld x5,0(x1), with x1=0
        let sys = machine.cpu.sys.as_mut().unwrap();
        sys.medeleg |= 1 << 5;
        sys.stvec = start;
        let mut dispatch = TestDispatch::default();

        let outcome = machine.run_cached_slice_outcome(8, &mut dispatch, true);

        assert_eq!(outcome.retired, 0);
        assert_eq!(machine.cpu.pc, start);
        assert_eq!(machine.cpu.exc_counts[5], 8);
    }

    fn sbi(machine: &mut VirtMachine, ext: u64, function: u64, arg0: u64, arg1: u64) {
        machine.cpu.x[17] = ext;
        machine.cpu.x[16] = function;
        machine.cpu.x[10] = arg0;
        machine.cpu.x[11] = arg1;
        machine.service_sbi();
    }

    #[test]
    fn direct_sbi_remote_fence_i_invalidates_decoded_pages() {
        let mut machine = direct_machine();
        let generation = machine.cpu.icache_gen;

        sbi(&mut machine, 0x5246_4e43, 1, 0, 0);
        assert_eq!(machine.cpu.icache_gen, generation);
        sbi(&mut machine, 0x5246_4e43, 0, 0, 0);
        assert_eq!(machine.cpu.icache_gen, generation + 1);
        sbi(&mut machine, 5, 0, 0, 0);
        assert_eq!(machine.cpu.icache_gen, generation + 2);
    }

    #[test]
    fn direct_boot_enters_linux_in_supervisor_mode() {
        let machine = direct_machine();
        assert_eq!(machine.cpu.pc, RAM_BASE + KERNEL_OFFSET);
        assert_eq!(machine.cpu.x[10], 0);
        assert_ne!(machine.cpu.x[11], 0);
        let sys = machine.cpu.sys.as_ref().unwrap();
        assert_eq!(sys.mode, Mode::Supervisor);
        assert_eq!(sys.satp, 0);
        assert_eq!(sys.mideleg & (IRQ_SSIP | IRQ_STIP | IRQ_SEIP), 0x222);
    }

    #[test]
    fn direct_sbi_base_time_and_unknown_calls() {
        let mut machine = direct_machine();
        sbi(&mut machine, 0x10, 0, 0, 0);
        assert_eq!((machine.cpu.x[10], machine.cpu.x[11]), (0, 2 << 24));
        sbi(&mut machine, 0x10, 3, 0x5449_4d45, 0);
        assert_eq!((machine.cpu.x[10], machine.cpu.x[11]), (0, 1));
        machine.bus.mtime = 100;
        machine.cpu.sys.as_mut().unwrap().mip |= IRQ_STIP;
        sbi(&mut machine, 0x5449_4d45, 0, 1234, 0);
        assert_eq!(machine.bus.mtimecmp, 1234);
        assert_eq!(machine.cpu.sys.as_ref().unwrap().mip & IRQ_STIP, 0);
        sbi(&mut machine, 0x5449_4d45, 0, 99, 0);
        assert_ne!(machine.cpu.sys.as_ref().unwrap().mip & IRQ_STIP, 0);
        sbi(&mut machine, 0x48_53_4d, 2, 0, 0);
        assert_eq!((machine.cpu.x[10], machine.cpu.x[11]), (0, 0));
        sbi(&mut machine, 0x5246_4e43, 0, 1, 0);
        assert_eq!(machine.cpu.x[10], 0);
        sbi(&mut machine, 0xdead_beef, 7, 0, 0);
        assert_eq!(machine.cpu.x[10], (-2i64) as u64);
        assert_eq!(machine.unsupported_sbi, Some((0xdead_beef, 7)));
    }

    #[test]
    fn direct_sbi_reset_powers_off() {
        let mut machine = direct_machine();
        write_program(&mut machine, &[0x0000_0073]); // ecall
        machine.cpu.x[17] = 0x5352_5354;
        machine.cpu.x[16] = 0;
        machine.cpu.x[10] = 0;
        machine.run_slice_until(10, |_| false);
        assert!(machine.power_off);
        assert!(machine.bus.power_off);
    }

    #[test]
    fn run_slice_until_services_sbi_before_stopping_at_compiled_code() {
        let mut machine = direct_machine();
        write_program(&mut machine, &[0x0000_0073, 0x0012_8293]); // ecall; addi x5,x5,1
        machine.cpu.x[17] = 0x10;
        machine.cpu.x[16] = 0;

        let target = RAM_BASE + KERNEL_OFFSET + 4;
        let retired = machine.run_slice_until(10, |pc| pc == target);

        assert_eq!(retired, 1);
        assert_eq!(machine.cpu.pc, target);
        assert_eq!(machine.cpu.x[5], 0);
        assert_eq!(machine.cpu.x[11], 2 << 24);
        assert_eq!(machine.sbi_calls[0], 1);
    }

    #[test]
    fn run_slice_until_keeps_unsupported_sbi_stopped() {
        let mut machine = direct_machine();
        write_program(&mut machine, &[0x0000_0073]); // ecall
        machine.cpu.x[17] = 0xdead_beef;
        machine.cpu.x[16] = 7;

        machine.run_slice_until(10, |_| false);

        assert_eq!(machine.unsupported_sbi, Some((0xdead_beef, 7)));
        assert!(machine.power_off);
    }

    #[test]
    fn run_slice_until_fast_forwards_wfi_without_faking_retired_instructions() {
        let mut machine = direct_machine();
        write_program(&mut machine, &[0x1050_0073]); // wfi
        machine.bus.mtimecmp = 50;

        let retired = machine.run_slice_until(10, |_| false);

        assert_eq!(retired, 1);
        assert_eq!(machine.cpu.insn_count, 1);
        assert_eq!(machine.idle_ticks, 50);
        assert_eq!(machine.bus.mtime, 50);
    }

    #[test]
    fn realtime_wfi_reports_idle_to_the_embedding() {
        let mut machine = direct_machine();
        write_program(&mut machine, &[0x1050_0073]); // wfi
        machine.advance_realtime_ns(0);

        let outcome = machine.run_slice_until_outcome(10, |_| false);

        assert_eq!(outcome.retired, 1);
        assert!(outcome.idle);
        assert_eq!(machine.idle_ticks, 0);
    }

    #[test]
    fn realtime_sync_does_not_move_rdtime_backward() {
        let rdtime = |machine: &VirtMachine| {
            let sys = machine.cpu.sys.as_ref().unwrap();
            machine
                .cpu
                .insn_count
                .checked_div(sys.time_scale)
                .map_or(sys.mtime, |ticks| ticks + sys.time_offset)
        };
        let mut machine = direct_machine();
        machine.advance_realtime_ns(1_000_000);
        machine.sync_devices();

        machine.cpu.insn_count = 6_400;
        let before = rdtime(&machine);
        machine.sync_devices();
        let after = rdtime(&machine);

        assert_eq!(before, 10_064);
        assert_eq!(after, before);
        assert_eq!(machine.bus.mtime, before);
        assert_eq!(machine.cpu.sys.as_ref().unwrap().time_scale, 0);

        machine.cpu.insn_count = 12_800;
        machine.sync_devices();
        let frozen = machine.cpu.sys.as_ref().unwrap().mtime;
        assert_eq!(frozen, before, "speculative time must not accumulate");

        machine.advance_realtime_ns(640_000);
        machine.sync_devices();
        let caught_up = machine.cpu.sys.as_ref().unwrap();
        assert_eq!(caught_up.mtime, 16_400);
        assert_eq!(caught_up.time_scale, machine.insns_per_tick);
    }

    #[test]
    fn virt_bus_jit_state_observes_guest_stores_once() {
        let mut machine = direct_machine();
        let code = RAM_BASE + KERNEL_OFFSET;
        assert!(machine.bus.jit_fast_off(code, code, true).is_some());
        machine.bus.jit_mark_page(code);
        assert!(machine.bus.jit_fast_off(code, code, true).is_none());

        machine.bus.write32(code, 1).unwrap();
        machine.bus.write32(code + 4, 2).unwrap();

        assert!(machine.bus.jit_page_marked(KERNEL_OFFSET >> 12));
        assert!(machine.bus.jit_page_dirty(KERNEL_OFFSET >> 12));
        assert_eq!(machine.bus.jit_take_dirty(), vec![KERNEL_OFFSET >> 12]);
    }
}
