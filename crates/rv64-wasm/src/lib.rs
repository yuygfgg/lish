//! Wasm host ABI for the Lish full-system RISC-V VM.
//!
//! The module uses a plain `extern "C"` ABI. One Wasm instance owns one VM.

use rv64_core::{Bus, Cpu};

// ---- host imports (provided by web/rv64.js) -----------------------------

#[link(wasm_import_module = "env")]
extern "C" {
    /// Console output from the guest (fd 1 = stdout, 2 = stderr).
    fn host_write(fd: i32, ptr: *const u8, len: usize);
    /// Milliseconds since an arbitrary epoch (performance.now()).
    fn host_now_ms() -> f64;
    /// Milliseconds since the Unix epoch (Date.now()), for the guest RTC.
    fn host_unix_ms() -> f64;
    /// Queue a dead table slot for cleanup after the current Wasm entry
    /// returns to JavaScript. reason=1 identifies policy eviction.
    #[cfg(not(test))]
    fn host_jit_retire(idx: i32, reason: u32);
    /// One Ethernet frame the guest transmitted, for the page to forward over
    /// its WebSocket relay. Called at quantum granularity, like host_write.
    fn host_net_send(ptr: *const u8, len: usize);
    /// Compile the module in JIT_OUT asynchronously and reserve `slot_count`
    /// contiguous table entries. JS calls sys_jit_ready between VM slices
    /// calls after every export is installed (base -1/-2 = failure/capacity).
    fn host_jit_register_async(ticket: u64, slot_count: u32);
}

// Host callbacks are copied into a JavaScript queue and delivered only after
// the current Wasm entry returns. Mark every user-visible event at this single
// ABI boundary so long guest runs can yield before that queue grows without
// bound. JIT bookkeeping imports do not enter the application event queue.
static mut HOST_EVENT_QUEUED: bool = false;

#[inline]
fn begin_host_event_batch() {
    unsafe { HOST_EVENT_QUEUED = false }
}

#[inline]
fn take_host_event() -> bool {
    unsafe {
        let queued = HOST_EVENT_QUEUED;
        HOST_EVENT_QUEUED = false;
        queued
    }
}

#[inline]
fn emit_host_write(fd: i32, bytes: &[u8]) {
    unsafe {
        HOST_EVENT_QUEUED = true;
        host_write(fd, bytes.as_ptr(), bytes.len());
    }
}

#[inline]
fn emit_host_net(frame: &[u8]) {
    unsafe {
        HOST_EVENT_QUEUED = true;
        host_net_send(frame.as_ptr(), frame.len());
    }
}

#[inline]
fn emit_host_jit_retire(idx: i32, reason: u32) {
    #[cfg(not(test))]
    unsafe {
        host_jit_retire(idx, reason);
    }
    #[cfg(test)]
    let _ = (idx, reason);
}

// ---- shared staging buffer (JS -> wasm data transfer) --------------------

static mut STAGING: Vec<u8> = Vec::new();

/// Resize the staging buffer and return its pointer; JS copies data in.
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn staging_alloc(len: usize) -> *mut u8 {
    unsafe {
        STAGING.clear();
        STAGING.resize(len, 0);
        STAGING.as_mut_ptr()
    }
}

// ---- JIT dispatch state ---------------------------------------------------

#[derive(Clone, Copy)]
struct JitBlock {
    /// Trace touches the FP file (claim policy: see TRACE_KEEP_MIN).
    fp: bool,
    /// Function-table index of the compiled block.
    idx: i32,
    /// Guest instructions it retires.
    n: u32,
    /// Static ordinary-trace mix (ALU/load/store/control/FP).
    mix: [u16; 5],
    mem: [u16; 10],
    control: [u16; 3],
    alu: [u16; 5],
    /// Physical address of the code (full-system: verified per dispatch).
    pa: u64,
    /// Sampled recency stamp. Updating this on every dispatch would defeat the
    /// direct-mapped fast path, so dispatchers touch one block per 256 calls.
    last_used: u64,
}

type JitOwnerId = u64;

/// Direct-mapped dispatch line: `pc` is the full key. A slot with
/// `pc == NO_PC` is empty. Indexed by low pc bits — a single array read +
/// compare replaces the HashMap+SipHash lookup on the hot path. Deliberately
/// PACKED to 16 bytes: at 200M+ dispatches/s the line load is the loop's main
/// memory traffic, and 16B lines double how many fit in cache versus carrying
/// the whole JitBlock (pa/n live in `cache` and are only needed on the rare
/// verify path).
#[derive(Clone, Copy)]
#[repr(C)] // compiled blocks read lines directly: pc @0, idx @8, gen @12
struct DispatchLine {
    pc: u64,
    /// Function-table index (JitBlock.idx; < 0 = blacklisted sentinel).
    idx: i32,
    /// Low 32 bits of cpu.map_gen at last successful pa-verify. The fast path
    /// re-verifies via the authoritative cache only when the generation moved
    /// (SFENCE.VMA / satp write); a wraparound false-mismatch just re-probes.
    gen: u32,
}

const NO_PC: u64 = u64::MAX;
// 262144 lines: CPython's ~20k hot pcs collided at ~7% in 131072 under
// guest ASLR — per-boot slot-eviction churn is one suspected source of the
// python row's 3.3-6.7s bimodality. Doubling costs 2MB and no hot-path work.
const DISPATCH_BITS: u32 = 18;
const DISPATCH_SIZE: usize = 1 << DISPATCH_BITS;

#[derive(Clone, Copy)]
struct PendingSuperblockClaim {
    ticket: u64,
    prior: bool,
    physical_page: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PageModuleSource {
    physical_page: u64,
    generation: u64,
    tlb_fill: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LivePageModule {
    source: PageModuleSource,
    owner: JitOwnerId,
    entries: Box<[u64]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PageModuleState {
    Attempted(PageModuleSource),
    Pending {
        source: PageModuleSource,
        ticket: u64,
        prior: Option<LivePageModule>,
    },
    Live(LivePageModule),
}

impl PageModuleState {
    fn source(&self) -> PageModuleSource {
        match self {
            Self::Attempted(source) | Self::Pending { source, .. } => *source,
            Self::Live(live) => live.source,
        }
    }

    fn active_owner(&self) -> Option<JitOwnerId> {
        match self {
            Self::Live(live) => Some(live.owner),
            Self::Pending {
                prior: Some(live), ..
            } => Some(live.owner),
            Self::Attempted(_) | Self::Pending { prior: None, .. } => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum PageModuleDecision {
    Individual,
    Awaiting,
    Build(Option<LivePageModule>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PageModuleIssue {
    Individual,
    Awaiting,
    Issued,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PageModuleFinish {
    Failed,
    Cancelled,
    Landed(JitOwnerId),
}

struct JitState {
    /// pc -> compiled block; None = tried and not translatable (blacklist).
    /// Authoritative store (iterated for per-page invalidation).
    cache: std::collections::HashMap<u64, Option<JitBlock>>,
    /// Number of cache entries that reference each raw table index. Region
    /// functions have many references. Ordinary and batch members have one.
    slot_refs: std::collections::HashMap<i32, u32>,
    /// A compiled module can own one slot, a contiguous batch, or one region
    /// function. Policy eviction retires the complete owner as one unit.
    owner_slots: std::collections::HashMap<JitOwnerId, Vec<i32>>,
    slot_owner: std::collections::HashMap<i32, JitOwnerId>,
    /// Owner-local cache entries make retirement proportional to one module,
    /// not to the complete code cache. `owner_recency` and `cold_owners` are
    /// the two views of one LRU index and must be updated together.
    owner_entries: std::collections::HashMap<JitOwnerId, Vec<u64>>,
    owner_recency: std::collections::HashMap<JitOwnerId, u64>,
    cold_owners: std::collections::BTreeSet<(u64, JitOwnerId)>,
    /// Module owner -> address-space/page key for page-packed modules. The key
    /// is released with the last owned slot, which permits a clean rebuild
    /// after dirty-code invalidation or policy eviction.
    page_module_owner: std::collections::HashMap<JitOwnerId, (u64, u64)>,
    hot: std::collections::HashMap<u64, u32>,
    /// Fast dispatch cache: direct-mapped, populated lazily from `cache`.
    dispatch: Vec<DispatchLine>,
    /// Conservative filter for compiled entry PCs. Decoded-block lookahead
    /// avoids a HashMap miss for the common uncompiled instruction; stale bits
    /// after eviction are harmless and are cleared with the whole JIT state.
    entry_filter: Vec<u64>,
    /// Last observed cpu.jit_flush_gen; a change means the va→pa code
    /// mapping was invalidated (satp/SFENCE) — drop everything.
    flush_gen: u64,
    /// Superblock compilation: the hot block-entry
    /// pcs discovered in each guest code page (keyed by virtual page base). When
    /// a new entry appears the page's superblock is recompiled to cover it, and
    /// every entry's `cache`/`dispatch` slot points at the one superblock.
    page_entries: std::collections::HashMap<(u64, u64), Vec<u64>>,
    /// Actually hot entry PCs, kept separate from superblock reachability
    /// seeds. Page-packed modules use these roots to discover one stable set of
    /// independent bodies instead of creating another batch at every tier-up.
    page_hot_entries: std::collections::HashMap<(u64, u64), Vec<u64>>,
    /// Fully translated hot traces waiting for one shared module wrapper.
    /// Staging is global because module packaging does not require code or
    /// control-flow locality; each body keeps its own source-page proof.
    confirmed_stage: Vec<StagedBlock>,
    /// Exact PCs covered by staged or asynchronously compiling confirmed
    /// traces. Reference counts keep overlapping traces independent.
    confirmed_coverage: std::collections::HashMap<(u64, u64), u16>,
    /// One lifecycle record per address-space/code-page generation. A failed
    /// translation cannot retry until the source generation changes. Transient
    /// cancellations restore the prior live owner or remove the pending plan.
    page_modules: std::collections::HashMap<(u64, u64), PageModuleState>,
    /// Cheap direct-mapped hot counter for decoded interpreter block entries.
    /// A real HashMap update on every block taxes cold boot, so only promote an
    /// entry into `hot` when its compact counter reaches the tier-up threshold.
    interp_hot: Vec<u16>,
    /// Full-pc tag for each interp_hot slot (low 32 bits of pc>>1). Untagged
    /// direct-mapped counters let unrelated pcs (or another address space)
    /// inherit a slot's heat and get compiled on their first execution —
    /// compile storms that depend on address layout (PERFORMANCE_PROGRESS.md).
    interp_hot_tag: Vec<u32>,
    /// Physical page -> pcs of cache entries whose code lives there (blocks
    /// AND pa-stamped blacklist sentinels). Lets dirty-page invalidation drop
    /// exactly the affected entries instead of scanning the whole cache per
    /// page — the cache persists across context switches now, so it's large.
    page_blocks: std::collections::HashMap<u64, Vec<u64>>,
    /// Virtual pages already compiled as a superblock — compile ONCE per page
    /// (with whatever entries were hot then); later hot pcs in the page get
    /// individual blocks. Recompiling the page's big br_table function on every
    /// new entry was a 2x regression on short workloads (the recompile storm).
    superblocked: std::collections::HashSet<(u64, u64)>,
    /// Latest asynchronous build that owns each virtual-page claim. Each claim
    /// keeps the original claimed state for rollback and its physical source
    /// page so DMA invalidation can cancel a build before it lands.
    pending_superblocks: std::collections::HashMap<(u64, u64), PendingSuperblockClaim>,
    /// Virtual page -> (hot entries at its last superblock compile, number of
    /// UNPRODUCTIVE compiles — rebuilds that covered no new hot pcs). A page's first superblock is built from whatever handful of
    /// pcs was hot at the threshold; code discovered later — a second function
    /// in the page, a callee reached only by an indirect call — would stay on
    /// individual blocks forever (measured: nbench IDEA ran cipher_idea as 1-15
    /// instruction blocks, 6.4 insns per dispatch, on a page that WAS
    /// superblocked). Recompile once the page has accumulated another
    /// threshold's worth of uncovered hot pcs, bounded so a pathological page
    /// can't loop on it.
    sb_gen: std::collections::HashMap<(u64, u64), (usize, u32, u64)>,
    /// Pages that wanted a superblock while the compile budget was spent.
    /// Drained one per quantum boundary, oldest first.
    sb_queue: Vec<(u64, u64)>,
    /// Hot pcs on a superblocked page that had to get their own block because
    /// the page function does not cover them — the direct measure of a page
    /// function that has fallen behind the code actually running.
    sb_missed: std::collections::HashMap<(u64, u64), u32>,
    /// Observed successor of each dispatched pc, direct-mapped by dispatch
    /// slot (pc, next_pc). This is the NEXT-EXECUTING-TAIL signal that
    /// trace-tree JITs form regions from: batches built from STATIC exit
    /// seeds only kept ~12% of exits in-batch, because a trace's textual
    /// successors are not the ones execution actually takes.
    succ: Vec<(u64, u64, u32)>,
    /// Repeated zero-retire entries which are not explained by the FP gate.
    /// These are overwhelmingly first-memory-operation TLB misses. The direct
    /// map keeps the signal cheap and identifies the few blocks worth
    /// recompiling after the adaptive refill policy turns on.
    tlb_bails: Vec<(u64, u8)>,
    tlb_bail_total: u64,
    tlb_auto_enabled: bool,
    /// Entry pcs already recompiled once with an inline cache — trace
    /// EXTENSION happens at most once per pc, so a flapping successor
    /// cannot loop the compiler.
    ic_done: std::collections::HashSet<u64>,
    /// Table index -> the (virtual page, physical page) list a MULTI-page
    /// superblock was compiled over. Entries carry their own page's pa (probed
    /// like any block at dispatch); this is the rest of the region, verified on
    /// the same slow path so a region can never execute against a page that was
    /// remapped out from under it.
    regions: std::collections::HashMap<i32, Vec<(u64, u64)>>,
    /// Table index -> live exit profile of a landed region function: which
    /// pages its (sampled) exits transfer control to. This is the measured
    /// signal that drives incremental region EXTENSION — a region grows only
    /// along traffic it demonstrably loses dispatches to, never from
    /// reachability guesses (which glued cold callees into hot regions and
    /// regressed the FP rows; see build_superblock).
    region_exits: std::collections::HashMap<i32, RegionExits>,
    /// Regions whose sampled out-of-region exit count crossed EXT_TRIGGER,
    /// awaiting a build slot at a quantum boundary.
    ext_queue: Vec<i32>,
}

/// Sampled exit profile of one landed region function (JitState::region_exits).
struct RegionExits {
    /// satp the region was discovered in.
    aspace: u64,
    /// The page whose threshold crossing originally built this region — the
    /// stable identity that keys build cooldowns across rebuilds/extensions.
    lead: u64,
    /// (virtual page, physical page) the function was compiled over,
    /// ascending. Extension reuses these RECORDED pas rather than re-probing:
    /// the build slot fires at an arbitrary guest moment (usually inside the
    /// kernel), where a fetch probe of a user va fails on privilege — the
    /// same trap that once dropped 96% of finished page functions in
    /// sys_sb_ready. Landing validation plus first-dispatch pa-verify carry
    /// the correctness burden, exactly as for every other region install.
    pages: Vec<(u64, u64)>,
    /// Sampled exits to a page OUTSIDE the region since landing.
    total: u32,
    /// Those exits per target page, first-come bounded (EXT_TARGET_CAP).
    targets: Vec<(u64, u32)>,
    /// Every sampled exit (in- or out-of-region), and the instructions those
    /// stays retired — the measured average stay length that picks the
    /// extension's register mode (locals for long stays, memory for short)
    /// and triggers DEMOTION when the function demonstrably doesn't hold.
    samples: u32,
    stay_sum: u64,
    /// The entry pcs installed at landing (needed to un-claim on demotion).
    entries: Vec<u64>,
}

impl JitState {
    fn new() -> JitState {
        JitState {
            cache: Default::default(),
            slot_refs: Default::default(),
            owner_slots: Default::default(),
            slot_owner: Default::default(),
            owner_entries: Default::default(),
            owner_recency: Default::default(),
            cold_owners: Default::default(),
            page_module_owner: Default::default(),
            hot: Default::default(),
            dispatch: vec![
                DispatchLine {
                    pc: NO_PC,
                    idx: 0,
                    gen: 0,
                };
                DISPATCH_SIZE
            ],
            entry_filter: vec![0; DISPATCH_SIZE / 64],
            flush_gen: 0,
            page_entries: Default::default(),
            page_hot_entries: Default::default(),
            confirmed_stage: Vec::new(),
            confirmed_coverage: Default::default(),
            page_modules: Default::default(),
            succ: vec![(NO_PC, 0, 0); DISPATCH_SIZE],
            tlb_bails: vec![(NO_PC, 0); DISPATCH_SIZE],
            tlb_bail_total: 0,
            tlb_auto_enabled: false,
            ic_done: Default::default(),
            interp_hot: vec![0; DISPATCH_SIZE],
            interp_hot_tag: vec![0; DISPATCH_SIZE],
            page_blocks: Default::default(),
            superblocked: Default::default(),
            pending_superblocks: Default::default(),
            regions: Default::default(),
            region_exits: Default::default(),
            ext_queue: Vec::new(),
            sb_gen: Default::default(),
            sb_queue: Vec::new(),
            sb_missed: Default::default(),
        }
    }
    fn clear(&mut self) {
        let slots: Vec<i32> = self.slot_owner.keys().copied().collect();
        for idx in slots {
            retire_table_slot(idx, false);
        }
        self.cache.clear();
        self.slot_refs.clear();
        self.owner_slots.clear();
        self.slot_owner.clear();
        self.owner_entries.clear();
        self.owner_recency.clear();
        self.cold_owners.clear();
        self.page_module_owner.clear();
        self.hot.clear();
        self.page_entries.clear();
        self.page_hot_entries.clear();
        self.confirmed_stage.clear();
        self.confirmed_coverage.clear();
        self.page_modules.clear();
        self.superblocked.clear();
        self.pending_superblocks.clear();
        self.regions.clear();
        self.region_exits.clear();
        self.ext_queue.clear();
        self.sb_gen.clear();
        self.sb_queue.clear();
        self.sb_missed.clear();
        for h in self.interp_hot.iter_mut() {
            *h = 0;
        }
        for t in self.interp_hot_tag.iter_mut() {
            *t = 0;
        }
        for e in self.succ.iter_mut() {
            *e = (NO_PC, 0, 0);
        }
        for e in self.tlb_bails.iter_mut() {
            *e = (NO_PC, 0);
        }
        self.tlb_bail_total = 0;
        self.tlb_auto_enabled = false;
        reset_tlb_fill_policy();
        self.ic_done.clear();
        self.page_blocks.clear();
        self.entry_filter.fill(0);
        self.clear_dispatch();
    }

    fn claim_pending_superblock(&mut self, ticket: u64, aspace: u64, pages: &[(u64, u64)]) {
        for &(va, physical) in pages {
            let key = (aspace, va);
            let prior = self
                .pending_superblocks
                .get(&key)
                .map_or_else(|| self.superblocked.contains(&key), |claim| claim.prior);
            let physical_page = physical
                .checked_sub(rv64_system::RAM_BASE)
                .expect("pending JIT page must belong to guest RAM")
                >> 12;
            self.pending_superblocks.insert(
                key,
                PendingSuperblockClaim {
                    ticket,
                    prior,
                    physical_page,
                },
            );
            self.superblocked.insert(key);
        }
    }

    fn pending_superblock_is_current(
        &self,
        ticket: u64,
        aspace: u64,
        pages: &[(u64, u64)],
    ) -> bool {
        pages.iter().all(|&(va, _)| {
            self.pending_superblocks
                .get(&(aspace, va))
                .is_some_and(|claim| claim.ticket == ticket)
        })
    }

    fn tracked_page_keys_for_physical(&self, physical_page: u64) -> Vec<(u64, u64)> {
        let mut keys: Vec<_> = self
            .pending_superblocks
            .iter()
            .filter_map(|(&key, claim)| (claim.physical_page == physical_page).then_some(key))
            .collect();
        keys.extend(self.page_modules.iter().filter_map(|(&key, state)| {
            (state.source().physical_page == physical_page).then_some(key)
        }));
        keys.sort_unstable();
        keys.dedup();
        keys
    }

    fn record_page_hot_entry(&mut self, aspace: u64, pc: u64) {
        let entries = self
            .page_hot_entries
            .entry((aspace, pc & !0xfff))
            .or_default();
        if let Err(index) = entries.binary_search(&pc) {
            entries.insert(index, pc);
        }
    }

    fn invalidate_superblock_state(
        &mut self,
        exact_pages: &std::collections::HashSet<(u64, u64)>,
        broad_virtual_pages: &std::collections::HashSet<u64>,
    ) {
        let keep_legacy =
            |key: &(u64, u64)| !exact_pages.contains(key) && !broad_virtual_pages.contains(&key.1);
        let retained_page_modules: std::collections::HashSet<_> = self
            .page_modules
            .keys()
            .filter(|key| !exact_pages.contains(key))
            .copied()
            .collect();
        let owners: Vec<_> = exact_pages
            .iter()
            .filter_map(|key| {
                self.page_modules
                    .get(key)
                    .and_then(PageModuleState::active_owner)
            })
            .collect();
        for owner in owners {
            self.retire_owner(owner, false);
        }

        self.page_entries.retain(|key, _| keep_legacy(key));
        self.page_hot_entries.retain(|key, _| {
            !exact_pages.contains(key)
                && (!broad_virtual_pages.contains(&key.1) || retained_page_modules.contains(key))
        });
        self.page_modules
            .retain(|key, _| !exact_pages.contains(key));
        self.superblocked.retain(keep_legacy);
        self.pending_superblocks.retain(|key, _| keep_legacy(key));
        self.sb_gen.retain(|key, _| keep_legacy(key));
    }

    /// Finish only page claims still owned by `ticket`. A newer overlapping
    /// build keeps its claims. Failed builds restore the state that existed
    /// before this chain of pending replacements began.
    fn finish_pending_superblock(
        &mut self,
        ticket: u64,
        aspace: u64,
        pages: &[(u64, u64)],
        landed: bool,
    ) {
        for &(va, _) in pages {
            let key = (aspace, va);
            let Some(claim) = self.pending_superblocks.get(&key).copied() else {
                continue;
            };
            if claim.ticket != ticket {
                continue;
            }
            self.pending_superblocks.remove(&key);
            if !landed && !claim.prior {
                self.superblocked.remove(&key);
            }
        }
    }

    fn page_module_pending_is_current(&self, ticket: u64, batch: &PendingBatch) -> bool {
        let Some(key) = batch.page_module else {
            return true;
        };
        matches!(
            self.page_modules.get(&key),
            Some(PageModuleState::Pending { ticket: current, .. }) if *current == ticket
        )
    }

    fn page_module_decision(
        &self,
        key: (u64, u64),
        source: PageModuleSource,
    ) -> PageModuleDecision {
        let Some(state) = self.page_modules.get(&key) else {
            return PageModuleDecision::Build(None);
        };
        match state {
            PageModuleState::Attempted(attempted) if *attempted == source => {
                PageModuleDecision::Individual
            }
            PageModuleState::Attempted(_) => PageModuleDecision::Build(None),
            PageModuleState::Pending { .. } => PageModuleDecision::Awaiting,
            PageModuleState::Live(live) if live.source != source => {
                PageModuleDecision::Build(Some(live.clone()))
            }
            PageModuleState::Live(_) => PageModuleDecision::Individual,
        }
    }

    fn record_page_module_failure(
        &mut self,
        key: (u64, u64),
        source: PageModuleSource,
        prior: Option<LivePageModule>,
    ) {
        if let Some(live) = prior {
            self.page_modules.insert(key, PageModuleState::Live(live));
        } else {
            self.page_modules
                .insert(key, PageModuleState::Attempted(source));
        }
    }

    fn finish_pending_page_module(
        &mut self,
        ticket: u64,
        batch: &PendingBatch,
        finish: PageModuleFinish,
    ) -> Option<JitOwnerId> {
        let key = batch.page_module?;
        let Some(PageModuleState::Pending {
            source,
            ticket: current,
            prior,
        }) = self.page_modules.get(&key).cloned()
        else {
            return None;
        };
        if current != ticket {
            return None;
        }
        match finish {
            PageModuleFinish::Failed => {
                self.record_page_module_failure(key, source, prior);
                None
            }
            PageModuleFinish::Cancelled => {
                if let Some(live) = prior {
                    self.page_modules.insert(key, PageModuleState::Live(live));
                } else {
                    self.page_modules.remove(&key);
                }
                None
            }
            PageModuleFinish::Landed(owner) => {
                let mut entries: Vec<_> = batch.members.iter().map(|member| member.pc).collect();
                entries.sort_unstable();
                entries.dedup();
                self.page_modules.insert(
                    key,
                    PageModuleState::Live(LivePageModule {
                        source,
                        owner,
                        entries: entries.into_boxed_slice(),
                    }),
                );
                self.page_module_owner.insert(owner, key);
                prior.map(|live| live.owner)
            }
        }
    }

    fn page_module_owns_cached_entry(&self, aspace: u64, pc: u64) -> bool {
        let key = (aspace, pc & !0xfff);
        let Some(owner) = self
            .page_modules
            .get(&key)
            .and_then(PageModuleState::active_owner)
        else {
            return false;
        };
        matches!(
            self.cache.get(&pc),
            Some(Some(block))
                if block.idx >= 0 && self.slot_owner.get(&block.idx) == Some(&owner)
        )
    }

    fn owner_for_slot(&self, index: i32) -> Option<JitOwnerId> {
        self.slot_owner.get(&index).copied()
    }

    fn update_owner_recency(&mut self, owner: JitOwnerId, stamp: u64) {
        if self
            .owner_recency
            .get(&owner)
            .is_some_and(|&current| current >= stamp)
        {
            return;
        }
        if let Some(previous) = self.owner_recency.insert(owner, stamp) {
            self.cold_owners.remove(&(previous, owner));
        }
        self.cold_owners.insert((stamp, owner));
    }

    fn add_owner_entry(&mut self, owner: JitOwnerId, pc: u64) {
        let entries = self.owner_entries.entry(owner).or_default();
        if !entries.contains(&pc) {
            entries.push(pc);
        }
    }

    fn remove_owner_entry(&mut self, owner: JitOwnerId, pc: u64) {
        let mut empty = false;
        if let Some(entries) = self.owner_entries.get_mut(&owner) {
            entries.retain(|&entry| entry != pc);
            empty = entries.is_empty();
        }
        if empty {
            self.owner_entries.remove(&owner);
        }
    }

    fn forget_owner(&mut self, owner: JitOwnerId) {
        self.owner_slots.remove(&owner);
        self.owner_entries.remove(&owner);
        if let Some(stamp) = self.owner_recency.remove(&owner) {
            self.cold_owners.remove(&(stamp, owner));
        }
        if let Some(key) = self.page_module_owner.remove(&owner) {
            let remove_state = match self.page_modules.get_mut(&key) {
                Some(PageModuleState::Live(live)) => live.owner == owner,
                Some(PageModuleState::Pending { prior, .. }) => {
                    if prior.as_ref().is_some_and(|live| live.owner == owner) {
                        *prior = None;
                    }
                    false
                }
                Some(PageModuleState::Attempted(_)) | None => false,
            };
            if remove_state {
                self.page_modules.remove(&key);
            }
        }
    }

    fn track_owner(
        &mut self,
        owner: JitOwnerId,
        slots: impl IntoIterator<Item = i32>,
    ) -> Option<JitOwnerId> {
        let slots: Vec<i32> = slots.into_iter().filter(|&idx| idx >= 0).collect();
        if slots.is_empty() {
            return None;
        }
        assert!(
            !self.owner_slots.contains_key(&owner),
            "asynchronous JIT ticket reused as a live owner"
        );
        for &idx in &slots {
            register_table_slot(idx);
            assert!(
                self.slot_owner.insert(idx, owner).is_none(),
                "live JIT table slot assigned to two owners"
            );
        }
        self.owner_slots.insert(owner, slots);
        self.update_owner_recency(owner, next_jit_use_stamp());
        Some(owner)
    }

    fn cache_insert(&mut self, pc: u64, entry: Option<JitBlock>) -> Option<Option<JitBlock>> {
        if entry.is_some_and(|block| block.idx >= 0) {
            let slot = Self::dslot(pc);
            self.entry_filter[slot / 64] |= 1 << (slot % 64);
        }
        let previous = self.cache.insert(pc, entry);
        let old_idx = previous
            .flatten()
            .map(|block| block.idx)
            .filter(|&idx| idx >= 0);
        let new_idx = entry.map(|block| block.idx).filter(|&idx| idx >= 0);
        let old_owner = old_idx.map(|idx| {
            self.owner_for_slot(idx)
                .expect("cached JIT slot is missing its owner")
        });
        let new_owner = new_idx.map(|idx| {
            self.owner_for_slot(idx)
                .expect("new JIT slot is missing its owner")
        });
        if let Some(Some(block)) = previous {
            self.unindex_block(pc, block);
        }
        if let Some(block) = entry {
            self.index_block(pc, block);
        }
        if old_owner != new_owner {
            if let Some(owner) = old_owner {
                self.remove_owner_entry(owner, pc);
            }
            if let Some(owner) = new_owner {
                self.add_owner_entry(owner, pc);
            }
        }
        if let (Some(owner), Some(block)) = (new_owner, entry) {
            self.update_owner_recency(owner, block.last_used);
        }
        if old_idx != new_idx {
            if let Some(idx) = new_idx {
                *self.slot_refs.entry(idx).or_insert(0) += 1;
            }
            if let Some(idx) = old_idx {
                self.release_slot(idx, false);
            }
        }
        previous
    }

    fn cache_remove(&mut self, pc: &u64) -> Option<Option<JitBlock>> {
        self.cache_remove_with_reason(pc, false)
    }

    fn cache_remove_with_reason(&mut self, pc: &u64, evicted: bool) -> Option<Option<JitBlock>> {
        let previous = self.cache.remove(pc);
        if let Some(Some(block)) = previous {
            self.unindex_block(*pc, block);
            if block.idx >= 0 {
                let owner = self
                    .owner_for_slot(block.idx)
                    .expect("cached JIT slot is missing its owner");
                self.remove_owner_entry(owner, *pc);
                self.release_slot(block.idx, evicted);
            }
        }
        previous
    }

    fn block_physical_pages(&self, block: JitBlock) -> Vec<u64> {
        if let Some(pages) = self.regions.get(&block.idx) {
            return pages
                .iter()
                .map(|&(_, physical)| (physical - rv64_system::RAM_BASE) >> 12)
                .collect();
        }
        if block.pa >= rv64_system::RAM_BASE {
            vec![(block.pa - rv64_system::RAM_BASE) >> 12]
        } else {
            Vec::new()
        }
    }

    fn index_block(&mut self, pc: u64, block: JitBlock) {
        for page in self.block_physical_pages(block) {
            let pcs = self.page_blocks.entry(page).or_default();
            if !pcs.contains(&pc) {
                pcs.push(pc);
            }
        }
    }

    fn unindex_block(&mut self, pc: u64, block: JitBlock) {
        for page in self.block_physical_pages(block) {
            let mut empty = false;
            if let Some(pcs) = self.page_blocks.get_mut(&page) {
                pcs.retain(|&entry| entry != pc);
                empty = pcs.is_empty();
            }
            if empty {
                self.page_blocks.remove(&page);
            }
        }
    }

    fn release_slot(&mut self, idx: i32, evicted: bool) {
        let Some(refs) = self.slot_refs.get_mut(&idx) else {
            return;
        };
        *refs -= 1;
        if *refs != 0 {
            return;
        }
        self.slot_refs.remove(&idx);
        self.retire_owned_slot(idx, evicted);
    }

    fn retire_owned_slot(&mut self, idx: i32, evicted: bool) {
        let Some(owner) = self.slot_owner.remove(&idx) else {
            return;
        };
        let mut remove_owner = false;
        if let Some(slots) = self.owner_slots.get_mut(&owner) {
            slots.retain(|&slot| slot != idx);
            remove_owner = slots.is_empty();
        }
        if remove_owner {
            self.forget_owner(owner);
        }
        self.regions.remove(&idx);
        self.region_exits.remove(&idx);
        self.ext_queue.retain(|&slot| slot != idx);
        retire_table_slot(idx, evicted);
    }

    fn retire_unreferenced_slots(&mut self, owner: JitOwnerId) {
        let slots = self.owner_slots.get(&owner).cloned().unwrap_or_default();
        for idx in slots {
            if !self.slot_refs.contains_key(&idx) {
                self.retire_owned_slot(idx, false);
            }
        }
    }

    fn touch(&mut self, pc: u64) {
        let stamp = next_jit_use_stamp();
        let Some(index) = self.cache.get_mut(&pc).and_then(|entry| {
            let block = entry.as_mut()?;
            block.last_used = stamp;
            Some(block.idx)
        }) else {
            return;
        };
        if let Some(owner) = self.owner_for_slot(index) {
            self.update_owner_recency(owner, stamp);
        }
    }

    fn cool_evicted_entry(&mut self, pc: u64) {
        self.hot.remove(&pc);
        let slot = Self::dslot(pc);
        if self.interp_hot_tag[slot] == (pc >> 1) as u32 {
            self.interp_hot[slot] = 0;
        }
        unsafe { JIT_EVICTION_COOLED_ENTRIES += 1 };
    }

    fn is_page_module_slot(&self, index: i32) -> bool {
        self.slot_owner
            .get(&index)
            .is_some_and(|owner| self.page_module_owner.contains_key(owner))
    }

    fn tagged_dispatch_index(&self, block: JitBlock) -> i32 {
        let mut index = if block.n == 0 && block.idx >= 0 {
            block.idx | SB_IDX_BIT
        } else {
            block.idx
        };
        if block.idx >= 0 && self.is_page_module_slot(block.idx) {
            index |= PAGE_IDX_BIT;
        }
        index
    }

    /// Retire one complete generated module and every cache entry which points
    /// into it. Page routers require this atomic unit: removing one PC would
    /// leave other entries able to call private code from the old page image.
    fn retire_owner(&mut self, owner: JitOwnerId, evicted: bool) -> usize {
        let slots = self.owner_slots.get(&owner).cloned().unwrap_or_default();
        let pcs = self.owner_entries.remove(&owner).unwrap_or_default();
        for pc in pcs {
            if evicted {
                self.cool_evicted_entry(pc);
            }
            let dispatch = Self::dslot(pc);
            if self.dispatch[dispatch].pc == pc {
                self.dispatch[dispatch].pc = NO_PC;
            }
            self.cache_remove_with_reason(&pc, evicted);
        }
        // A batch export can be unreferenced because a newer landing won the
        // cache race. It still belongs to this owner and must be released.
        for idx in slots.iter().copied() {
            if !self.slot_refs.contains_key(&idx) {
                self.retire_owned_slot(idx, evicted);
            }
        }
        if slots.is_empty() {
            self.forget_owner(owner);
        }
        slots.len()
    }

    fn retire_page_module_for_slot(&mut self, index: i32) -> bool {
        let Some(owner) = self.slot_owner.get(&index).copied() else {
            return false;
        };
        if !self.page_module_owner.contains_key(&owner) {
            return false;
        }
        self.retire_owner(owner, false);
        true
    }

    /// Evict the coldest module owner without scanning the complete code cache.
    fn evict_cold_owner(&mut self) -> usize {
        let Some(&(_, owner)) = self.cold_owners.first() else {
            return 0;
        };
        let retired = self.retire_owner(owner, true);
        unsafe {
            JIT_EVICTED_OWNERS += 1;
            JIT_EVICTED_SLOTS += retired as u64;
        }
        retired
    }
    #[inline]
    fn clear_dispatch(&mut self) {
        for l in self.dispatch.iter_mut() {
            l.pc = NO_PC;
        }
    }
    #[inline]
    fn dslot(pc: u64) -> usize {
        ((pc >> 1) as usize) & (DISPATCH_SIZE - 1)
    }

    #[inline]
    fn may_have_compiled(&self, pc: u64) -> bool {
        let slot = Self::dslot(pc);
        self.entry_filter[slot / 64] & (1 << (slot % 64)) != 0
    }

    fn add_confirmed_coverage(&mut self, block: &PendingBlock) {
        for &pc in &block.trace_pcs {
            let refs = self
                .confirmed_coverage
                .entry((block.aspace, pc))
                .or_insert(0);
            *refs = refs.saturating_add(1);
        }
    }

    fn remove_confirmed_coverage(&mut self, block: &PendingBlock) {
        for &pc in &block.trace_pcs {
            let key = (block.aspace, pc);
            if let std::collections::hash_map::Entry::Occupied(mut entry) =
                self.confirmed_coverage.entry(key)
            {
                if *entry.get() <= 1 {
                    entry.remove();
                } else {
                    *entry.get_mut() -= 1;
                }
            }
        }
    }

    fn remove_pending_coverage(&mut self, kind: &PendingJitKind) {
        match kind {
            PendingJitKind::Block(block) => self.remove_confirmed_coverage(block),
            PendingJitKind::Batch(batch) => {
                for block in &batch.members {
                    self.remove_confirmed_coverage(block);
                }
            }
            PendingJitKind::Region(_) => {}
        }
    }
}

struct InterpreterDispatch<'a> {
    jit: &'a mut JitState,
    tier_up: bool,
    aspace: u64,
}

impl InterpreterDispatch<'_> {
    #[inline]
    fn compiled(&self, pc: u64) -> bool {
        if !self.jit.may_have_compiled(pc) {
            return false;
        }
        matches!(self.jit.cache.get(&pc), Some(Some(block)) if block.idx >= 0)
    }
}

impl rv64_system::CodeDispatch for InterpreterDispatch<'_> {
    #[inline]
    fn contains(&self, pc: u64) -> bool {
        self.compiled(pc)
    }

    #[inline]
    fn observe(&mut self, pc: u64) -> bool {
        if self.compiled(pc) {
            return true;
        }
        if !self.tier_up {
            return false;
        }
        if self.jit.confirmed_coverage.contains_key(&(self.aspace, pc)) {
            return false;
        }

        let slot = JitState::dslot(pc);
        let tag = (pc >> 1) as u32;
        if self.jit.interp_hot_tag[slot] != tag {
            self.jit.interp_hot_tag[slot] = tag;
            self.jit.interp_hot[slot] = 0;
        }
        let (warm, tiered) = {
            let count = &mut self.jit.interp_hot[slot];
            *count = count.saturating_add(1);
            (
                usize::from(*count) == PAGE_MODULE_WARM_THRESHOLD,
                u32::from(*count) >= unsafe { JIT_THRESHOLD },
            )
        };
        if warm && unsafe { PAGE_MODULES_ON } {
            self.jit.record_page_hot_entry(self.aspace, pc);
        }
        if !tiered {
            return false;
        }
        self.jit.hot.insert(pc, unsafe { JIT_THRESHOLD });
        true
    }
}

#[cfg(test)]
mod jit_state_tests {
    use super::{
        account_compiled_dispatch, block_should_replace_region, JitBlock, JitState, LivePageModule,
        PageModuleDecision, PageModuleFinish, PageModuleSource, PageModuleState, PendingBatch,
        PendingBlock, PAGE_IDX_BIT,
    };
    use std::collections::HashSet;

    fn page_source(physical_page: u64) -> PageModuleSource {
        PageModuleSource {
            physical_page,
            generation: 3,
            tlb_fill: false,
        }
    }

    fn compiled_block(idx: i32, pa: u64) -> JitBlock {
        JitBlock {
            fp: false,
            idx,
            n: 4,
            mix: [0; 5],
            mem: [0; 10],
            control: [0; 3],
            alu: [0; 5],
            pa,
            last_used: 0,
        }
    }

    fn pending_page_batch(key: (u64, u64), pc: u64) -> PendingBatch {
        PendingBatch {
            cell: 0,
            sequence: 0,
            members: vec![PendingBlock {
                aspace: key.0,
                pc,
                block: compiled_block(-1, 0x8000_0000 + (pc & 0xfff)),
                pages: vec![(key.1, 0x8000_0000)],
                page_generations: vec![3],
                seeds: Vec::new(),
                trace_pcs: Vec::new(),
                missed_superblock: false,
            }],
            page_module: Some(key),
        }
    }

    #[test]
    fn latest_pending_superblock_owns_overlapping_page_claims() {
        let mut jit = JitState::new();
        let a = (0x1000, 0x8000_1000);
        let b = (0x2000, 0x8000_2000);
        let c = (0x3000, 0x8000_3000);

        jit.claim_pending_superblock(1, 7, &[a, b]);
        jit.claim_pending_superblock(2, 7, &[b, c]);

        assert!(!jit.pending_superblock_is_current(1, 7, &[a, b]));
        assert!(jit.pending_superblock_is_current(2, 7, &[b, c]));
        assert_eq!(jit.tracked_page_keys_for_physical(1), vec![(7, a.0)]);
        assert_eq!(jit.tracked_page_keys_for_physical(2), vec![(7, b.0)]);
        jit.finish_pending_superblock(1, 7, &[a, b], false);
        assert!(!jit.superblocked.contains(&(7, a.0)));
        assert!(jit.superblocked.contains(&(7, b.0)));

        jit.finish_pending_superblock(2, 7, &[b, c], false);
        assert!(!jit.superblocked.contains(&(7, b.0)));
        assert!(!jit.superblocked.contains(&(7, c.0)));

        jit.superblocked.insert((7, a.0));
        jit.claim_pending_superblock(3, 7, &[a]);
        jit.claim_pending_superblock(4, 7, &[a]);
        jit.finish_pending_superblock(4, 7, &[a], false);
        assert!(jit.superblocked.contains(&(7, a.0)));
    }

    #[test]
    fn failed_page_module_reopens_only_for_a_new_source() {
        let mut jit = JitState::new();
        let key = (7, 0x4000);
        let source = page_source(1);
        jit.page_modules
            .insert(key, PageModuleState::Attempted(source));

        assert_eq!(
            jit.page_module_decision(key, source),
            PageModuleDecision::Individual
        );
        let changed = PageModuleSource {
            generation: source.generation + 1,
            ..source
        };
        assert_eq!(
            jit.page_module_decision(key, changed),
            PageModuleDecision::Build(None)
        );
    }

    #[test]
    fn pending_page_module_defers_same_page_individual_work() {
        let mut jit = JitState::new();
        let key = (7, 0x4000);
        let source = page_source(1);
        jit.page_modules.insert(
            key,
            PageModuleState::Pending {
                source,
                ticket: 9,
                prior: None,
            },
        );

        assert_eq!(
            jit.page_module_decision(key, source),
            PageModuleDecision::Awaiting
        );
    }

    #[test]
    fn page_module_priority_is_scoped_to_address_space() {
        let mut jit = JitState::new();
        let pc = 0x4120;
        let key = (7, 0x4000);
        let owner = 21_u64;
        let slot = 21;
        jit.owner_slots.insert(owner, vec![slot]);
        jit.slot_owner.insert(slot, owner);
        jit.page_module_owner.insert(owner, key);
        jit.page_modules.insert(
            key,
            PageModuleState::Live(LivePageModule {
                source: page_source(1),
                owner,
                entries: vec![pc].into_boxed_slice(),
            }),
        );
        jit.cache
            .insert(pc, Some(compiled_block(slot, 0x8000_1120)));

        assert!(jit.page_module_owns_cached_entry(7, pc));
        assert!(!jit.page_module_owns_cached_entry(8, pc));
    }

    #[test]
    fn failed_page_replacement_restores_the_live_owner() {
        let mut jit = JitState::new();
        let key = (7, 0x4000);
        let source = page_source(1);
        let prior = LivePageModule {
            source,
            owner: 21,
            entries: vec![0x4000, 0x4004].into_boxed_slice(),
        };
        jit.page_modules.insert(
            key,
            PageModuleState::Pending {
                source,
                ticket: 9,
                prior: Some(prior),
            },
        );
        let batch = pending_page_batch(key, 0x4010);

        assert_eq!(
            jit.finish_pending_page_module(8, &batch, PageModuleFinish::Failed),
            None
        );
        assert!(matches!(
            jit.page_modules.get(&key),
            Some(PageModuleState::Pending { .. })
        ));
        assert_eq!(
            jit.finish_pending_page_module(9, &batch, PageModuleFinish::Failed),
            None
        );
        assert!(matches!(
            jit.page_modules.get(&key),
            Some(PageModuleState::Live(LivePageModule { owner: 21, .. }))
        ));
    }

    #[test]
    fn successful_page_replacement_transfers_lifecycle_ownership() {
        let mut jit = JitState::new();
        let key = (7, 0x4000);
        let source = page_source(1);
        jit.page_modules.insert(
            key,
            PageModuleState::Pending {
                source,
                ticket: 9,
                prior: Some(LivePageModule {
                    source,
                    owner: 21,
                    entries: vec![0x4000].into_boxed_slice(),
                }),
            },
        );
        let batch = pending_page_batch(key, 0x4010);

        assert_eq!(
            jit.finish_pending_page_module(9, &batch, PageModuleFinish::Landed(31)),
            Some(21)
        );
        assert!(matches!(
            jit.page_modules.get(&key),
            Some(PageModuleState::Live(LivePageModule { owner: 31, .. }))
        ));
        assert_eq!(jit.page_module_owner.get(&31), Some(&key));
    }

    #[test]
    fn cancelled_page_module_can_retry_the_same_source() {
        let mut jit = JitState::new();
        let key = (7, 0x4000);
        let source = page_source(1);
        jit.page_modules.insert(
            key,
            PageModuleState::Pending {
                source,
                ticket: 9,
                prior: None,
            },
        );
        let batch = pending_page_batch(key, 0x4010);

        assert_eq!(
            jit.finish_pending_page_module(9, &batch, PageModuleFinish::Cancelled),
            None
        );
        assert!(!jit.page_modules.contains_key(&key));
        assert_eq!(
            jit.page_module_decision(key, source),
            PageModuleDecision::Build(None)
        );
    }

    #[test]
    fn dirty_pending_pages_do_not_invalidate_another_address_space() {
        let mut jit = JitState::new();
        let virtual_page = 0x1000;
        let a = (7, virtual_page);
        let b = (8, virtual_page);

        for (ticket, key, physical) in [(1, a, 0x8000_1000), (2, b, 0x8000_2000)] {
            jit.page_entries.insert(key, vec![virtual_page]);
            jit.superblocked.insert(key);
            jit.claim_pending_superblock(ticket, key.0, &[(key.1, physical)]);
            jit.sb_gen.insert(key, (1, 0, physical));
        }

        let exact = jit.tracked_page_keys_for_physical(1).into_iter().collect();
        jit.invalidate_superblock_state(&exact, &Default::default());

        assert!(!jit.page_entries.contains_key(&a));
        assert!(!jit.superblocked.contains(&a));
        assert!(!jit.pending_superblocks.contains_key(&a));
        assert!(!jit.sb_gen.contains_key(&a));
        assert!(jit.page_entries.contains_key(&b));
        assert!(jit.superblocked.contains(&b));
        assert!(jit.pending_superblocks.contains_key(&b));
        assert!(jit.sb_gen.contains_key(&b));

        jit.invalidate_superblock_state(&Default::default(), &HashSet::from([virtual_page]));
        assert!(!jit.page_entries.contains_key(&b));
        assert!(!jit.superblocked.contains(&b));
        assert!(!jit.pending_superblocks.contains_key(&b));
        assert!(!jit.sb_gen.contains_key(&b));
    }

    #[test]
    fn dirty_page_module_invalidation_is_physical_and_owner_atomic() {
        let mut jit = JitState::new();
        let virtual_page = 0x4000;
        let a = (7, virtual_page);
        let b = (8, virtual_page);
        let pc_a = virtual_page + 0x120;
        let pc_b = virtual_page + 0x220;

        for (key, owner, physical, pc) in [(a, 21_u64, 1, pc_a), (b, 31, 2, pc_b)] {
            let slot = owner as i32;
            jit.owner_slots.insert(owner, vec![slot]);
            jit.slot_owner.insert(slot, owner);
            jit.page_module_owner.insert(owner, key);
            jit.page_modules.insert(
                key,
                PageModuleState::Live(LivePageModule {
                    source: page_source(physical),
                    owner,
                    entries: vec![pc].into_boxed_slice(),
                }),
            );
            jit.page_hot_entries.insert(key, vec![pc]);
            jit.cache_insert(
                pc,
                Some(compiled_block(
                    slot,
                    0x8000_0000 + (physical << 12) + (pc & 0xfff),
                )),
            );
        }

        let exact: HashSet<_> = jit.tracked_page_keys_for_physical(1).into_iter().collect();
        jit.invalidate_superblock_state(&exact, &HashSet::from([virtual_page]));

        assert!(!jit.page_modules.contains_key(&a));
        assert!(!jit.page_module_owner.contains_key(&21));
        assert!(!jit.slot_owner.contains_key(&21));
        assert!(!jit.cache.contains_key(&pc_a));

        assert!(jit.page_modules.contains_key(&b));
        assert_eq!(jit.page_module_owner.get(&31), Some(&b));
        assert_eq!(jit.slot_owner.get(&31), Some(&31));
        assert!(jit.cache.contains_key(&pc_b));
        assert!(jit.page_hot_entries.contains_key(&b));
        assert_eq!(
            jit.tagged_dispatch_index(compiled_block(31, 0x8000_2220)) & PAGE_IDX_BIT,
            PAGE_IDX_BIT
        );
    }

    #[test]
    fn cold_owner_eviction_retires_the_complete_owner() {
        let mut jit = JitState::new();
        let hot_owner = 21_u64;
        let cold_owner = 31_u64;
        let hot_pc = 0x4120;
        let cold_pcs = [0x8120, 0x9120];

        for (owner, slots) in [(hot_owner, vec![21]), (cold_owner, vec![31, 32])] {
            jit.owner_slots.insert(owner, slots.clone());
            for slot in slots {
                jit.slot_owner.insert(slot, owner);
            }
        }
        jit.cache_insert(hot_pc, Some(compiled_block(21, 0x8000_1120)));
        jit.cache_insert(cold_pcs[0], Some(compiled_block(31, 0x8000_2120)));
        jit.cache_insert(cold_pcs[1], Some(compiled_block(32, 0x8000_3120)));
        for pc in cold_pcs {
            jit.hot.insert(pc, unsafe { super::JIT_THRESHOLD });
            let slot = JitState::dslot(pc);
            jit.interp_hot_tag[slot] = (pc >> 1) as u32;
            jit.interp_hot[slot] = unsafe { super::JIT_THRESHOLD } as u16;
        }

        jit.update_owner_recency(hot_owner, super::next_jit_use_stamp());
        jit.update_owner_recency(cold_owner, super::next_jit_use_stamp());
        jit.touch(hot_pc);

        assert_eq!(jit.evict_cold_owner(), 2);
        assert!(jit.cache.contains_key(&hot_pc));
        assert!(cold_pcs.iter().all(|pc| !jit.cache.contains_key(pc)));
        assert!(!jit.owner_slots.contains_key(&cold_owner));
        assert!(!jit.owner_entries.contains_key(&cold_owner));
        assert!(!jit.owner_recency.contains_key(&cold_owner));
        assert!(jit
            .cold_owners
            .iter()
            .all(|&(_, owner)| owner != cold_owner));
        assert!(jit.slot_owner.values().all(|&owner| owner != cold_owner));
        assert!(jit.slot_refs.keys().all(|slot| ![31, 32].contains(slot)));
        for pc in cold_pcs {
            assert!(!jit.hot.contains_key(&pc));
            assert_eq!(jit.interp_hot[JitState::dslot(pc)], 0);
        }

        jit.retire_owner(hot_owner, false);
    }

    #[test]
    fn reused_table_slot_does_not_merge_distinct_owners() {
        let mut jit = JitState::new();
        let first_owner = 100_u64;
        let second_owner = 101_u64;
        let first_pc = 0x10_120;
        let second_pc = 0x20_120;

        jit.track_owner(first_owner, [101, 102]).unwrap();
        jit.cache_insert(first_pc, Some(compiled_block(102, 0x8001_0120)));
        jit.retire_unreferenced_slots(first_owner);
        assert!(!jit.slot_owner.contains_key(&101));
        assert_eq!(jit.slot_owner.get(&102), Some(&first_owner));

        jit.track_owner(second_owner, [101]).unwrap();
        jit.cache_insert(second_pc, Some(compiled_block(101, 0x8002_0120)));
        assert_eq!(jit.owner_entries.get(&first_owner), Some(&vec![first_pc]));
        assert_eq!(jit.owner_entries.get(&second_owner), Some(&vec![second_pc]));

        jit.retire_owner(first_owner, false);
        assert!(!jit.cache.contains_key(&first_pc));
        assert!(jit.cache.contains_key(&second_pc));
        assert_eq!(jit.slot_owner.get(&101), Some(&second_owner));
        jit.retire_owner(second_owner, false);
    }

    #[test]
    fn one_slot_owner_retires_all_shared_entries() {
        let mut jit = JitState::new();
        let owner = 200_u64;
        let entries = [0x30_120, 0x30_220, 0x30_320];

        jit.track_owner(owner, [201]).unwrap();
        for (offset, pc) in entries.into_iter().enumerate() {
            jit.cache_insert(
                pc,
                Some(compiled_block(201, 0x8003_0120 + offset as u64 * 0x100)),
            );
        }

        assert_eq!(jit.retire_owner(owner, false), 1);
        assert!(entries.iter().all(|pc| !jit.cache.contains_key(pc)));
        assert!(!jit.slot_owner.contains_key(&201));
        assert!(!jit.owner_slots.contains_key(&owner));
        assert!(!jit.owner_entries.contains_key(&owner));
        assert!(!jit.owner_recency.contains_key(&owner));
    }

    #[test]
    fn internal_jit_callbacks_reject_forged_context_handles() {
        assert_eq!(super::jit_tlb_fill(0, 0, 0), -1);
        assert_eq!(super::jit_tlb_fill(1, 0, 0), -1);
        assert_eq!(super::jit_tlb_fill(0x1234, 0, 0), -1);
        super::chain_next(0x1234);
    }

    #[test]
    fn pending_host_io_still_accounts_compiled_dispatch() {
        let mut retired_sum = 5;
        let mut chained = 2;

        assert!(account_compiled_dispatch(
            &mut retired_sum,
            &mut chained,
            7,
            true,
        ));
        assert_eq!(retired_sum, 12);
        assert_eq!(chained, 3);
    }

    #[test]
    fn async_trace_region_priority_is_completion_order_independent() {
        let region = Some(JitBlock {
            fp: false,
            idx: 10,
            n: 0,
            mix: [0; 5],
            mem: [0; 10],
            control: [0; 3],
            alu: [0; 5],
            pa: 0x8000_0000,
            last_used: 0,
        });
        let short = JitBlock {
            fp: false,
            idx: -1,
            n: 15,
            mix: [0; 5],
            mem: [0; 10],
            control: [0; 3],
            alu: [0; 5],
            pa: 0x8000_0000,
            last_used: 0,
        };
        let long = JitBlock { n: 16, ..short };
        let fp = JitBlock { fp: true, ..long };

        assert!(!block_should_replace_region(Some(&region), short, 16));
        assert!(block_should_replace_region(Some(&region), long, 16));
        assert!(!block_should_replace_region(Some(&region), fp, 16));
        assert!(block_should_replace_region(None, short, 16));
    }
}

static mut SYS_JIT: Option<JitState> = None;

// Cell every compiled block writes with the number of guest instructions
// it actually retired before returning (sys blocks with inline memory ops
// can bail mid-block, so the count is dynamic — the dispatcher reads this
// rather than assuming full block length).
static mut RETIRED_CELL: u64 = 0;
/// Instruction fuel granted to the CURRENT dispatch (see JitLayout::fuel_addr):
/// compiled loops/superblocks yield once they retire this many instructions,
/// so caller budgets and the interrupt quantum hold to block granularity.
static mut FUEL_CELL: u64 = 0;
/// Diagnostics: emitted copy-loop fast paths bump this cell once per bulk
/// chunk (the emitter receives its address via JitLayout.copystat_addr).
static mut COPY_CHUNKS: u64 = 0;

fn retired_addr() -> u32 {
    (&raw const RETIRED_CELL) as u32
}

fn fuel_addr() -> u32 {
    (&raw const FUEL_CELL) as u32
}

fn copystat_addr() -> u32 {
    (&raw const COPY_CHUNKS) as u32
}

/// The JavaScript code store owns the hard module, byte, and slot limits. A
/// capacity rejection retires one cold owner and pauses compilation until the
/// current Wasm call returns, when JavaScript can safely reuse its slots.
const JIT_REGISTER_CAPACITY: i32 = -2;
static mut JIT_LIVE_SLOTS: Option<std::collections::HashSet<i32>> = None;
static mut JIT_TABLE_PEAK: u64 = 0;
static mut JIT_TABLE_RETIRED: u64 = 0;
static mut JIT_EVICTED_OWNERS: u64 = 0;
static mut JIT_EVICTED_SLOTS: u64 = 0;
static mut JIT_CAPACITY_REJECTIONS: u64 = 0;
static mut JIT_EVICTION_COOLED_ENTRIES: u64 = 0;
static mut JIT_CAPACITY_BLOCKED: bool = false;
static mut JIT_USE_STAMP: u64 = 0;

fn next_jit_use_stamp() -> u64 {
    unsafe {
        JIT_USE_STAMP = JIT_USE_STAMP.wrapping_add(1);
        JIT_USE_STAMP
    }
}

#[allow(static_mut_refs)]
fn register_table_slot(idx: i32) {
    if idx < 0 {
        return;
    }
    unsafe {
        let live = JIT_LIVE_SLOTS.get_or_insert_with(Default::default);
        live.insert(idx);
        JIT_TABLE_PEAK = JIT_TABLE_PEAK.max(live.len() as u64);
    }
}

#[allow(static_mut_refs)]
fn retire_table_slot(idx: i32, evicted: bool) {
    if idx < 0 {
        return;
    }
    unsafe {
        let Some(live) = JIT_LIVE_SLOTS.as_mut() else {
            return;
        };
        if live.remove(&idx) {
            emit_host_jit_retire(idx, u32::from(evicted));
            JIT_TABLE_RETIRED += 1;
        }
    }
}

fn jit_compilation_allowed() -> bool {
    unsafe { !JIT_CAPACITY_BLOCKED }
}

fn handle_jit_capacity(jit: &mut JitState) {
    unsafe {
        JIT_CAPACITY_REJECTIONS += 1;
        JIT_CAPACITY_BLOCKED = true;
    }
    // One owner is enough to make the next registration progress. Larger
    // bursts reclaim incrementally and cannot evict a full working set in one
    // long execution slice before JavaScript can reuse the first slot.
    jit.evict_cold_owner();
}

/// Longest compiled-code residency between interrupt/device checks, in guest
/// instructions: ~2.5ms at JIT speed. Loops yield at this bound even when the
/// caller's budget is larger (P0 interrupt-latency contract).
const INTERRUPT_QUANTUM: u64 = 1 << 20;

#[derive(Clone)]
struct PendingBlock {
    aspace: u64,
    pc: u64,
    block: JitBlock,
    pages: Vec<(u64, u64)>,
    page_generations: Vec<u64>,
    seeds: Vec<u64>,
    trace_pcs: Vec<u64>,
    missed_superblock: bool,
}

struct PendingBatch {
    cell: usize,
    sequence: u64,
    members: Vec<PendingBlock>,
    page_module: Option<(u64, u64)>,
}

struct StagedBlock {
    body: rv64_jit::RawBatchBody,
    pending: PendingBlock,
}

/// A page superblock compiling asynchronously on the browser's Wasm compiler.
/// Guest execution continues on existing code until the completed module passes
/// the boot, ownership, and physical-code-page checks below.
struct PendingRegion {
    /// satp of the address space the region was discovered in.
    aspace: u64,
    /// The page whose threshold crossing owns this region (cooldown identity
    /// across rebuilds and extensions).
    lead: u64,
    /// (virtual page, physical page) of every page in the compiled region,
    /// ascending (sparse regions need not be virtually contiguous).
    pages: Vec<(u64, u64)>,
    /// Write generation of each physical page when compilation was issued.
    /// This rejects an old compile after a dirty drain and later re-mark.
    page_generations: Vec<u64>,
    entries: Vec<u64>,
}

enum PendingJitKind {
    Block(PendingBlock),
    Batch(PendingBatch),
    Region(PendingRegion),
}

struct PendingJit {
    ticket: u64,
    boot_gen: u64,
    kind: PendingJitKind,
}

impl PendingJit {
    fn slot_count(&self) -> u32 {
        match &self.kind {
            PendingJitKind::Block(_) | PendingJitKind::Region(_) => 1,
            PendingJitKind::Batch(batch) if batch.page_module.is_some() => 1,
            PendingJitKind::Batch(batch) => batch.members.len() as u32,
        }
    }
}

/// Keep a small discovery window. The browser host limits active compiler jobs
/// separately; this queue hides Promise scheduling latency without allowing a
/// compile storm to starve the guest.
const MAX_PENDING_JIT: usize = 4;
const MAX_JIT_ISSUES_PER_RUN: u32 = 1;

/// Virtual pages a superblock region may span. Loops and functions straddle
/// page boundaries constantly; a page-clamped region turns every crossing into
/// a host dispatch (measured: nbench NUMERIC SORT ran its sift loop as six
/// 2-10 instruction blocks, 5.6 insns per dispatch, because the loop sits
/// across a page boundary).
// 3, not 8: the call-graph BFS at 8 pages regressed CPython's eval loop
// 2.4s -> 7s (big glued functions rebuild more and codegen worse); at 3 the
// sparse mechanics keep loop-straddling pages together and pull in ONE hot
// callee, which is where the measured value was.
const MAX_REGION_PAGES: usize = 3;
/// Pages an EXTENDED region may reach (translate_superblock_sparse caps hard
/// at 16). Extension only ever grows along measured exit traffic, so the cap
/// bounds V8 compile cost (~4KB/page of module bytes, 15-40ms per 8-page
/// async build measured), not guesswork about reachability. tcc's hot call
/// graph clusters at ~15 pages — an 8-page cap left its calls crossing
/// region boundaries forever.
const MAX_EXT_REGION_PAGES: usize = 16;
/// Attribute 1 of every 2^N region-function exits (full attribution is a
/// HashMap probe per dispatch — measurable on region-heavy code).
const EXIT_SAMPLE_SHIFT: u32 = 5;
/// Sampled out-of-region exits before a region asks for extension
/// (~EXT_TRIGGER << EXIT_SAMPLE_SHIFT real exits).
const EXT_TRIGGER: u32 = 16;
/// Distinct out-of-region target pages tracked per region.
const EXT_TARGET_CAP: usize = 8;
/// Dispatch-line idx bit marking a region function, so the chain loop can
/// attribute the following exit without a cache probe. Table indices stay
/// far below this bit (the host store defaults to 65536 slots); blacklist (-1)
/// keeps its sign.
/// Shared with the emitter, whose chain transfers mask it off.
const SB_IDX_BIT: i32 = rv64_jit::SB_IDX_BIT;
/// Page-module marker carried in DispatchLine::idx. Unlike owner lookup, this
/// is cheap enough to test after every compiled call.
const PAGE_IDX_BIT: i32 = rv64_jit::PAGE_IDX_BIT;
const IDX_TAG_MASK: i32 = rv64_jit::IDX_TAG_MASK;
/// Measured average stay (retired insns per dispatch of a region function)
/// below which an EXTENDED region is built with registers in MEMORY instead
/// of locals: short stays pay the union load/store at every entry/exit,
/// which for call-shaped code exceeds the work itself. Long-stay regions
/// (FP EMULATION holds ~444 insns) keep locals.
const EXT_MEMORY_MODE_STAY: u64 = 48;
/// Measured average stay below which a landed region function is DEMOTED:
/// its entries return to individual trace blocks and the lead page stops
/// rebuilding. A function whose visits run ~a dozen instructions pays its
/// per-entry cost for nothing — call-shaped code (tcc measured 8-20-insn
/// stays and ran 2x slower under page functions) wants traces, while
/// genuinely holding functions (FP EMULATION ~444-insn stays) never come
/// near this bar. The signal is per-region and measured, so one guest can
/// have both kinds of code and each page gets the winner.
const DEMOTE_STAY: u64 = 24;
/// Runtime toggle for the demotion pass (A/B diagnostics).
static mut DEMOTE_ON: bool = true;
#[no_mangle]
pub extern "C" fn jit_set_demote(on: u32) {
    unsafe { DEMOTE_ON = on != 0 }
}
/// Sampled exits before the demotion verdict is trusted. Zero-retire
/// samples (entry bails: FP gate, first-instruction TLB miss) are excluded
/// from the average — they are refusals, not stays, and a legitimate
/// long-stay FP region bails exactly like that while FS is off. 64 (~2K
/// real exits): the 16-sample verdict condemned regions on WARM-UP stays —
/// NUMERIC SORT's straddling-loop region measured 320-467 iter/s across
/// identical boots because whether it
/// survived demotion depended on how cold its first sampled exits were.
const DEMOTE_MIN_SAMPLES: u32 = 64;
static mut SB_DEMOTED: u64 = 0;
/// Legacy wide trace-window support: a 64-page (256KB) aligned VA region
/// around a hot pc, gathered into one contiguous buffer so traces can follow
/// calls across page boundaries. The measured default is now one page, but
/// the wide mode remains available for diagnostics and uses this cache.
const TRACE_WIN_PAGES: u64 = 64;
const TRACE_WIN_MASK: u64 = TRACE_WIN_PAGES * 0x1000 - 1;
struct TraceWin {
    aspace: u64,
    map_gen: u32,
    boot_gen: u64,
    first_va: u64,
    /// (va, physical page) of every RAM-backed mapped page in the window;
    /// unmapped holes stay zero-filled in `buf` (invalid instructions, so
    /// a trace walking into one simply ends there).
    pages: Vec<(u64, u64)>,
    buf: Vec<u8>,
}
/// Small LRU of gathered windows: one entry thrashed when a workload's hot
/// code alternates across several 256KB regions (CPython spans many — each
/// miss re-copies 256KB, which dwarfed the compile itself).
const TRACE_WIN_CACHE: usize = 8;
static mut TRACE_WIN: Vec<TraceWin> = Vec::new();
/// Translation-window control: 1 (the measured default) uses one page, 2
/// forces the legacy 64-page window, and 0 follows TRACE_LEVEL (level 0 is one
/// page, higher levels are wide). This keeps the old combinations available
/// for diagnostics while making the reproducibly faster narrow window the
/// normal policy.
static mut TRACE_WINDOW_MODE: u32 = 1;
#[no_mangle]
pub extern "C" fn jit_set_trace_window(mode: u32) {
    // Benchmark/runtime knobs are configured before boot, while TRACE_WIN is
    // still empty (the same pre-boot contract as the other JIT setters).
    unsafe { TRACE_WINDOW_MODE = mode.min(2) }
}
/// A landed region function does NOT claim a pc whose individual block is a
/// trace at least this long. 0 = functions claim everything: mixed claiming
/// fragments execution into function/trace ping-pong. Measured medians favor
/// claim-all (compile 3.3s vs 3.6-4.0s, HUFFMAN ~1010 vs ~920), but note the
/// boot-to-boot coverage races swing NUMERIC/HUFFMAN/ASSIGNMENT +/-20% in
/// EVERY configuration — single draws cannot distinguish 0 from 24 (both
/// were sampled at NUMERIC ~320 and ~445 on identical binaries). Runtime-
/// settable for A/B.
static mut TRACE_KEEP_MIN: u32 = 0;
#[no_mangle]
pub extern "C" fn jit_set_trace_keep_min(v: u32) {
    unsafe { TRACE_KEEP_MIN = v }
}
/// After a drain visit finds no matching aspace, don't rescan until this
/// many instructions pass (the fall-through drain otherwise scans the
/// queue on every chain break — measured 1.1M scans on one tcc run).
static mut SB_EXT_NEXT_ICOUNT: u64 = 0;
static mut EXIT_TICK: u64 = 0;
static mut SB_EXT_ISSUED: u64 = 0;
static mut SB_EXIT_SAMPLED: u64 = 0;
/// Diagnostic split of SB_EXIT_SAMPLED (jit_stat 35-38).
static mut SB_EXIT_NOMAP: u64 = 0;
static mut SB_EXIT_INREGION: u64 = 0;
static mut SB_EXT_DEFER_COOL: u64 = 0;
static mut SB_EXT_NO_TARGET: u64 = 0;
static mut SB_EXT_PUSHED: u64 = 0;
static mut SB_EXT_DRAIN_VISITS: u64 = 0;
static mut SB_EXT_DRAIN_NOMATCH: u64 = 0;
static mut PENDING_JIT: Vec<PendingJit> = Vec::new();
static mut NEXT_JIT_TICKET: u64 = 1;
static mut JIT_ISSUES_THIS_RUN: u32 = 0;

#[allow(static_mut_refs)]
fn full_system_jit_issue_allowed() -> bool {
    unsafe {
        !JIT_CAPACITY_BLOCKED
            && PENDING_JIT.len() < MAX_PENDING_JIT
            && JIT_ISSUES_THIS_RUN < MAX_JIT_ISSUES_PER_RUN
    }
}

#[allow(static_mut_refs)]
fn pending_jit_contains_pc(aspace: u64, pc: u64) -> bool {
    unsafe {
        PENDING_JIT.iter().any(|pending| match &pending.kind {
            PendingJitKind::Block(block) => block.aspace == aspace && block.pc == pc,
            PendingJitKind::Batch(batch) => batch
                .members
                .iter()
                .any(|member| member.aspace == aspace && member.pc == pc),
            // A pending region deliberately does not suppress an individual
            // block. Existing code remains fast while a region compiles.
            PendingJitKind::Region(_) => false,
        })
    }
}

fn code_page_generations<M: FullSystemJitMachine>(m: &M, pages: &[(u64, u64)]) -> Option<Vec<u64>> {
    pages
        .iter()
        .map(|&(_, physical)| {
            let page = physical.checked_sub(rv64_system::RAM_BASE)? >> 12;
            m.code_page_generation(page)
        })
        .collect()
}

fn pending_block<M: FullSystemJitMachine>(
    m: &M,
    aspace: u64,
    pc: u64,
    block: JitBlock,
    pages: Vec<(u64, u64)>,
    seeds: Vec<u64>,
    missed_superblock: bool,
) -> Option<PendingBlock> {
    Some(PendingBlock {
        aspace,
        pc,
        block,
        page_generations: code_page_generations(m, &pages)?,
        pages,
        seeds,
        trace_pcs: Vec::new(),
        missed_superblock,
    })
}

fn staged_block<M: FullSystemJitMachine>(
    m: &mut M,
    aspace: u64,
    source_pages: &[(u64, u64)],
    body: rv64_jit::RawBatchBody,
    mut member: rv64_jit::BatchMember,
    missed_superblock: bool,
) -> Option<StagedBlock> {
    let (lo, hi) = if member.span == (0, 0) {
        (member.pc, member.pc.checked_add(2)?)
    } else {
        member.span
    };
    let last = hi.checked_sub(1)?;
    let last_page = last & !0xfff;
    let mut pages = Vec::new();
    let mut va = lo & !0xfff;
    loop {
        let &(_, physical) = source_pages.iter().find(|&&(page, _)| page == va)?;
        pages.push((va, physical));
        if va == last_page {
            break;
        }
        va = va.checked_add(0x1000)?;
    }
    let entry_page = member.pc & !0xfff;
    let &(_, physical) = pages.iter().find(|&&(page, _)| page == entry_page)?;
    let block = JitBlock {
        fp: member.uses_fp,
        idx: -1,
        n: member.n_insns,
        mix: member.trace_mix,
        mem: member.trace_mem,
        control: member.trace_control,
        alu: member.trace_alu,
        pa: physical + (member.pc & 0xfff),
        last_used: 0,
    };
    for &(_, physical) in &pages {
        m.code_mark_page(physical);
    }
    let mut trace_pcs = core::mem::take(&mut member.trace_pcs);
    trace_pcs.extend_from_slice(&member.seeds);
    trace_pcs.sort_unstable();
    trace_pcs.dedup();
    let mut pending = pending_block(
        m,
        aspace,
        member.pc,
        block,
        pages,
        member.seeds,
        missed_superblock && block.n < unsafe { TRACE_KEEP_MIN },
    )?;
    pending.trace_pcs = trace_pcs;
    Some(StagedBlock { body, pending })
}

fn jit_work_contains_pc(jit: &JitState, aspace: u64, pc: u64) -> bool {
    jit.confirmed_stage
        .iter()
        .any(|staged| staged.pending.aspace == aspace && staged.pending.pc == pc)
        || pending_jit_contains_pc(aspace, pc)
}

#[allow(static_mut_refs)]
fn submit_confirmed_stage(jit: &mut JitState) -> bool {
    let target = unsafe { CONFIRMED_BATCH_TARGET };
    if jit.confirmed_stage.len() < target || !full_system_jit_issue_allowed() {
        return false;
    }

    let staged: Vec<_> = jit.confirmed_stage.drain(..target).collect();
    let mut raw = Vec::with_capacity(target);
    let mut members = Vec::with_capacity(target);
    for entry in staged {
        raw.push(entry.body);
        members.push(entry.pending);
    }
    let wasm = rv64_jit::finish_confirmed_body_batch(raw);

    let (cell, sequence) = unsafe {
        let cell = BATCH_CELL_NEXT;
        BATCH_CELL_NEXT = (cell + 1) % BATCH_CELLS;
        let sequence = NEXT_BATCH_SEQUENCE;
        NEXT_BATCH_SEQUENCE = NEXT_BATCH_SEQUENCE.wrapping_add(1);
        BATCH_CELL_SEQUENCE[cell] = sequence;
        (cell, sequence)
    };
    unsafe { JIT_OUT = wasm };
    submit_pending_jit(PendingJitKind::Batch(PendingBatch {
        cell,
        sequence,
        members,
        page_module: None,
    }))
    .expect("confirmed batch lost its reserved issue slot");
    true
}

/// Build one stable multi-function module for a hot code page. Observed hot
/// targets become independent traces; loop headers stay with the structured
/// loop translator.
fn issue_page_module<M: FullSystemJitMachine>(
    m: &mut M,
    jit: &mut JitState,
    current_pc: u64,
    pa_page: u64,
    code: &[u8],
    mut lay: rv64_jit::JitLayout,
) -> PageModuleIssue {
    let aspace = m.cpu().sys.as_ref().map_or(0, |context| context.satp);
    let vpage = current_pc & !0xfff;
    let key = (aspace, vpage);
    if !unsafe { PAGE_MODULES_ON } {
        return PageModuleIssue::Individual;
    }
    let Some(seeds) = jit.page_hot_entries.get(&key).cloned() else {
        return PageModuleIssue::Individual;
    };
    if seeds.len() < PAGE_MODULE_THRESHOLD {
        return PageModuleIssue::Individual;
    }
    if !full_system_jit_issue_allowed() {
        return PageModuleIssue::Individual;
    }
    let Some(physical_page) = pa_page
        .checked_sub(rv64_system::RAM_BASE)
        .map(|offset| offset >> 12)
    else {
        return PageModuleIssue::Individual;
    };
    let Some(generation) = m.code_page_generation(physical_page) else {
        return PageModuleIssue::Individual;
    };
    let source = PageModuleSource {
        physical_page,
        generation,
        tlb_fill: unsafe {
            TLB_FILL_POLICY == TLB_FILL_ON
                || (TLB_FILL_POLICY == TLB_FILL_AUTO && jit.tlb_auto_enabled)
        },
    };
    let usable = |entry: u64| {
        rv64_jit::emittable_at(code, vpage, entry, lay)
            && !rv64_jit::is_loop_at(code, vpage, entry, lay)
    };
    if !usable(current_pc) {
        return PageModuleIssue::Individual;
    }
    let prior = match jit.page_module_decision(key, source) {
        PageModuleDecision::Individual => return PageModuleIssue::Individual,
        PageModuleDecision::Awaiting => return PageModuleIssue::Awaiting,
        PageModuleDecision::Build(prior) => prior,
    };

    // Keep table-visible entries small. Further observed hot targets remain
    // private functions in the module and are reachable through direct links
    // or the private computed-target router.
    let mut entries = Vec::with_capacity(PAGE_MODULE_MAX_LEADERS);
    entries.push(current_pc);
    for entry in seeds.iter().copied() {
        if entries.len() >= PAGE_MODULE_MAX_LEADERS {
            break;
        }
        if entry != current_pc && usable(entry) && !entries.contains(&entry) {
            entries.push(entry);
        }
    }
    let trace_entries = entries.len();
    if let Some(live) = prior.as_ref() {
        for &entry in live.entries.iter() {
            if entries.len() >= PAGE_MODULE_MAX_LEADERS {
                break;
            }
            if !entries.contains(&entry) && usable(entry) {
                entries.push(entry);
            }
        }
    }
    let (discovered, loop_candidates) = rv64_jit::discover_page_leaders_ext(
        code,
        vpage,
        vpage,
        0x1000,
        &entries,
        PAGE_MODULE_MAX_LEADERS,
    );
    for entry in discovered {
        if entries.len() >= PAGE_MODULE_MAX_LEADERS || entries.contains(&entry) {
            continue;
        }
        if !rv64_jit::emittable_at(code, vpage, entry, lay) {
            continue;
        }
        if loop_candidates.contains(&entry) && rv64_jit::is_loop_at(code, vpage, entry, lay) {
            continue;
        }
        entries.push(entry);
    }
    if entries.len() < 2 {
        return PageModuleIssue::Individual;
    }
    lay.dispatch_base = jit.dispatch.as_ptr() as u32;
    lay.dispatch_mask = (DISPATCH_SIZE - 1) as u32;
    lay.map_gen_addr = m.cpu().jit_map_gen_ptr() as u32;
    let hot = |pc: u64| matches!(jit.cache.get(&pc), Some(Some(block)) if block.idx >= 0);
    let next = |pc: u64| {
        let successor = jit.succ[JitState::dslot(pc)];
        (successor.0 == pc).then_some(successor.1)
    };
    let Some((wasm, members)) =
        rv64_jit::translate_page_module(code, vpage, &entries, trace_entries, lay, &hot, &next)
    else {
        jit.record_page_module_failure(key, source, prior);
        return PageModuleIssue::Individual;
    };

    let mut pending_members = Vec::with_capacity(members.len());
    for member in members {
        let block = JitBlock {
            fp: member.uses_fp,
            idx: -1,
            n: member.n_insns,
            mix: member.trace_mix,
            mem: member.trace_mem,
            control: member.trace_control,
            alu: member.trace_alu,
            pa: pa_page + (member.pc - vpage),
            last_used: 0,
        };
        let Some(member) = pending_block(
            m,
            aspace,
            member.pc,
            block,
            vec![(vpage, pa_page)],
            member.seeds,
            false,
        ) else {
            return PageModuleIssue::Individual;
        };
        pending_members.push(member);
    }
    if pending_members.len() < 2 {
        return PageModuleIssue::Individual;
    }

    let (cell, sequence) = unsafe {
        let cell = BATCH_CELL_NEXT;
        BATCH_CELL_NEXT = (cell + 1) % BATCH_CELLS;
        let sequence = NEXT_BATCH_SEQUENCE;
        NEXT_BATCH_SEQUENCE = NEXT_BATCH_SEQUENCE.wrapping_add(1);
        BATCH_CELL_SEQUENCE[cell] = sequence;
        (cell, sequence)
    };
    m.code_mark_page(pa_page);
    unsafe { JIT_OUT = wasm };
    let pending = PendingJitKind::Batch(PendingBatch {
        cell,
        sequence,
        members: pending_members,
        page_module: Some(key),
    });
    let Some(ticket) = submit_pending_jit(pending) else {
        return PageModuleIssue::Individual;
    };
    jit.page_modules.insert(
        key,
        PageModuleState::Pending {
            source,
            ticket,
            prior,
        },
    );
    unsafe { PAGE_MODULES_ISSUED += 1 };
    PageModuleIssue::Issued
}

#[allow(static_mut_refs)]
fn submit_pending_jit(kind: PendingJitKind) -> Option<u64> {
    if !full_system_jit_issue_allowed() {
        return None;
    }
    unsafe {
        let ticket = NEXT_JIT_TICKET;
        NEXT_JIT_TICKET = NEXT_JIT_TICKET.wrapping_add(1);
        let pending = PendingJit {
            ticket,
            boot_gen: BOOT_GEN,
            kind,
        };
        let slot_count = pending.slot_count();
        PENDING_JIT.push(pending);
        JIT_ISSUES_THIS_RUN += 1;
        host_jit_register_async(ticket, slot_count);
        Some(ticket)
    }
}
// Superblock lifecycle counters (diagnostic, jit_stat 10..14).
static mut SB_TRIGGER: u64 = 0;
static mut SB_XLATE_FAIL: u64 = 0;
static mut SB_ISSUED: u64 = 0;
static mut SB_LANDED: u64 = 0;
static mut SB_STALE: u64 = 0;
/// Dispatches that entered a compiled block and retired NOTHING (entry bail:
/// FP gate, first-instruction TLB miss, or a br_table slot the function
/// doesn't own). Each one costs a call plus a single interpreted instruction.
static mut ZERO_RETIRE: u64 = 0;
/// Individual blocks compiled for a pc on a page that is already superblocked
/// (i.e. code the page's superblock does not cover), and superblock compiles
/// still awaiting their async module.
static mut SB_INDIV: u64 = 0;
/// Why an entry retired nothing (sampled under DPROF_ON): the FP gate's three
/// conditions, checked host-side at the moment of the bail.
static mut ZR_NX: u64 = 0;
static mut ZR_FRM: u64 = 0;
static mut ZR_FS: u64 = 0;
/// Compiled entries evicted at dispatch because their code page no longer maps
/// where it did (split: the entry's own page vs another page of its region).
static mut DROP_SELF: u64 = 0;
static mut DROP_REGION: u64 = 0;
/// Dirty-code-page events and the compiled entries they dropped.
static mut DIRTY_EVENTS: u64 = 0;
static mut DIRTY_DROPPED: u64 = 0;
/// Entries installed by landed superblocks, and how many of those installs
/// replaced an individual block (i.e. code that had already fallen back).
static mut SB_ENTRIES_IN: u64 = 0;
static mut SB_REPLACED: u64 = 0;
/// Trace one pc through the compile pipeline (diagnostic).
static mut TRACE_PC: u64 = 0;
static mut TRACE_SB_INSTALL: u64 = 0;
static mut TRACE_INDIV: u64 = 0;
static mut TRACE_SEED: u64 = 0;
static mut TRACE_ENTRY: u64 = 0;
/// Bumped by every VM boot: async results from a previous machine must be dropped.
static mut BOOT_GEN: u64 = 0;

// Perf instrumentation: guest instructions retired inside JIT blocks vs
// total, and dispatch counts (block calls). Exposed via jit_stat().
static mut JIT_RETIRED: u64 = 0;
static mut SLICE_CALLS: u64 = 0;
static mut SLICE_INSNS: u64 = 0;
static mut JIT_DISPATCHES: u64 = 0;
/// Diagnostic mode bits: 1=memory paths, 2=register boundaries, 4=size only.
static mut MEMPROF_MODE: u32 = 0;
static mut MULTI_LATCH: bool = true;
static mut MEMPROF: [u64; 89] = [0; 89];

#[no_mangle]
pub extern "C" fn memprof_set(on: u32) {
    unsafe {
        MEMPROF_MODE = on;
        MEMPROF = [0; 89];
    }
}

#[no_mangle]
pub extern "C" fn memprof_get(index: u32) -> u64 {
    unsafe { MEMPROF[index as usize % 89] }
}

#[no_mangle]
pub extern "C" fn jit_set_multi_latch(on: u32) {
    unsafe { MULTI_LATCH = on != 0 }
}

#[allow(static_mut_refs)]
fn mem_profile_layout() -> Option<[u32; 17]> {
    unsafe {
        if MEMPROF_MODE & 3 == 0 {
            return None;
        }
        let base = MEMPROF.as_ptr() as u32;
        let mem = MEMPROF_MODE & 1 != 0;
        let regs = MEMPROF_MODE & 2 != 0;
        Some([
            if mem { base } else { 0 },
            if mem { base + 8 } else { 0 },
            if mem { base + 16 } else { 0 },
            if mem { base + 24 } else { 0 },
            if mem { base + 32 } else { 0 },
            if regs { base + 40 } else { 0 },
            if regs { base + 48 } else { 0 },
            if regs { base + 56 } else { 0 },
            if regs { base + 64 } else { 0 },
            if regs { base + 152 } else { 0 },
            if regs { base + 160 } else { 0 },
            if regs { base + 168 } else { 0 },
            if regs { base + 176 } else { 0 },
            if regs { base + 184 } else { 0 },
            if regs { base + 192 } else { 0 },
            if regs { base + 200 } else { 0 },
            if regs { base + 208 } else { 0 },
        ])
    }
}

fn reg_stress() -> bool {
    unsafe { MEMPROF_MODE & 8 != 0 }
}

#[allow(static_mut_refs)]
fn reg_profile_base() -> u32 {
    unsafe {
        if MEMPROF_MODE & 2 != 0 {
            MEMPROF.as_ptr().add(27) as u32
        } else {
            0
        }
    }
}

// Dispatch-site profiler (diagnostic, off by default): direct-mapped
// (pc -> dispatches, retired) so a run can be attributed per guest pc —
// the metric that tells small-block/dispatch-bound kernels apart from
// genuinely slow code. One predictable branch per dispatch when off.
const DPROF_N: usize = 8192;
static mut DPROF_PC: [u64; DPROF_N] = [0; DPROF_N];
static mut DPROF_CNT: [u64; DPROF_N] = [0; DPROF_N];
static mut DPROF_RET: [u64; DPROF_N] = [0; DPROF_N];
static mut EPROF_SRC: [u64; DPROF_N] = [0; DPROF_N];
static mut EPROF_DST: [u64; DPROF_N] = [0; DPROF_N];
static mut EPROF_CNT: [u64; DPROF_N] = [0; DPROF_N];
static mut EPROF_RET: [u64; DPROF_N] = [0; DPROF_N];
static mut DPROF_ON: bool = false;
/// Profile one of every 2^N dispatches. Full attribution materially perturbs
/// dispatch-heavy nbench kernels, so diagnostics default to sampling in the
/// JS worker while shift=0 preserves the original exact profiler.
static mut DPROF_SAMPLE_SHIFT: u32 = 0;
static mut DPROF_TICK: u64 = 0;
static mut DPROF_BLOCK_CALLS: u64 = 0;
static mut DPROF_BLOCK_INSNS: u64 = 0;
static mut DPROF_REGION_CALLS: u64 = 0;
static mut DPROF_REGION_INSNS: u64 = 0;
static mut DPROF_TRACE_MIX: [u64; 5] = [0; 5];
static mut DPROF_TRACE_MEM: [u64; 10] = [0; 10];
static mut DPROF_TRACE_CONTROL: [u64; 3] = [0; 3];
static mut DPROF_TRACE_ALU: [u64; 5] = [0; 5];

#[no_mangle]
pub extern "C" fn dprof_set(on: u32) {
    unsafe {
        DPROF_ON = on != 0;
        if on != 0 {
            DPROF_TICK = 0;
            DPROF_PC = [0; DPROF_N];
            DPROF_CNT = [0; DPROF_N];
            DPROF_RET = [0; DPROF_N];
            EPROF_SRC = [0; DPROF_N];
            EPROF_DST = [0; DPROF_N];
            EPROF_CNT = [0; DPROF_N];
            EPROF_RET = [0; DPROF_N];
            DPROF_BLOCK_CALLS = 0;
            DPROF_BLOCK_INSNS = 0;
            DPROF_REGION_CALLS = 0;
            DPROF_REGION_INSNS = 0;
            DPROF_TRACE_MIX = [0; 5];
            DPROF_TRACE_MEM = [0; 10];
            DPROF_TRACE_CONTROL = [0; 3];
            DPROF_TRACE_ALU = [0; 5];
        }
    }
}

/// which: 0 = source pc, 1 = target pc, 2 = transitions, 3 = retired insns.
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn eprof_get(which: u32, i: u32) -> u64 {
    let i = i as usize % DPROF_N;
    unsafe {
        match which {
            0 => EPROF_SRC[i],
            1 => EPROF_DST[i],
            2 => EPROF_CNT[i],
            _ => EPROF_RET[i],
        }
    }
}

#[no_mangle]
pub extern "C" fn dprof_set_sample_shift(shift: u32) {
    unsafe { DPROF_SAMPLE_SHIFT = shift.min(20) }
}

/// which: 0 = pc, 1 = dispatches, 2 = retired insns.
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn dprof_get(which: u32, i: u32) -> u64 {
    let i = i as usize % DPROF_N;
    unsafe {
        match which {
            0 => DPROF_PC[i],
            1 => DPROF_CNT[i],
            _ => DPROF_RET[i],
        }
    }
}

/// Histogram of the INSTRUCTION the JIT gave up on (diagnostic, DPROF_ON):
/// keyed by the encoding fields that select an emitter path. Interpreted
/// instructions cost ~300x a compiled one, so a handful of missing encodings
/// can dominate a kernel's wall time.
const IHIST_N: usize = 1024;
static mut IHIST_KEY: [u32; IHIST_N] = [0; IHIST_N];
static mut IHIST_CNT: [u64; IHIST_N] = [0; IHIST_N];

#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn ihist_get(which: u32, i: u32) -> u64 {
    let i = i as usize % IHIST_N;
    unsafe {
        match which {
            0 => IHIST_KEY[i] as u64,
            1 => IHIST_CNT[i],
            _ => IHIST_INSNS[i],
        }
    }
}

/// Interpreted instructions charged to the fallback that started the stretch.
static mut IHIST_INSNS: [u64; IHIST_N] = [0; IHIST_N];
static mut IHIST_LAST: usize = usize::MAX;

#[allow(static_mut_refs)]
fn ihist_hit(insn: u32) {
    // opcode + funct3 + funct7 (and the rs2 field, which selects FCVT variants)
    let key = if insn & 3 != 3 {
        insn & 0xffff // compressed: whole halfword
    } else {
        insn & 0xfff0_707f
    };
    unsafe {
        let h = ((key ^ (key >> 13)).wrapping_mul(0x9e37_79b9) >> 18) as usize & (IHIST_N - 1);
        if IHIST_KEY[h] != key {
            if IHIST_CNT[h] != 0 {
                return;
            }
            IHIST_KEY[h] = key;
        }
        IHIST_CNT[h] += 1;
        IHIST_LAST = h;
    }
}

#[inline(always)]
#[allow(static_mut_refs)]
fn dprof_hit(pc: u64, retired: u64) {
    unsafe {
        let h = ((pc >> 1) ^ (pc >> 13)) as usize & (DPROF_N - 1);
        if DPROF_PC[h] != pc {
            if DPROF_CNT[h] != 0 {
                return; // collision: first hot pc keeps the slot
            }
            DPROF_PC[h] = pc;
        }
        DPROF_CNT[h] += 1;
        DPROF_RET[h] += retired;
    }
}

#[inline(always)]
#[allow(static_mut_refs)]
fn eprof_hit(src: u64, dst: u64, retired: u64) {
    unsafe {
        let h = ((src >> 1) ^ (src >> 13) ^ (dst >> 3) ^ (dst >> 17)) as usize & (DPROF_N - 1);
        if EPROF_SRC[h] != src || EPROF_DST[h] != dst {
            if EPROF_CNT[h] != 0 {
                return; // collision: first hot edge keeps the slot
            }
            EPROF_SRC[h] = src;
            EPROF_DST[h] = dst;
        }
        EPROF_CNT[h] += 1;
        EPROF_RET[h] += retired;
    }
}

/// Return one JIT diagnostic counter.
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn jit_stat(which: u32) -> u64 {
    unsafe {
        match which {
            0 => JIT_RETIRED,
            1 => JIT_DISPATCHES,
            2 => 0,
            3 => SYS_JIT.as_ref().map_or(0, |j| j.cache.len() as u64),
            4 => SLICE_CALLS,
            5 => SLICE_INSNS,
            8 => COPY_CHUNKS,
            10 => SB_TRIGGER,
            11 => SB_XLATE_FAIL,
            12 => SB_ISSUED,
            13 => SB_LANDED,
            14 => SB_STALE,
            15 => ZERO_RETIRE,
            16 => SB_INDIV,
            17 => PENDING_JIT.len() as u64,
            18 => ZR_NX,
            19 => ZR_FRM,
            20 => ZR_FS,
            21 => DROP_SELF,
            22 => DROP_REGION,
            31 => TLB_FILLS,
            23 => DIRTY_EVENTS,
            24 => DIRTY_DROPPED,
            25 => SB_ENTRIES_IN,
            26 => SB_REPLACED,
            27 => TRACE_SB_INSTALL,
            28 => TRACE_INDIV,
            29 => TRACE_SEED,
            30 => TRACE_ENTRY,
            32 => SB_EXT_ISSUED,
            33 => SB_EXIT_SAMPLED,
            34 => SB_BUILD_MS as u64,
            35 => SB_EXIT_NOMAP,
            36 => SB_EXIT_INREGION,
            37 => SB_EXT_DEFER_COOL,
            38 => SB_EXT_NO_TARGET,
            39 => SB_EXT_PUSHED,
            40 => SB_EXT_DRAIN_VISITS,
            41 => SB_EXT_DRAIN_NOMATCH,
            42 => SB_DEMOTED,
            43 => BATCHES,
            44 => BATCH_MEMBERS,
            45 => IC_EXTENDS,
            46 => DPROF_BLOCK_CALLS,
            47 => DPROF_BLOCK_INSNS,
            48 => DPROF_REGION_CALLS,
            49 => DPROF_REGION_INSNS,
            50..=54 => DPROF_TRACE_MIX[(which - 50) as usize],
            55..=64 => DPROF_TRACE_MEM[(which - 55) as usize],
            65..=67 => DPROF_TRACE_CONTROL[(which - 65) as usize],
            68..=72 => DPROF_TRACE_ALU[(which - 68) as usize],
            73 => JIT_LIVE_SLOTS
                .as_ref()
                .map_or(0, |slots| slots.len() as u64),
            74 => JIT_TABLE_PEAK,
            75 => JIT_TABLE_RETIRED,
            76 => JIT_EVICTED_OWNERS,
            77 => JIT_EVICTED_SLOTS,
            78 => JIT_CAPACITY_REJECTIONS,
            79 => PENDING_JIT
                .iter()
                .filter(|pending| matches!(pending.kind, PendingJitKind::Block(_)))
                .count() as u64,
            80 => PENDING_JIT
                .iter()
                .filter(|pending| matches!(pending.kind, PendingJitKind::Batch(_)))
                .count() as u64,
            81 => PENDING_JIT
                .iter()
                .filter(|pending| matches!(pending.kind, PendingJitKind::Region(_)))
                .count() as u64,
            82 => SYS_JIT
                .as_ref()
                .map_or(0, |jit| jit.page_blocks.len() as u64),
            83 => SYS_JIT.as_ref().map_or(0, |jit| {
                jit.cache
                    .keys()
                    .map(|pc| pc & !0xfff)
                    .collect::<std::collections::HashSet<_>>()
                    .len() as u64
            }),
            84 => SYS_JIT.as_ref().map_or(0, |jit| jit.hot.len() as u64),
            85 => SYS_JIT.as_ref().map_or(0, |jit| jit.tlb_bail_total),
            86 => SYS_JIT.as_ref().is_some_and(|jit| jit.tlb_auto_enabled) as u64,
            87 => PAGE_MODULES_ISSUED,
            88 => PAGE_MODULES_LANDED,
            89 => PAGE_MODULE_MEMBERS,
            90 => SYS_JIT
                .as_ref()
                .map_or(0, |jit| jit.confirmed_stage.len() as u64),
            91 => SYS_JIT
                .as_ref()
                .map_or(0, |jit| jit.confirmed_coverage.len() as u64),
            92 => JIT_EVICTION_COOLED_ENTRIES,
            _ => 0,
        }
    }
}

// JIT tier-up threshold. Settable at runtime (jit_set_enabled) so
// benchmarks can compare against the pure wasm interpreter.
/// Compile a block after it is dispatched this many times. High enough
/// that one-shot boot code stays interpreted; low enough that compute
/// loops (dispatched millions of times) tier up quickly.
const JIT_ON_THRESHOLD: u32 = 64;
static mut JIT_THRESHOLD: u32 = 64;
/// Enable/disable JIT tier-up (1/0). Disabling sets the threshold beyond
/// any counter so blocks are never compiled — pure interpreter baseline.
#[no_mangle]
pub extern "C" fn jit_set_enabled(on: u32) {
    unsafe {
        JIT_THRESHOLD = if on == 0 { u32::MAX } else { JIT_ON_THRESHOLD };
        // "Disabled" means EXECUTE NO JIT CODE, not just "stop compiling":
        // drop already-compiled blocks so A/B comparisons and the API name
        // stay honest (PERFORMANCE_PROGRESS.md). (Wasm function-table entries are not
        // reclaimable, but they become unreachable.)
        if on == 0 {
            #[allow(clippy::deref_addrof)]
            if let Some(j) = (*(&raw mut SYS_JIT)).as_mut() {
                j.clear();
            }
        }
    }
}

/// Set the tier-up threshold for performance diagnostics. Production keeps
/// the measured default unless the embedding configures this before boot.
#[no_mangle]
pub extern "C" fn jit_set_threshold(threshold: u32) {
    unsafe { JIT_THRESHOLD = threshold.max(1) }
}
/// Opt-in: fold branchy code pages into one superblock (function-per-page with
/// an internal br_table dispatch). Correct and validated, but per-page
/// granularity doesn't capture CPython's multi-page eval loop and the
/// recompile-on-new-entry cost regresses short warmups — off until whole-
/// function superblocks + incremental compilation land. See translate_superblock.
static mut SYS_SUPERBLOCK: bool = false;
#[no_mangle]
pub extern "C" fn sys_set_superblock(on: u32) {
    unsafe { SYS_SUPERBLOCK = on != 0 }
}
/// Max chained block dispatches before returning to the interpreter (keeps
/// interrupt/budget latency bounded in fully-jitted loops).
const JIT_CHAIN_CAP: u32 = 1024;
/// Once a code page accumulates this many hot NON-loop block entries it is
/// branchy enough (e.g. an interpreter's dispatch loop) to compile as one
/// superblock — one wasm function covering the whole page with an internal
/// br_table dispatch and registers cached in locals across all blocks.
/// Hot pcs on a page before it is compiled as one function. Low on purpose:
/// every individual block is its own WebAssembly module — a Module build, an
/// Instance, and a table growth each — so a page's worth of them costs far
/// more than the single page function that covers the same code (an in-guest
/// `tcc -c` built 8517 block modules against 54 page functions).
const SUPERBLOCK_THRESHOLD: usize = 6;
/// How many times one page may be recompiled as a superblock as more of it
/// turns out to be hot (see JitState::sb_gen).
const SB_RECOMPILE_CAP: u32 = 16;
/// Distinct (address space, page) discovery records kept before the whole
/// table is dropped — address spaces die and their pages go with them.
const SB_SPACE_CAP: usize = 16384;
/// Superblock/region builds are paced by their MEASURED host cost, not a
/// flat instruction gap: cumulative host-side build time (leader analysis,
/// register scan, wasm emission — the V8 module compile itself is async and
/// off-thread) may not exceed this fraction of wall time since the machine
/// started. The old flat 16M-insn spacing allowed ~20 builds over an entire
/// `tcc -c`, so the hot call graph's ~50 pages never got covered while the
/// workload still ran (measured: 34 landed, 8.0 insns/dispatch, extension
/// starved); a fraction-of-runtime budget lets a cold workload take a fast
/// burst of coverage while still bounding total compile cost on any run.
const SB_BUILD_BUDGET: f64 = 0.08;
/// Floor between two builds, in retired instructions. This is the old flat
/// spacing: at 1M the build/rebuild rate went up ~16x and python fib ran
/// 8.5s against 3.6s — rebuild churn discards V8-optimized page functions
/// (the measured FP EMULATION 3x cliff), so the wall-time budget alone is
/// NOT a sufficient pacing signal. The budget still caps pathological
/// translate storms below this floor's rate.
static mut SB_MIN_SPACING: u64 = 16_000_000;
#[no_mangle]
pub extern "C" fn jit_set_sb_spacing(m_insns: u32) {
    unsafe { SB_MIN_SPACING = m_insns as u64 * 1_000_000 }
}
static mut SB_BUILD_MS: f64 = 0.0;
static mut SB_ANCHOR_MS: f64 = -1.0;

/// May another region build be issued now? (Measured-cost budget above.)
#[allow(static_mut_refs)]
fn sb_build_allowed(insn_count: u64) -> bool {
    unsafe {
        if insn_count < SB_LAST_ICOUNT.wrapping_add(SB_MIN_SPACING) {
            return false;
        }
        if SB_ANCHOR_MS < 0.0 {
            SB_ANCHOR_MS = host_now_ms();
        }
        let elapsed = host_now_ms() - SB_ANCHOR_MS;
        // The +2ms grace admits the first builds while elapsed is still ~0.
        SB_BUILD_MS <= elapsed * SB_BUILD_BUDGET + 2.0
    }
}
/// Deferred superblock requests kept before new ones are dropped.
const SB_QUEUE_CAP: usize = 64;
/// Individually-compiled hot pcs on a superblocked page before its page
/// function is rebuilt to cover them.
const SB_MISSED_TRIGGER: u32 = 8;
/// Retired instructions a page must run before its FIRST rebuild; each further
/// rebuild doubles the wait (see the cooldown comment at the trigger).
const SB_PAGE_COOLDOWN: u64 = 8_000_000;
static mut SB_LAST_ICOUNT: u64 = 0;
/// Guest TLB misses served inside compiled code (jit_stat 31). In-block TLB
/// fill is off by default: it costs register pressure in every memory-op block
/// and buys ~10% on an in-guest `tcc -c` (whose symbol tables thrash the TLB)
/// while costing ~15% on CPython's eval loop, whose working set never misses.
/// Switching it from a measured miss rate was tried and made the CPython row
/// worse without flipping the tcc row, so the policy stays explicit:
/// jit_set_tlb_fill(1) for guests with a working set past the 4096-entry TLB.
static mut TLB_FILLS: u64 = 0;
/// JIT TLB-refill policy: 0 = forced off, 1 = forced on, 2 = adaptive.
/// Adaptive is the product default. It preserves the smaller register set for
/// locality-friendly workloads, then enables refill only after execution has
/// proved that first-operation TLB bails are material.
const TLB_FILL_OFF: u8 = 0;
const TLB_FILL_ON: u8 = 1;
const TLB_FILL_AUTO: u8 = 2;
static mut TLB_FILL_POLICY: u8 = TLB_FILL_AUTO;
const TLB_AUTO_BAIL_TRIGGER: u64 = 65_536;
const TLB_AUTO_PC_RECOMPILE: u8 = 8;

fn reset_tlb_fill_policy() {
    unsafe {
        rv64_jit::set_tlb_fill(TLB_FILL_POLICY == TLB_FILL_ON);
    }
}
/// Leaders per superblock. Every entry into the function loads the register
/// UNION over all its bodies and every exit stores the written union, so a
/// function that covers more of the page pays more on each entry — worth it
/// for code that then stays inside (IDEA), ruinous for code that re-enters
/// constantly (FOURIER's cross-page libm calls). Hot pcs are seeded first, so
/// the cap trims cold reachable code, not the hot core.
const MAX_LEADERS: usize = 512;

trait FullSystemJitBus: Bus {
    fn pending_host_io(&self) -> bool;
}

impl FullSystemJitBus for rv64_system::virt::VirtBus {
    #[inline]
    fn pending_host_io(&self) -> bool {
        self.virtio
            .iter()
            .any(rv64_system::virtio::VirtioDev::has_pending_block_request)
    }
}

/// Context passed to full-system compiled code. Generated modules treat this
/// as opaque and pass it back when they need a host TLB refill or a chained
/// dispatch. The callbacks make the concrete bus type explicit without tying
/// the generated-module ABI to either full-system machine implementation.
#[derive(Clone, Copy)]
#[repr(C)]
struct JitExecutionContext {
    cpu: *mut Cpu,
    bus: *mut (),
    jit: *mut JitState,
    tlb_fill: unsafe extern "C" fn(*mut Cpu, *mut (), u64, u32) -> i64,
    host_io_pending: unsafe fn(*const ()) -> bool,
}

// Generated full-system modules receive this opaque handle, never a linear-
// memory address. The concrete context lives in one dispatcher-owned slot so
// arbitrary exported-callback arguments cannot become Rust pointers.
const FULL_SYSTEM_CONTEXT_HANDLE: i32 = 1;
static mut ACTIVE_JIT_CONTEXT: Option<JitExecutionContext> = None;
static mut FULL_SYSTEM_DISPATCH_ACTIVE: bool = false;

impl JitExecutionContext {
    fn new<B: FullSystemJitBus>(cpu: &mut Cpu, bus: &mut B, jit: &mut JitState) -> Self {
        unsafe extern "C" fn fill<B: Bus>(cpu: *mut Cpu, bus: *mut (), va: u64, store: u32) -> i64 {
            unsafe {
                (*cpu)
                    .jit_fill_tlb(&mut *(bus.cast::<B>()), va, store != 0)
                    .unwrap_or(-1)
            }
        }

        unsafe fn pending<B: FullSystemJitBus>(bus: *const ()) -> bool {
            unsafe { (*bus.cast::<B>()).pending_host_io() }
        }

        Self {
            cpu,
            bus: (bus as *mut B).cast(),
            jit,
            tlb_fill: fill::<B>,
            host_io_pending: pending::<B>,
        }
    }

    unsafe fn fill_tlb(&mut self, va: u64, store: u32) -> i64 {
        unsafe { (self.tlb_fill)(self.cpu, self.bus, va, store) }
    }

    unsafe fn dispatch_parts(&mut self) -> (&mut Cpu, &mut JitState) {
        unsafe { (&mut *self.cpu, &mut *self.jit) }
    }

    unsafe fn pending_host_io(self) -> bool {
        unsafe { (self.host_io_pending)(self.bus.cast_const()) }
    }
}

/// Call a compiled block. The opaque state value deliberately escapes into
/// the call so the compiler reloads CPU fields afterwards instead of caching
/// them in locals. Full-system code receives `FULL_SYSTEM_CONTEXT_HANDLE`;
/// user and emitter tests receive their direct state address.
#[inline]
fn call_block(idx: i32, state: i32) {
    unsafe {
        // A Wasm trap can unwind past chain_next before it decrements the
        // depth. Each host dispatch starts a new, independent chain.
        CHAIN_DEPTH = 0;
        // The retirement cell is CUMULATIVE across one host dispatch: blocks
        // ADD what they retire (tail-call transfers keep accumulating without
        // returning here), so it must start each chain at zero.
        RETIRED_CELL = 0;
        let f: extern "C" fn(i32) = core::mem::transmute(idx as usize);
        f(state);
    }
}

// ---- full-system API -------------------------------------------------------

fn begin_full_system_dispatch() -> bool {
    unsafe {
        if FULL_SYSTEM_DISPATCH_ACTIVE {
            return false;
        }
        FULL_SYSTEM_DISPATCH_ACTIVE = true;
        true
    }
}

fn end_full_system_dispatch() {
    unsafe {
        ACTIVE_JIT_CONTEXT = None;
        FULL_SYSTEM_DISPATCH_ACTIVE = false;
    }
}

#[inline]
unsafe fn active_jit_context() -> Option<JitExecutionContext> {
    unsafe { core::ptr::addr_of!(ACTIVE_JIT_CONTEXT).read() }
}

/// Clear dispatcher-owned context after a Wasm trap escaped a run export.
/// The JavaScript ABI wrapper calls this raw export from its exception edge;
/// normal returns clear the same state inside `virt_run`.
#[no_mangle]
pub extern "C" fn full_system_dispatch_abort() {
    end_full_system_dispatch();
}

#[allow(static_mut_refs)]
unsafe fn reset_full_system_jit() {
    unsafe {
        end_full_system_dispatch();
        BOOT_GEN = BOOT_GEN.wrapping_add(1);
        PENDING_JIT.clear();
        if let Some(jit) = SYS_JIT.as_mut() {
            jit.clear();
        }
        JIT_RETIRED = 0;
        JIT_DISPATCHES = 0;
        SLICE_CALLS = 0;
        SLICE_INSNS = 0;
        JIT_CAPACITY_BLOCKED = false;
    }
}

macro_rules! stage_into {
    ($name:ident, $slot:ident) => {
        #[no_mangle]
        #[allow(static_mut_refs)]
        pub extern "C" fn $name() {
            unsafe {
                $slot = core::mem::take(&mut STAGING);
            }
        }
    };
}

// ---- modern virt-machine API (OpenSBI + current Linux) -------------------

static mut VIRT_OPENSBI: Vec<u8> = Vec::new();
static mut VIRT_KERNEL: Vec<u8> = Vec::new();
static mut VIRT_INITRD: Vec<u8> = Vec::new();
static mut VIRT_DISK: Vec<u8> = Vec::new();
static mut VIRT_EXTERNAL_DISK_SIZE: u64 = 0;
static mut VIRT_CMDLINE: Vec<u8> = Vec::new();
static mut VIRT_NET_ON: bool = false;
static mut VIRT_NET_MAC: Vec<u8> = Vec::new();
static mut VIRT_LAST_MONOTONIC_MS: f64 = 0.0;
static mut VIRT: Option<rv64_system::virt::VirtMachine> = None;

stage_into!(virt_stage_opensbi, VIRT_OPENSBI);
stage_into!(virt_stage_kernel, VIRT_KERNEL);
stage_into!(virt_stage_initrd, VIRT_INITRD);
stage_into!(virt_stage_disk, VIRT_DISK);
stage_into!(virt_stage_cmdline, VIRT_CMDLINE);
stage_into!(virt_stage_net_mac, VIRT_NET_MAC);

/// Give the next modern virt machine a virtio-net NIC.
#[no_mangle]
pub extern "C" fn virt_net_enable(on: u32) {
    unsafe { VIRT_NET_ON = on != 0 }
}

/// Configure a native disk image without copying its contents into Wasm.
#[no_mangle]
pub extern "C" fn virt_external_disk_size(size: u64) {
    unsafe { VIRT_EXTERNAL_DISK_SIZE = size }
}

/// Assemble and boot the modern virt machine from staged images.
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn virt_boot(ram_mb: u32) {
    boot_virt(ram_mb, false);
}

/// Assemble the modern virt machine and enter Linux directly in S-mode.
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn virt_boot_direct(ram_mb: u32) {
    boot_virt(ram_mb, true);
}

#[allow(static_mut_refs)]
fn boot_virt(ram_mb: u32, direct: bool) {
    unsafe {
        let cmdline = String::from_utf8_lossy(&VIRT_CMDLINE).into_owned();
        let cmdline = if cmdline.is_empty() {
            "console=ttyS0 root=/dev/vda rw"
        } else {
            &cmdline
        };
        let net = VIRT_NET_ON.then(|| {
            <[u8; 6]>::try_from(VIRT_NET_MAC.as_slice()).unwrap_or(rv64_system::virtio::DEFAULT_MAC)
        });
        let disk = (!VIRT_DISK.is_empty()).then(|| core::mem::take(&mut VIRT_DISK));
        let external_disk_size =
            (disk.is_none() && VIRT_EXTERNAL_DISK_SIZE != 0).then_some(VIRT_EXTERNAL_DISK_SIZE);
        let images = rv64_system::virt::VirtImages {
            opensbi: &VIRT_OPENSBI,
            kernel: &VIRT_KERNEL,
            cmdline,
            initrd: (!VIRT_INITRD.is_empty()).then_some(VIRT_INITRD.as_slice()),
            disk,
            external_disk_size,
            net,
        };
        let mut machine = if direct {
            rv64_system::virt::VirtMachine::new_direct(u64::from(ram_mb) << 20, images)
        } else {
            rv64_system::virt::VirtMachine::new(u64::from(ram_mb) << 20, images)
        };
        machine.set_rtc_unix_ns(host_unix_ms() as u64 * 1_000_000);
        VIRT_OPENSBI.clear();
        VIRT_KERNEL.clear();
        VIRT_INITRD.clear();
        VIRT_CMDLINE.clear();
        VIRT_EXTERNAL_DISK_SIZE = 0;
        VIRT = Some(machine);
        VIRT_LAST_MONOTONIC_MS = host_now_ms();
        reset_full_system_jit();
    }
}

/// Run one modern-machine slice and stream UART output to the host.
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn virt_run(max_insns: u64) -> i32 {
    if !begin_full_system_dispatch() {
        return 0;
    }
    let machine = unsafe { VIRT.as_mut().expect("call virt_boot() first") };
    let now = unsafe { host_now_ms() };
    let elapsed_ms = unsafe { (now - VIRT_LAST_MONOTONIC_MS).max(0.0) };
    unsafe { VIRT_LAST_MONOTONIC_MS = now };
    machine.advance_realtime_ns((elapsed_ms * 1_000_000.0) as u64);
    machine.set_rtc_unix_ns(unsafe { host_unix_ms() } as u64 * 1_000_000);
    let jit = unsafe { SYS_JIT.get_or_insert_with(JitState::new) };
    let result = run_full_system_jit(machine, jit, max_insns);
    end_full_system_dispatch();
    if machine.pending_block_request().is_some() {
        2
    } else {
        result
    }
}

/// Return the pending native disk operation kind: 1=read, 2=write, 3=flush.
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn virt_disk_request_kind() -> u32 {
    unsafe {
        VIRT.as_ref()
            .and_then(|machine| machine.pending_block_request())
            .map_or(0, |request| match request.kind {
                rv64_system::virtio::BlockRequestKind::Read => 1,
                rv64_system::virtio::BlockRequestKind::Write => 2,
                rv64_system::virtio::BlockRequestKind::Flush => 3,
            })
    }
}

#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn virt_disk_request_id() -> u64 {
    unsafe {
        VIRT.as_ref()
            .and_then(|machine| machine.pending_block_request())
            .map_or(0, |request| request.id)
    }
}

#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn virt_disk_request_offset() -> u64 {
    unsafe {
        VIRT.as_ref()
            .and_then(|machine| machine.pending_block_request())
            .map_or(0, |request| request.offset)
    }
}

#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn virt_disk_request_length() -> u64 {
    unsafe {
        VIRT.as_ref()
            .and_then(|machine| machine.pending_block_request())
            .map_or(0, |request| request.len())
    }
}

/// Copy the pending write body into the staging buffer and return its length.
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn virt_disk_request_body() -> *mut u8 {
    unsafe {
        let body = VIRT
            .as_ref()
            .and_then(|machine| machine.pending_block_request())
            .map(|request| request.data().to_vec())
            .unwrap_or_default();
        STAGING = body;
        STAGING.as_mut_ptr()
    }
}

/// Complete the pending native disk operation. The current staging buffer is
/// used as read data when `ok` is non-zero.
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn virt_disk_complete(ok: u32) -> u32 {
    unsafe {
        let data = core::mem::take(&mut STAGING);
        let Some(machine) = VIRT.as_mut() else {
            return 0;
        };
        let Some(request) = machine.pending_block_request() else {
            return 0;
        };
        machine.complete_block_request(request.id, &data, ok != 0) as u32
    }
}

#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn virt_console_input() {
    let machine = unsafe { VIRT.as_mut().expect("call virt_boot() first") };
    let bytes = unsafe { core::mem::take(&mut STAGING) };
    machine.console_input(&bytes);
}

/// Deliver one inbound Ethernet frame to the modern machine's NIC.
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn virt_net_input() {
    let machine = unsafe { VIRT.as_mut().expect("call virt_boot() first") };
    let frame = unsafe { core::mem::take(&mut STAGING) };
    machine.net_input(&frame);
}

#[allow(static_mut_refs)]
fn pump_virt_net(machine: &mut rv64_system::virt::VirtMachine) {
    for frame in machine.net_take_output() {
        emit_host_net(&frame);
    }
}

#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn virt_insn_count() -> u64 {
    unsafe { VIRT.as_ref().map(|m| m.cpu.insn_count).unwrap_or(0) }
}

/// Diagnostic direct-SBI call counter. Indexes are total, BASE, TIME, IPI,
/// RFENCE, HSM, SRST, and legacy/other.
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn virt_sbi_call_count(index: u32) -> u64 {
    unsafe {
        VIRT.as_ref()
            .and_then(|m| m.sbi_calls.get(index as usize))
            .copied()
            .unwrap_or(0)
    }
}

/// Current modern-machine guest PC (diagnostic: boot and workload profiling).
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn virt_pc() -> u64 {
    unsafe { VIRT.as_ref().map_or(0, |m| m.cpu.pc) }
}

/// Unsupported direct-boot SBI extension/function, or zero when none.
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn virt_unsupported_sbi_ext() -> u64 {
    unsafe {
        VIRT.as_ref()
            .and_then(|m| m.unsupported_sbi)
            .map_or(0, |v| v.0)
    }
}

#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn virt_unsupported_sbi_function() -> u64 {
    unsafe {
        VIRT.as_ref()
            .and_then(|m| m.unsupported_sbi)
            .map_or(0, |v| v.1)
    }
}

/// The machine operations used by the full-system JIT dispatcher. This trait
/// is private and statically dispatched: it defines the semantic boundary
/// without adding virtual calls to the execution path.
trait FullSystemJitMachine {
    type Bus: FullSystemJitBus;

    fn cpu(&self) -> &Cpu;
    fn cpu_mut(&mut self) -> &mut Cpu;
    fn cpu_bus_mut(&mut self) -> (&mut Cpu, &mut Self::Bus);
    fn ram(&self) -> &[u8];
    fn jit_pages(&self) -> &rv64_system::JitPageState;
    fn jit_pages_mut(&mut self) -> &mut rv64_system::JitPageState;

    fn run_interpreter_cached(
        &mut self,
        max_insns: u64,
        jit: &mut JitState,
        tier_up: bool,
    ) -> rv64_system::RunSliceOutcome;
    fn sync_jit_devices(&mut self);
    fn powered_off(&self) -> bool;
    fn refresh_jit_time(&mut self, force: bool);
    fn flush_host_io(&mut self);
    fn pending_host_io(&self) -> bool;

    #[inline]
    fn ram_range(&self, physical: u64, len: usize) -> Option<core::ops::Range<usize>> {
        rv64_system::checked_ram_range(self.ram().len(), rv64_system::RAM_BASE, physical, len)
    }

    #[inline]
    fn code_has_dirty(&self) -> bool {
        self.jit_pages().has_dirty()
    }

    #[inline]
    fn code_page_dirty(&self, page: u64) -> bool {
        self.jit_pages().is_dirty(page)
    }

    #[inline]
    fn code_page_marked(&self, page: u64) -> bool {
        self.jit_pages().page_marked(page)
    }

    #[inline]
    fn code_page_generation(&self, page: u64) -> Option<u64> {
        self.jit_pages().page_generation(page)
    }

    #[inline]
    fn code_mark_page(&mut self, pa: u64) {
        if self.jit_pages_mut().mark_address(pa) {
            self.cpu_mut().invalidate_store_jtlb_page(pa);
        }
    }

    #[inline]
    fn code_unmark_page(&mut self, page: u64) {
        self.jit_pages_mut().unmark_page(page);
    }

    #[inline]
    fn code_take_dirty(&mut self) -> Vec<u64> {
        self.jit_pages_mut().take_dirty()
    }

    #[inline]
    fn probe_fetch(&mut self, va: u64) -> Option<u64> {
        let (cpu, bus) = self.cpu_bus_mut();
        cpu.jit_probe_fetch(bus, va)
    }

    #[inline]
    fn check_interrupts(&mut self) {
        let (cpu, bus) = self.cpu_bus_mut();
        cpu.check_interrupts(bus);
    }

    fn execution_context(&mut self, jit: &mut JitState) -> JitExecutionContext {
        let (cpu, bus) = self.cpu_bus_mut();
        JitExecutionContext::new(cpu, bus, jit)
    }
}

impl FullSystemJitMachine for rv64_system::virt::VirtMachine {
    type Bus = rv64_system::virt::VirtBus;

    #[inline]
    fn cpu(&self) -> &Cpu {
        &self.cpu
    }

    #[inline]
    fn cpu_mut(&mut self) -> &mut Cpu {
        &mut self.cpu
    }

    #[inline]
    fn cpu_bus_mut(&mut self) -> (&mut Cpu, &mut Self::Bus) {
        (&mut self.cpu, &mut self.bus)
    }

    #[inline]
    fn ram(&self) -> &[u8] {
        &self.bus.ram
    }

    #[inline]
    fn jit_pages(&self) -> &rv64_system::JitPageState {
        &self.bus.jit
    }

    #[inline]
    fn jit_pages_mut(&mut self) -> &mut rv64_system::JitPageState {
        &mut self.bus.jit
    }

    #[inline]
    fn run_interpreter_cached(
        &mut self,
        max_insns: u64,
        jit: &mut JitState,
        tier_up: bool,
    ) -> rv64_system::RunSliceOutcome {
        let stop_capable = !jit.cache.is_empty();
        let aspace = self.cpu.sys.as_ref().map_or(0, |system| system.satp);
        let mut dispatch = InterpreterDispatch {
            jit,
            tier_up,
            aspace,
        };
        self.run_cached_slice_outcome(max_insns, &mut dispatch, stop_capable)
    }

    #[inline]
    fn sync_jit_devices(&mut self) {
        rv64_system::virt::VirtMachine::sync_jit_devices(self);
    }

    #[inline]
    fn powered_off(&self) -> bool {
        self.power_off
    }

    #[inline]
    fn refresh_jit_time(&mut self, _force: bool) {}

    fn flush_host_io(&mut self) {
        let out = self.console_output();
        if !out.is_empty() {
            emit_host_write(1, &out);
        }
        pump_virt_net(self);
    }

    #[inline]
    fn pending_host_io(&self) -> bool {
        self.bus.pending_host_io()
    }
}

/// The JIT's view of machine state (register file, fcsr, TLB tables, budget
/// cells) — identical for every translation of the current machine.
fn jit_layout(cpu: &Cpu) -> rv64_jit::JitLayout {
    let (lt, lo, st, so) = cpu.jit_ftlb_ptrs();
    rv64_jit::JitLayout {
        x_base: cpu.x.as_ptr() as u32,
        pc_addr: &cpu.pc as *const u64 as u32,
        mem: None,
        sys: Some(rv64_jit::SysMem {
            ftlb_load_tag: lt as u32,
            ftlb_load_off: lo as u32,
            ftlb_store_tag: st as u32,
            ftlb_store_off: so as u32,
            tlb_mask: (rv64_core::Cpu::jit_tlb_size() - 1) as u32,
            context_tag: cpu.jit_tlb_context_tag(),
            context_addr: cpu.jit_tlb_context_ptr() as u32,
        }),
        mem_profile: mem_profile_layout(),
        reg_stress: reg_stress(),
        reg_profile_base: reg_profile_base(),
        multi_latch: unsafe { MULTI_LATCH },
        retired_addr: retired_addr(),
        f_base: cpu.f.as_ptr() as u32,
        fcsr_addr: &cpu.fcsr as *const u32 as u32,
        fuel_addr: fuel_addr(),
        mstatus_addr: cpu.jit_mstatus_ptr() as u32,
        copystat_addr: copystat_addr(),
        chain_off_addr: chain_off_addr(),
        batch_base_addr: 0,
        dispatch_base: 0,
        dispatch_mask: 0,
        map_gen_addr: 0,
    }
}

/// Build (asynchronously) the superblock covering `vpage` in address space
/// `aspace`, whose current physical page is `pa_page`. Returns true if a
/// module was issued. Called from the compile path and, when the compile
/// budget deferred one, from the quantum boundary.
#[allow(static_mut_refs)]
fn build_superblock<M: FullSystemJitMachine>(
    m: &mut M,
    jit: &mut JitState,
    aspace: u64,
    vpage: u64,
    pa_page: u64,
    sb_compiles: u32,
) -> bool {
    let n_entries = jit
        .page_entries
        .get(&(aspace, vpage))
        .map_or(0, |e| e.len());
    // Enough individually-hot pcs: compile the page as
    // ONE function — but over the FULL statically
    // discovered leader set, not
    // just the hot seeds. That keeps intra-page control
    // flow inside the function (any discovered target
    // hits its br_table slot) without recompiling per
    // newly-hot entry. Loop headers are EXCLUDED: their
    // br_table slots fall to the exit default, so the
    // tight individual loop-region blocks keep owning
    // them.
    unsafe { SB_TRIGGER += 1 };
    // Assemble the region: the hot page plus its
    // virtually contiguous, RAM-backed neighbours, so
    // control flow that leaves the page still lands
    // inside the same wasm function.
    let ram_ok = |m: &M, pa: u64| m.ram_range(pa & !0xfff, 0x1000).is_some();
    // Assemble the region from the CALL GRAPH, not from
    // address adjacency: pages join when the code already
    // in the region calls into them and they are hot
    // (page_entries non-empty), plus the contiguous next/
    // previous page when hot code sits within a block's
    // reach of the shared edge (loops straddle page
    // boundaries; calls do not care about distance). The
    // sparse translator resolves a target to (page, slot)
    // with one compare per page, so a caller and a callee
    // hundreds of KB apart still transfer inside one
    // function — the compile row's 9 insns per host
    // dispatch were exactly these cross-page calls.
    const EDGE: u64 = 0x80;
    let seeds = jit.page_entries[&(aspace, vpage)].clone();
    let hot = |jit: &JitState, va: u64| {
        jit.page_entries
            .get(&(aspace, va))
            .is_some_and(|e| !e.is_empty())
    };
    let mut pages: Vec<(u64, u64)> = vec![(vpage, pa_page)];
    let probe_add = |m: &mut M, pages: &mut Vec<(u64, u64)>, va: u64| {
        if pages.len() >= MAX_REGION_PAGES || pages.iter().any(|&(v, _)| v == va) {
            return;
        }
        if let Some(p) = m.probe_fetch(va) {
            if ram_ok(m, p) {
                pages.push((va, p & !0xfff));
            }
        }
    };
    if seeds.iter().any(|&e| (e & 0xfff) >= 0x1000 - EDGE) {
        probe_add(m, &mut pages, vpage + 0x1000);
    }
    if vpage >= 0x1000 && seeds.iter().any(|&e| (e & 0xfff) < EDGE) {
        probe_add(m, &mut pages, vpage - 0x1000);
    }
    // Contiguous hot neighbours first (the configuration
    // that measured 11/13), THEN up to two call-graph
    // joins — cross-page calls only pay when the callee
    // is genuinely hot, and gluing more than a couple of
    // far pages regressed CPython 3x.
    let mut va = vpage + 0x1000;
    while pages.len() < MAX_REGION_PAGES && hot(jit, va) {
        let before = pages.len();
        probe_add(m, &mut pages, va);
        if pages.len() == before {
            break;
        }
        va += 0x1000;
    }
    let mut va = vpage.wrapping_sub(0x1000);
    while va < vpage && pages.len() < MAX_REGION_PAGES && hot(jit, va) {
        let before = pages.len();
        probe_add(m, &mut pages, va);
        if pages.len() == before {
            break;
        }
        va = va.wrapping_sub(0x1000);
    }
    // Far (call-graph) pages join only under MEASURED
    // pressure: a first build stays contiguous — exactly
    // the configuration that held 11/13 — and rebuilds
    // pull in call targets once this page's misses prove
    // cross-page traffic. Reachability alone glued cold
    // callees into hot regions and regressed the FP rows.
    let missed_now = jit.sb_missed.get(&(aspace, vpage)).copied().unwrap_or(0);
    // With regions capped at 3 pages the far joins are
    // cheap and a19ea3b-measured; the miss gate was
    // compensating for the (now reverted) 8-page growth.
    // Far (call-graph) joins are DISABLED: they have never
    // demonstrated a win, and in a back-to-back sample on
    // an identically loaded host, regions without them ran
    // FP EMULATION at 1971 MIPS against 896 for the
    // baseline JIT. Gluing a callee page in costs every
    // entry a bigger register union and V8 a bigger
    // function; the compile row's cross-page calls need
    // regions that EXTEND on measured misses (see the
    // historical incremental-extension design (see PERFORMANCE_PROGRESS.md), not
    // regions that guess from reachability. The selection
    // code stays — it is one predicate away from being
    // re-enabled behind that signal.
    let _ = missed_now;
    let far_cap = pages.len();
    let mut scanned = 0usize;
    while scanned < pages.len() && pages.len() < far_cap {
        let (va, pp) = pages[scanned];
        scanned += 1;
        let range = m
            .ram_range(pp, 0x1000)
            .expect("region pages were validated before leader discovery");
        let targets = rv64_jit::page_call_targets(&m.ram()[range], va);
        for t in targets {
            if hot(jit, t & !0xfff) {
                probe_add(m, &mut pages, t & !0xfff);
            }
        }
    }
    // Only a rebuild that covered nothing new counts against the allowance:
    // a page whose hot set is still growing must be able to keep up, or code
    // that gets hot late is stranded on individual blocks forever.
    let prev = jit.sb_gen.get(&(aspace, vpage)).map_or(0, |&(e, _, _)| e);
    issue_region(
        m,
        jit,
        aspace,
        vpage,
        pages,
        sb_compiles,
        n_entries,
        n_entries <= prev,
        false,
    )
}

/// Translate `pages` as one sparse region function and issue it for ASYNC
/// compilation on V8's background threads (the sync Module build of a page
/// function stalls the guest for ms — the cold-compile cost that kept
/// superblocks gated). Execution continues on whatever is installed NOW —
/// individual blocks or a previous region function — and sys_sb_ready
/// repoints the entries only once the new function is in the table, after
/// re-validating page identity. Never uninstalls anything early: the gap
/// between issue and landing running on individual blocks was the measured
/// FP EMULATION 2568 -> 550 MIPS rebuild cliff.
///
/// `lead` keys the build cooldown (sb_gen): the page whose threshold crossing
/// owns this region, across rebuilds AND extensions.
#[allow(clippy::too_many_arguments)]
#[allow(static_mut_refs)]
fn issue_region<M: FullSystemJitMachine>(
    m: &mut M,
    jit: &mut JitState,
    aspace: u64,
    lead: u64,
    pages: Vec<(u64, u64)>,
    sb_compiles: u32,
    n_entries: usize,
    unproductive: bool,
    regs_in_memory: bool,
) -> bool {
    if !full_system_jit_issue_allowed() {
        return false;
    }
    // The build budget (sb_build_allowed) charges every issue attempt its
    // real host cost, translate failures included.
    let t0 = unsafe { host_now_ms() };
    let r = issue_region_inner(
        m,
        jit,
        aspace,
        lead,
        pages,
        sb_compiles,
        n_entries,
        unproductive,
        regs_in_memory,
    );
    unsafe { SB_BUILD_MS += host_now_ms() - t0 };
    r
}

#[allow(clippy::too_many_arguments)]
#[allow(static_mut_refs)]
fn issue_region_inner<M: FullSystemJitMachine>(
    m: &mut M,
    jit: &mut JitState,
    aspace: u64,
    lead: u64,
    mut pages: Vec<(u64, u64)>,
    sb_compiles: u32,
    n_entries: usize,
    unproductive: bool,
    regs_in_memory: bool,
) -> bool {
    let mut lay = jit_layout(m.cpu());
    lay.dispatch_base = jit.dispatch.as_ptr() as u32;
    lay.dispatch_mask = (DISPATCH_SIZE - 1) as u32;
    lay.map_gen_addr = m.cpu().jit_map_gen_ptr() as u32;
    // Ascending order keeps virtually contiguous pages adjacent in the
    // concat, which is what lets bodies flow across their shared boundary.
    pages.sort_unstable_by_key(|&(va, _)| va);
    let mut code = Vec::with_capacity(pages.len() * 0x1000);
    for &(_, pp) in &pages {
        let range = m
            .ram_range(pp, 0x1000)
            .expect("issued region pages must belong to guest RAM");
        code.extend_from_slice(&m.ram()[range]);
    }
    let vas: Vec<u64> = pages.iter().map(|&(va, _)| va).collect();
    // Leader discovery per CONTIGUOUS RUN of pages, from the union of the
    // run's recorded hot pcs: static reachability crosses page boundaries,
    // so a seed on one page discovers the leaders of its neighbours. The
    // per-page variant that briefly lived here (a bisect configuration from
    // the invalidated 2026-07-25 session, labeled SUB-BISECT(i)) silently
    // skipped every page with no seeds of its own — regions covered
    // fragments, exits dominated, and the branchy-int kernels lost a third
    // to half their throughput (ASSIGNMENT 12.6 -> 8.0, HUFFMAN 1525 -> 950
    // against the 11/13-era JIT).
    let mut entries: Vec<u64> = Vec::new();
    let mut i = 0usize;
    while i < pages.len() && entries.len() < MAX_LEADERS {
        let mut j = i;
        while j + 1 < pages.len() && pages[j + 1].0 == pages[j].0 + 0x1000 {
            j += 1;
        }
        let run_slice = &code[i * 0x1000..(j + 1) * 0x1000];
        let run_va = pages[i].0;
        let run_span = ((j - i + 1) * 0x1000) as u64;
        let mut rseeds: Vec<u64> = Vec::new();
        for &(va, _) in &pages[i..=j] {
            if let Some(v) = jit.page_entries.get(&(aspace, va)) {
                rseeds.extend_from_slice(v);
            }
        }
        if !rseeds.is_empty() {
            let (mut l, back) = rv64_jit::discover_page_leaders_ext(
                run_slice,
                run_va,
                run_va,
                run_span,
                &rseeds,
                MAX_LEADERS - entries.len(),
            );
            l.retain(|&e| {
                rv64_jit::emittable_at(run_slice, run_va, e, lay)
                    && (!back.contains(&e) || !rv64_jit::is_loop_at(run_slice, run_va, e, lay))
            });
            entries.extend(l);
        }
        i = j + 1;
    }
    let sb = rv64_jit::translate_superblock_sparse(&code, &vas, &entries, lay, regs_in_memory);
    if sb.is_none() {
        unsafe { SB_XLATE_FAIL += 1 };
        return false;
    }
    let blk = sb.unwrap();
    unsafe { JIT_OUT = blk.wasm };
    for &(_, pp) in &pages {
        m.code_mark_page(pp);
    }
    let page_generations = pages
        .iter()
        .map(|&(_, pp)| {
            let page = (pp - rv64_system::RAM_BASE) >> 12;
            m.code_page_generation(page)
                .expect("compiled page must belong to guest RAM")
        })
        .collect();
    let claim_pages = pages.clone();
    let pending = PendingJitKind::Region(PendingRegion {
        aspace,
        lead,
        pages,
        page_generations,
        entries,
    });
    let Some(ticket) = submit_pending_jit(pending) else {
        return false;
    };
    // Every page the region covers is claimed by the latest pending build.
    // A newer overlapping build supersedes this ticket without losing the
    // state that existed before either build started.
    jit.claim_pending_superblock(ticket, aspace, &claim_pages);
    for &(pva, _) in &claim_pages {
        jit.sb_missed.remove(&(aspace, pva));
    }
    // The recorded instruction count starts the lead page's build cooldown.
    jit.sb_gen.insert(
        (aspace, lead),
        (
            n_entries,
            sb_compiles + u32::from(unproductive),
            m.cpu().insn_count,
        ),
    );
    unsafe {
        SB_ISSUED += 1;
        SB_LAST_ICOUNT = m.cpu().insn_count;
    }
    // The caller still gives its pc an individual block right now; the
    // region function repoints the entries when the module arrives.
    true
}

/// Record one sampled exit of a landed region function: `target` is the pc
/// the function published on its way out. Out-of-region targets accumulate
/// per page; crossing EXT_TRIGGER queues the region for measured extension.
#[inline(never)]
fn record_region_exit(jit: &mut JitState, idx: i32, target: u64, stay: u64) {
    unsafe { SB_EXIT_SAMPLED += 1 };
    let mut queue = false;
    let mut demote = false;
    if let Some(r) = jit.region_exits.get_mut(&idx) {
        if stay > 0 {
            r.samples = r.samples.saturating_add(1);
            r.stay_sum = r.stay_sum.saturating_add(stay);
        }
        // The demotion verdict: enough evidence, and the function's visits
        // are too short to pay for their entries.
        if r.samples == DEMOTE_MIN_SAMPLES
            && r.stay_sum / (r.samples as u64) < DEMOTE_STAY
            && unsafe { DEMOTE_ON }
        {
            demote = true;
        }
        let tp = target & !0xfff;
        if !demote {
            if r.pages.iter().any(|&(va, _)| va == tp) {
                unsafe { SB_EXIT_INREGION += 1 };
                return; // in-region uncovered pc: sb_missed/rebuild owns that
            }
            r.total = r.total.saturating_add(1);
            if let Some(t) = r.targets.iter_mut().find(|t| t.0 == tp) {
                t.1 = t.1.saturating_add(1);
            } else if r.targets.len() < EXT_TARGET_CAP {
                r.targets.push((tp, 1));
            }
            queue = r.total == EXT_TRIGGER;
        }
    } else {
        unsafe { SB_EXIT_NOMAP += 1 };
    }
    if demote {
        demote_region(jit, idx);
        return;
    }
    if queue && !jit.ext_queue.contains(&idx) && jit.ext_queue.len() < SB_QUEUE_CAP {
        jit.ext_queue.push(idx);
        unsafe { SB_EXT_PUSHED += 1 };
    }
}

/// Un-claim a region function that measurably does not hold execution: its
/// entry pcs go back to individual (trace) blocks — they are hot, so the
/// interp-stretch counters re-tier them within microseconds — and the lead
/// page's build allowance is spent so the page function does not come back.
#[allow(static_mut_refs)]
fn demote_region(jit: &mut JitState, idx: i32) {
    let Some(r) = jit.region_exits.remove(&idx) else {
        return;
    };
    unsafe { SB_DEMOTED += 1 };
    for &e in &r.entries {
        if matches!(jit.cache.get(&e), Some(Some(b)) if b.idx == idx) {
            jit.cache_remove(&e);
            let slot = JitState::dslot(e);
            if jit.dispatch[slot].pc == e {
                jit.dispatch[slot].pc = NO_PC;
            }
        }
    }
    jit.regions.remove(&idx);
    jit.ext_queue.retain(|&i| i != idx);
    // Spend the allowance for every page the region covered: rebuilds check
    // `n_entries > sb_last || compiles < CAP`, so a huge sb_last plus a
    // capped compile count keeps both arms false.
    for &(va, _) in &r.pages {
        jit.sb_gen
            .insert((r.aspace, va), (usize::MAX / 2, SB_RECOMPILE_CAP, 0));
        jit.sb_missed.remove(&(r.aspace, va));
    }
}

/// Pop and build one queued extension whose region belongs to the CURRENT
/// address space. Called from the quantum boundary AND from the chain-break
/// fall-through: the boundary alone almost never runs during dispatch-miss-
/// heavy code (chains break long before the cap) and usually lands in kernel
/// moments where no queued aspace matches — extension starved at 5 builds
/// against 158k measured out-of-region exits until the fall-through call.
#[allow(static_mut_refs)]
fn drain_ext_queue<M: FullSystemJitMachine>(m: &mut M, jit: &mut JitState) {
    if jit.ext_queue.is_empty()
        || !full_system_jit_issue_allowed()
        || m.cpu().insn_count < unsafe { SB_EXT_NEXT_ICOUNT }
        || !sb_build_allowed(m.cpu().insn_count)
    {
        return;
    }
    unsafe { SB_EXT_DRAIN_VISITS += 1 };
    let aspace = m.cpu().sys.as_ref().map_or(0, |c| c.satp);
    if let Some(i) = jit.ext_queue.iter().position(|idx| {
        jit.region_exits
            .get(idx)
            .is_some_and(|r| r.aspace == aspace)
    }) {
        let idx = jit.ext_queue.remove(i);
        try_extend_region(m, jit, idx);
    } else {
        unsafe { SB_EXT_DRAIN_NOMATCH += 1 };
        // Nothing for this address space: back off before rescanning, and
        // drop entries that no longer resolve at all.
        unsafe { SB_EXT_NEXT_ICOUNT = m.cpu().insn_count + SB_MIN_SPACING };
        let JitState {
            ext_queue,
            region_exits,
            ..
        } = jit;
        ext_queue.retain(|idx| region_exits.contains_key(idx));
    }
}

/// Grow a region along its measured exit traffic: rebuild it over the old
/// page set plus the hottest out-of-region exit-target pages, asynchronously;
/// the old function keeps running until the superset lands. This is what a
/// build-time selection could never do for tcc-shaped code — a caller and a
/// callee 16KB apart join only when dispatches actually flow between them.
fn try_extend_region<M: FullSystemJitMachine>(m: &mut M, jit: &mut JitState, idx: i32) {
    let Some(r) = jit.region_exits.get(&idx) else {
        return;
    };
    let (aspace, lead) = (r.aspace, r.lead);
    let old_pages = r.pages.clone();
    let mut targets = r.targets.clone();
    // The measured average stay picks the register mode for the superset:
    // short stays (call-shaped code) go memory-direct so entries cost
    // nothing; long stays keep the locals that make loops fast.
    let avg_stay = r.stay_sum / r.samples.max(1) as u64;
    let regs_in_memory = avg_stay < EXT_MEMORY_MODE_STAY;
    // Build cooldown keyed to the REGION (its lead page), not each member.
    let (_, compiles, when) = jit
        .sb_gen
        .get(&(aspace, lead))
        .copied()
        .unwrap_or((0, 0, 0));
    let cooldown = SB_PAGE_COOLDOWN << compiles.min(6);
    if m.cpu().insn_count < when.wrapping_add(cooldown) || compiles >= SB_RECOMPILE_CAP {
        // Not yet (or allowance spent): let the counters re-arm — the next
        // EXT_TRIGGER crossing re-queues it.
        unsafe { SB_EXT_DEFER_COOL += 1 };
        if let Some(r) = jit.region_exits.get_mut(&idx) {
            r.total = EXT_TRIGGER / 2;
        }
        return;
    }
    // The old pages carry their recorded pas; a page remapped since then is
    // caught at landing (marked/dirty) and at first dispatch (pa-verify), so
    // no probe — a probe here would fail on privilege whenever the build slot
    // lands inside the kernel, which is most of the time.
    let mut pages: Vec<(u64, u64)> = old_pages;
    // Hottest measured targets first; only pages with recorded hot code join
    // (a target with no page_entries has nothing to discover leaders from).
    // A target's pa comes from any of its already-compiled blocks — again no
    // probe; a target with no pa-carrying cache entry is skipped.
    targets.sort_unstable_by_key(|&(_, c)| core::cmp::Reverse(c));
    let mut added = 0usize;
    for &(tp, _) in &targets {
        if pages.len() >= MAX_EXT_REGION_PAGES {
            break;
        }
        if pages.iter().any(|&(v, _)| v == tp) {
            continue;
        }
        let Some(entries) = jit.page_entries.get(&(aspace, tp)) else {
            continue;
        };
        let Some(pa) = entries.iter().find_map(|e| {
            jit.cache
                .get(e)
                .and_then(|b| b.as_ref())
                .filter(|b| b.idx >= 0)
                .map(|b| b.pa & !0xfff)
        }) else {
            continue;
        };
        if m.ram_range(pa, 0x1000).is_some() {
            pages.push((tp, pa));
            added += 1;
        }
    }
    if added == 0 {
        unsafe { SB_EXT_NO_TARGET += 1 };
        if let Some(r) = jit.region_exits.get_mut(&idx) {
            r.total = EXT_TRIGGER / 2;
        }
        return;
    }
    // Consume the profile: the old function keeps running (and keeps its
    // regions pa-verify entry) but stops sampling; the superset starts a
    // fresh profile when it lands.
    jit.region_exits.remove(&idx);
    let n_entries = jit.page_entries.get(&(aspace, lead)).map_or(0, |e| e.len());
    if issue_region(
        m,
        jit,
        aspace,
        lead,
        pages,
        compiles,
        n_entries,
        false,
        regs_in_memory,
    ) {
        unsafe { SB_EXT_ISSUED += 1 };
    }
}

#[inline]
fn account_compiled_dispatch(
    retired_sum: &mut u64,
    chained: &mut u32,
    retired: u64,
    stop_requested: bool,
) -> bool {
    *retired_sum += retired;
    *chained += 1;
    stop_requested || retired == 0
}

/// Feed a zero-progress compiled entry into the adaptive TLB-refill policy.
/// Returns true when the current entry was removed for one refill-enabled
/// recompilation, so the chain must return to the tier-up path.
fn observe_tlb_bail(jit: &mut JitState, cpu: &Cpu, pc: u64) -> bool {
    unsafe {
        if TLB_FILL_POLICY != TLB_FILL_AUTO {
            return false;
        }
    }

    // FP blocks deliberately bail before their first instruction while the
    // architectural FP gate is closed. That is not a memory-locality signal.
    let uses_fp = matches!(jit.cache.get(&pc), Some(Some(block)) if block.fp);
    if uses_fp {
        let fs = cpu
            .sys
            .as_ref()
            .map_or(3, |system| (system.mstatus >> 13) & 3);
        if cpu.fcsr & 1 == 0 || (cpu.fcsr >> 5) & 7 != 0 || fs != 3 {
            return false;
        }
    }

    jit.tlb_bail_total = jit.tlb_bail_total.saturating_add(1);
    let slot = JitState::dslot(pc);
    let entry = &mut jit.tlb_bails[slot];
    if entry.0 != pc {
        *entry = (pc, 1);
    } else if entry.1 != u8::MAX {
        entry.1 = entry.1.saturating_add(1);
    }

    if !jit.tlb_auto_enabled && jit.tlb_bail_total >= TLB_AUTO_BAIL_TRIGGER {
        rv64_jit::set_tlb_fill(true);
        jit.tlb_auto_enabled = true;
    }
    if !jit.tlb_auto_enabled || entry.1 < TLB_AUTO_PC_RECOMPILE || entry.1 == u8::MAX {
        return false;
    }

    // Recompile a repeatedly bailing entry once. A page router has unchecked
    // private links, so retire that complete owner as one coherent unit.
    entry.1 = u8::MAX;
    let block_idx = jit.cache.get(&pc).and_then(Option::as_ref).map(|b| b.idx);
    if !block_idx.is_some_and(|idx| jit.retire_page_module_for_slot(idx)) {
        jit.cache_remove(&pc);
        if jit.dispatch[slot].pc == pc {
            jit.dispatch[slot].pc = NO_PC;
        }
    }
    true
}

#[allow(clippy::needless_range_loop)] // avoids references to mutable profiling statics
#[allow(static_mut_refs)]
fn run_full_system_jit<M: FullSystemJitMachine>(
    m: &mut M,
    jit: &mut JitState,
    max_insns: u64,
) -> i32 {
    begin_host_event_batch();
    unsafe {
        JIT_CAPACITY_BLOCKED = false;
        JIT_ISSUES_THIS_RUN = 0;
    }
    // SysCsrs remains public for machine integration. Synchronize the packed
    // live context at the ABI boundary so direct setup changes cannot leave
    // compiled memory probes observing an old privilege state.
    m.cpu_mut().refresh_jit_tlb_context();
    let context = m.execution_context(jit);
    unsafe { ACTIVE_JIT_CONTEXT = Some(context) };
    let mut remaining = max_insns;

    // Preserve the machine slice contract before the first compiled block:
    // consume host-arrived device work, publish interrupt lines, and take any
    // pending interrupt before guest code runs.
    m.refresh_jit_time(true);
    FullSystemJitMachine::sync_jit_devices(m);
    m.check_interrupts();

    'run: while remaining > 0 && !m.powered_off() {
        // Refresh the wall-clock time source (opt-in) so the CLINT tracks real
        // host time. host_now_ms is a wasm->JS round-trip (~7% of a dispatch-
        // heavy workload if done per iteration), so gate it: refresh only after
        // ~16k retired insns (~40us at JIT speed, far finer than the 10ms kernel
        // tick) or after 64 iterations without insn progress (WFI idle — time
        // must still advance or timers never fire).
        m.refresh_jit_time(false);
        if m.pending_host_io() {
            break;
        }
        // Address-space switch (satp write): compiled blocks SURVIVE — they're
        // va-keyed and every dispatch lazily re-verifies its va→pa mapping when
        // cpu.map_gen moved, so a block whose va now maps elsewhere (or nowhere)
        // is dropped at dispatch, and kernel/global mappings keep their blocks
        // across every context switch (recompiling the working set per switch
        // was a large fraction of boot time). Only va-keyed state that could
        // POISON a different address space at the same va must go: blacklist
        // entries (a va untranslatable in space A may be hot compilable code in
        // space B) and superblock page-entry lists (block starts from A are
        // arbitrary byte offsets in B).
        if m.cpu().jit_flush_gen != jit.flush_gen {
            jit.flush_gen = m.cpu().jit_flush_gen;
            // Superblock discovery state is keyed by (satp, virtual page), so a
            // context switch no longer throws it away. It used to: every satp
            // write cleared every page's hot-pc list, and since those pcs were
            // by then compiled (and so never re-registered), a page could never
            // grow its superblock again — whether a page ended up covered came
            // down to how many switches happened while it was warming up, which
            // is why nbench IDEA measured 1538 or 4594 iter/s from identical
            // runs. Bound the map instead: address spaces come and go.
            if jit.page_entries.len() > SB_SPACE_CAP {
                jit.page_entries.clear();
                jit.superblocked.clear();
                jit.pending_superblocks.clear();
                jit.sb_gen.clear();
            }
            if jit.page_hot_entries.len() > SB_SPACE_CAP {
                jit.page_hot_entries.clear();
                jit.page_modules
                    .retain(|_, state| !matches!(state, PageModuleState::Attempted(_)));
            }
        }
        // Per-page invalidation: drop only blocks whose physical code page
        // was written (self-modifying code / recycled pages), and clear only
        // their dispatch lines (a full dispatch memset is megabytes per event).
        if m.code_has_dirty() {
            let dirty = m.code_take_dirty();
            let mut dirty_pending_pages: std::collections::HashSet<(u64, u64)> = Default::default();
            let mut dirty_cached_vpages: std::collections::HashSet<u64> = Default::default();
            unsafe {
                DIRTY_EVENTS += dirty.len() as u64;
                // The trace-window gathers may hold pre-store bytes.
                TRACE_WIN.clear();
            }
            for &ppage in &dirty {
                dirty_pending_pages.extend(jit.tracked_page_keys_for_physical(ppage));
                let staged_blocks = core::mem::take(&mut jit.confirmed_stage);
                for staged in staged_blocks {
                    let source_dirty = staged.pending.pages.iter().any(|&(_, physical)| {
                        physical
                            .checked_sub(rv64_system::RAM_BASE)
                            .is_some_and(|offset| offset >> 12 == ppage)
                    });
                    if source_dirty {
                        jit.remove_confirmed_coverage(&staged.pending);
                    } else {
                        jit.confirmed_stage.push(staged);
                    }
                }
                if let Some(pcs) = jit.page_blocks.remove(&ppage) {
                    for pc in pcs {
                        unsafe { DIRTY_DROPPED += 1 };
                        dirty_cached_vpages.insert(pc & !0xfff);
                        jit.cache_remove(&pc);
                        let slot = JitState::dslot(pc);
                        if jit.dispatch[slot].pc == pc {
                            jit.dispatch[slot].pc = NO_PC;
                        }
                    }
                }
                m.code_unmark_page(ppage);
            }
            // Re-discover superblock entries for the pages whose code bytes
            // changed (any address space that mapped them), not globally.
            if !dirty_pending_pages.is_empty() || !dirty_cached_vpages.is_empty() {
                jit.invalidate_superblock_state(&dirty_pending_pages, &dirty_cached_vpages);
            }
        }
        if unsafe { CONFIRMED_BATCH_ON } && submit_confirmed_stage(jit) {
            break;
        }
        // --- JIT fast path: direct-mapped dispatch + cheap pa-verify ---
        // Per-dispatch bookkeeping accumulates in LOCALS and flushes once after
        // the chain: at ~200M+ dispatches per second of guest compute, the five
        // read-modify-writes this loop used to do per iteration (insn_count,
        // remaining, two stat counters, chain counter) were a measurable slice
        // of total wall time. map_gen is hoisted too — blocks can't execute
        // satp/SFENCE (SYSTEM never compiles; blocks bail AT it), so it cannot
        // move inside a chain.
        let map_gen = m.cpu().map_generation();
        let mut chained = 0u32;
        let mut retired_sum = 0u64;
        // Budget/interrupt contract: this round may retire at most
        // min(remaining, INTERRUPT_QUANTUM) instructions (to block/iteration
        // granularity); each dispatch is granted the leftover as loop fuel.
        let round_budget = remaining.min(INTERRUPT_QUANTUM);
        // The fuel cell is only consulted by loop/region blocks; refreshing
        // it on every dispatch is a store per ~13-insn block. Refresh every
        // 8 dispatches or 4K retired — staleness overshoots the round by at
        // most that, within the documented block-granularity tolerance
        // The full-system dispatcher owns one store for the active VM.
        unsafe { FUEL_CELL = round_budget };
        let mut fuel_stored_at = 0u64;
        while chained < JIT_CHAIN_CAP && retired_sum < round_budget {
            if chained & 7 == 0 || retired_sum.wrapping_sub(fuel_stored_at) > 4096 {
                unsafe { FUEL_CELL = round_budget - retired_sum };
                fuel_stored_at = retired_sum;
            }
            let pc = m.cpu().pc;
            let slot = JitState::dslot(pc);
            // Fast path: line hit AND no mapping event since it verified —
            // one 16-byte load and two compares, then straight to the call.
            // Any other case (miss, or SFENCE.VMA/satp moved cpu.map_gen)
            // resolves through the authoritative cache with a fetch-TLB
            // probe and refills the line stamped with the current generation
            // — so the cache survives the frequent data-page SFENCEs of
            // malloc-heavy processes (one re-probe per block per event, not
            // a flush).
            let line = jit.dispatch[slot];
            let idx = if line.pc == pc && line.gen == map_gen {
                line.idx
            } else {
                match jit.cache.get(&pc) {
                    Some(Some(b)) => {
                        let b = *b;
                        // Multi-page code: every page it was compiled over
                        // must still map where it did. Region functions AND
                        // page-crossing trace blocks both record their page
                        // sets here; single-page blocks miss (fast).
                        let region = jit.regions.get(&b.idx).cloned();
                        let self_ok = matches!(m.probe_fetch(pc), Some(pa) if pa == b.pa);
                        let region_ok = region.is_none_or(|pgs| {
                            pgs.iter().all(|&(va, pp)| {
                                matches!(m.probe_fetch(va), Some(q) if q & !0xfff == pp)
                            })
                        });
                        let mapped = self_ok && region_ok;
                        if !mapped {
                            unsafe {
                                if !self_ok {
                                    DROP_SELF += 1;
                                } else {
                                    DROP_REGION += 1;
                                }
                            }
                            if !jit.retire_page_module_for_slot(b.idx) {
                                jit.cache_remove(&pc);
                                jit.dispatch[slot].pc = NO_PC;
                            }
                            break;
                        }
                        // Region functions (n == 0) carry SB_IDX_BIT in their
                        // dispatch line so the exit below can be attributed
                        // without a cache probe (blacklist -1 keeps its sign).
                        let tagged = jit.tagged_dispatch_index(b);
                        jit.dispatch[slot] = DispatchLine {
                            pc,
                            idx: tagged,
                            gen: map_gen,
                        };
                        tagged
                    }
                    _ => break, // uncompiled or blacklisted
                }
            };
            if idx < 0 {
                break; // blacklisted (pa-verified for the current mapping)
            }
            if chained & 0xff == 0 {
                jit.touch(pc);
            }
            call_block(idx & !IDX_TAG_MASK, FULL_SYSTEM_CONTEXT_HANDLE);
            // Compiled memory operations can access RAM only. MMIO misses bail
            // before the access, so a JIT block cannot create a host disk
            // request. Check host I/O once after the chain instead of scanning
            // the virtio devices after every short compiled block.
            let mut stop_after_dispatch = false;
            // Observed successor + stability count (JitState::succ). A
            // trace ends at its first indirect jump, so this records where
            // that jump actually goes. Once the target is proven stable,
            // drop the block ONCE so it recompiles with an inline cache
            // that continues through the edge — trace EXTENSION at a hot
            // side exit, the mechanism that reduces dispatch COUNT for
            // indirect-heavy code. (The oracle is empty at first compile
            // by construction: the pc has never dispatched yet.)
            if idx & PAGE_IDX_BIT == 0 {
                let sl = JitState::dslot(pc);
                let e = &mut jit.succ[sl];
                // A pc is extended at most once. Freeze its direct-mapped
                // successor row afterwards instead of updating a saturated
                // counter on every future dispatch. An alias replaces the row
                // normally and therefore still receives a fresh profile.
                if e.0 != pc {
                    *e = (pc, m.cpu().pc, 1);
                } else if e.2 != u32::MAX {
                    if e.1 == m.cpu().pc {
                        e.2 = e.2.saturating_add(1);
                    } else {
                        *e = (pc, m.cpu().pc, 1);
                    }
                    if e.2 == unsafe { IC_EXTEND_TRIGGER } {
                        e.2 = u32::MAX;
                        if !jit.ic_done.contains(&pc) {
                            jit.ic_done.insert(pc);
                            jit.cache_remove(&pc);
                            jit.dispatch[sl].pc = NO_PC;
                            unsafe { IC_EXTENDS += 1 };
                            stop_after_dispatch = true; // recompile on the next tier-up pass
                        }
                    }
                }
            }
            // Sampled exit attribution: after a region function returns,
            // cpu.pc holds the pc it exited TO. Out-of-region targets are
            // the measured signal for incremental extension.
            if idx & SB_IDX_BIT != 0 {
                let tick = unsafe {
                    EXIT_TICK = EXIT_TICK.wrapping_add(1);
                    EXIT_TICK
                };
                if tick & ((1 << EXIT_SAMPLE_SHIFT) - 1) == 0 {
                    let stay = unsafe { RETIRED_CELL };
                    record_region_exit(jit, idx & !IDX_TAG_MASK, m.cpu().pc, stay);
                }
            }
            // Sys blocks with inline memory ops may bail mid-block; read the
            // count they actually retired (pc is set by the block either way).
            let retired = unsafe { RETIRED_CELL };
            let dprof_sample = unsafe {
                if DPROF_ON {
                    DPROF_TICK = DPROF_TICK.wrapping_add(1);
                    DPROF_TICK & ((1u64 << DPROF_SAMPLE_SHIFT) - 1) == 0
                } else {
                    false
                }
            };
            if dprof_sample {
                dprof_hit(pc, retired);
                eprof_hit(pc, m.cpu().pc, retired);
            }
            if unsafe { DPROF_ON } {
                unsafe {
                    if idx & SB_IDX_BIT != 0 {
                        DPROF_REGION_CALLS += 1;
                        DPROF_REGION_INSNS += retired;
                    } else {
                        DPROF_BLOCK_CALLS += 1;
                        DPROF_BLOCK_INSNS += retired;
                        if let Some(Some(b)) = jit.cache.get(&pc) {
                            if b.n != 0 {
                                let mut attributed = 0u64;
                                for i in 1..5 {
                                    let count = retired * b.mix[i] as u64 / b.n as u64;
                                    DPROF_TRACE_MIX[i] += count;
                                    attributed += count;
                                }
                                DPROF_TRACE_MIX[0] += retired.saturating_sub(attributed);
                                for i in 0..10 {
                                    DPROF_TRACE_MEM[i] += retired * b.mem[i] as u64 / b.n as u64;
                                }
                                for i in 0..3 {
                                    DPROF_TRACE_CONTROL[i] +=
                                        retired * b.control[i] as u64 / b.n as u64;
                                }
                                for i in 0..5 {
                                    DPROF_TRACE_ALU[i] += retired * b.alu[i] as u64 / b.n as u64;
                                }
                            }
                        }
                    }
                }
            }
            stop_after_dispatch = account_compiled_dispatch(
                &mut retired_sum,
                &mut chained,
                retired,
                stop_after_dispatch,
            );
            // A block that retired nothing bailed on its very first instruction
            // (TLB miss / MMIO / FP fast-path). It makes no progress, so stop
            // chaining and let the interpreter handle that instruction — never
            // spin re-calling it.
            if retired == 0 {
                unsafe { ZERO_RETIRE += 1 };
                stop_after_dispatch |= observe_tlb_bail(jit, m.cpu(), pc);
                if dprof_sample {
                    let fcsr = m.cpu().fcsr;
                    let fs = m.cpu().sys.as_ref().map_or(3, |c| (c.mstatus >> 13) & 3);
                    unsafe {
                        if fcsr & 1 == 0 {
                            ZR_NX += 1;
                        }
                        if (fcsr >> 5) & 7 != 0 {
                            ZR_FRM += 1;
                        }
                        if fs != 3 {
                            ZR_FS += 1;
                        }
                    }
                }
            }
            if stop_after_dispatch {
                break;
            }
        }
        let pending_host_io = m.pending_host_io();
        m.cpu_mut().insn_count += retired_sum;
        unsafe {
            JIT_RETIRED += retired_sum;
            JIT_DISPATCHES += chained as u64;
        }
        remaining = remaining.saturating_sub(retired_sum);
        if pending_host_io {
            break;
        }

        // If we stopped only because we hit the chain cap (the next pc is still
        // compiled and making progress), keep running in the JIT: advance the
        // clock and service interrupts here — the interrupt/timer work the
        // interpreter slice below used to do — instead of dropping to a wasteful
        // ~256-insn interp slice. This is the difference between ~50% and ~95%
        // JIT coverage on branchy, deeply-chained workloads (the CPython eval
        // loop). (`chained == CAP` can only be reached when every block in the
        // batch retired > 0, since a zero-retire block breaks above.)
        if remaining == 0 {
            break;
        }
        if chained == JIT_CHAIN_CAP || retired_sum >= round_budget {
            // Quantum boundary: re-anchor the wall clock BEFORE advancing
            // devices — a full quantum (1M insns) can pass in well under the
            // interpolation model's assumptions when bulk fast paths run,
            // and mtime must track real time, not extrapolated time.
            m.refresh_jit_time(true);
            FullSystemJitMachine::sync_jit_devices(m);
            m.check_interrupts();
            chain_ctl_boundary(m.cpu().insn_count);
            // Extension FIRST: a landed region whose measured exits keep
            // leaving it grows along that traffic. Extensions outrank fresh
            // page builds for the build budget — a fresh 3-page function
            // over call-heavy code exits immediately and holds nothing,
            // while an extension is provably where dispatches are lost
            // (drained behind the page queue, tcc got 3 extensions against
            // 179 page builds and kept its 8-insn dispatches).
            drain_ext_queue(m, jit);
            // Then spend what's left on the oldest deferred page that still
            // resolves in the CURRENT address space (issuing an extension
            // above moved SB_LAST_ICOUNT, so at most one build per boundary).
            if !jit.sb_queue.is_empty()
                && full_system_jit_issue_allowed()
                && sb_build_allowed(m.cpu().insn_count)
            {
                let aspace = m.cpu().sys.as_ref().map_or(0, |c| c.satp);
                if let Some(i) = jit.sb_queue.iter().position(|&(a, _)| a == aspace) {
                    let (_, vpage) = jit.sb_queue.remove(i);
                    let compiles = jit.sb_gen.get(&(aspace, vpage)).map_or(0, |&(_, c, _)| c);
                    if let Some(pa) = m.probe_fetch(vpage) {
                        if m.ram_range(pa & !0xfff, 0x1000).is_some() {
                            build_superblock(m, jit, aspace, vpage, pa & !0xfff, compiles);
                        }
                    }
                }
            }
            if unsafe { JIT_ISSUES_THIS_RUN != 0 } {
                break;
            }
            continue;
        }

        // Extension drain in USER context: the chain just broke while the
        // guest code that queued the work is the one running (the quantum
        // boundary above misses dispatch-heavy phases entirely).
        drain_ext_queue(m, jit);

        // --- hot counting + compile (from physical code bytes) ---
        let pc = m.cpu().pc;
        // Address space this discovery belongs to (satp; 0 in bare mode).
        let aspace = m.cpu().sys.as_ref().map_or(0, |c| c.satp);
        if unsafe { DPROF_ON } {
            if let Some(pa) = m.probe_fetch(pc) {
                if let Some(range) = m.ram_range(pa, 4) {
                    ihist_hit(u32::from_le_bytes(
                        m.ram()[range].try_into().expect("four-byte RAM range"),
                    ));
                }
            }
        }
        if jit_compilation_allowed()
            && full_system_jit_issue_allowed()
            && !jit.cache.contains_key(&pc)
            && !jit_work_contains_pc(jit, aspace, pc)
            && !jit.confirmed_coverage.contains_key(&(aspace, pc))
        {
            let hot = {
                let c = jit.hot.entry(pc).or_insert(0);
                *c += 1;
                *c
            };
            if hot >= unsafe { JIT_THRESHOLD } {
                if let Some(pa) = m.probe_fetch(pc) {
                    if let Some(range) = m.ram_range(pa, 1) {
                        let mut lay = jit_layout(m.cpu());
                        lay.dispatch_base = jit.dispatch.as_ptr() as u32;
                        lay.dispatch_mask = (DISPATCH_SIZE - 1) as u32;
                        lay.map_gen_addr = m.cpu().jit_map_gen_ptr() as u32;
                        let vpage = pc & !0xfff;
                        let pa_page = pa & !0xfff;
                        jit.record_page_hot_entry(aspace, pc);
                        let page_is_in_ram = m.ram_range(pa_page, 0x1000).is_some();
                        let off = range.start;
                        let end = ((off + 1024).min(off | 0xfff) + 1).min(m.ram().len());
                        // Superblock path (opt-in): loop headers stay individual
                        // (tight wasm loop); non-loop pages accumulate entries and
                        // upgrade to a page superblock once branchy enough.
                        let (is_loop, n_entries) = if unsafe { SYS_SUPERBLOCK } {
                            let il = rv64_jit::is_loop_at(&m.ram()[off..end], pc, pc, lay);
                            let ne = if il {
                                0
                            } else {
                                let e = jit.page_entries.entry((aspace, vpage)).or_default();
                                if let Err(i) = e.binary_search(&pc) {
                                    e.insert(i, pc);
                                }
                                e.len()
                            };
                            (il, ne)
                        } else {
                            (false, 0)
                        };

                        let (sb_last, sb_compiles, sb_when) = jit
                            .sb_gen
                            .get(&(aspace, vpage))
                            .copied()
                            .unwrap_or((0, 0, 0));
                        // Rebuilding a page function DISCARDS the optimized
                        // code V8 built for it: the replacement module starts
                        // in the baseline compiler again. Measured, a page that
                        // kept rebuilding every couple of seconds ran ~3x
                        // slower with identical coverage (nbench FP EMULATION:
                        // 444 insns/dispatch either way, 2568 -> 820 MIPS), and
                        // nbench itself flagged the result as statistically
                        // uncertain. So each rebuild costs the page an
                        // exponentially longer quiet period.
                        let cooldown = SB_PAGE_COOLDOWN << sb_compiles.min(6);
                        let sb_cool = m.cpu().insn_count >= sb_when.wrapping_add(cooldown);
                        // Recompile on DOUBLING, not on a fixed increment: a
                        // page discovered 6 hot pcs at a time would need 20
                        // recompiles to cover the 120 that nbench IDEA ends up
                        // with, so a fixed cap left most of the page on
                        // individual blocks forever. Doubling covers a page of
                        // any size in a handful of compiles and is
                        // self-amortizing — each one costs at most as much as
                        // all the previous ones together.
                        let sb_spaced = sb_build_allowed(m.cpu().insn_count);
                        let sb_want = if jit.superblocked.contains(&(aspace, vpage)) {
                            // Recompile when the page has grown by half again,
                            // OR as soon as the page function has visibly
                            // fallen behind: SB_MISSED_TRIGGER hot pcs on it
                            // needed their own blocks. Growth alone raced —
                            // a page that ends at 120 hot pcs after compiling
                            // at 96 never doubles again, and which of its
                            // functions the page function happened to cover
                            // decided whether nbench IDEA scored 1600 or 4400
                            // iter/s from identical runs.
                            // Rebuild when the page function has visibly
                            // fallen behind: enough hot pcs needed their own
                            // blocks, scaled to what it already covers. A flat
                            // trigger burned the whole recompile allowance
                            // during warmup (16 rebuilds while barely a handful
                            // of pcs were hot), after which the bulk of
                            // cipher_idea — which only gets hot later — could
                            // never be covered: nbench IDEA scored 1600 instead
                            // of 4400 iter/s depending on that race.
                            let missed = jit.sb_missed.get(&(aspace, vpage)).copied().unwrap_or(0);
                            // Rebuild when enough hot pcs have had to build
                            // their own blocks, scaled to what the page
                            // function already covers. The counter is reset by
                            // each build, so this converges: a page only keeps
                            // rebuilding while it keeps discovering hot code.
                            sb_cool
                                && missed >= SB_MISSED_TRIGGER.max(sb_last as u32 / 4)
                                && (n_entries > sb_last || sb_compiles < SB_RECOMPILE_CAP)
                        } else {
                            n_entries >= SUPERBLOCK_THRESHOLD
                        };
                        if !is_loop && sb_want && page_is_in_ram {
                            if sb_spaced {
                                build_superblock(m, jit, aspace, vpage, pa_page, sb_compiles);
                            } else {
                                // Budget says not yet: remember the page and
                                // build it at a later quantum boundary rather
                                // than dropping the request — a page whose hot
                                // pcs all appear inside one budget window would
                                // otherwise never be revisited (nbench IDEA
                                // fell back to 6.4 insns/dispatch).
                                if jit.sb_queue.len() < SB_QUEUE_CAP
                                    && !jit.sb_queue.contains(&(aspace, vpage))
                                {
                                    jit.sb_queue.push((aspace, vpage));
                                }
                            }
                        }
                        if pc == unsafe { TRACE_PC } {
                            unsafe { TRACE_INDIV += 1 };
                        }
                        // The rebuild-pressure count moves BELOW, after the
                        // block exists: only SHORT blocks count as misses.
                        // Long traces would not be claimed by a page function
                        // anyway (TRACE_KEEP_MIN), and in the trace world new
                        // hot pcs are minted continuously (side-exit
                        // targets), so counting every one drove PERPETUAL
                        // rebuilds that discarded V8-optimized functions —
                        // the measured 3x churn cliff, back from the dead.
                        let missed_here = !is_loop && jit.superblocked.contains(&(aspace, vpage));
                        // Individual block (loop or pre-threshold non-loop).
                        // Deliberately NOT deferred while a page function is on
                        // its way: making hot pcs wait for one stalls code
                        // behind an async compile that may never land — that
                        // was measured as 138M retries and a 10x slowdown once
                        // pending builds backed up.
                        // The measured default supplies the current 4KB page.
                        // TRACE_WINDOW_MODE can instead select the legacy
                        // aligned 64-page gather (see TraceWin), in which case
                        // a trace may follow calls within 256KB and registers
                        // every page its final span covers. Multi-page spans
                        // ride the regions pa-verify.
                        let single_page = match unsafe { TRACE_WINDOW_MODE } {
                            1 => true,
                            2 => false,
                            _ => rv64_jit::trace_level() == 0,
                        };
                        let first_va = if single_page {
                            vpage
                        } else {
                            vpage & !TRACE_WIN_MASK
                        };
                        let wins = unsafe { &mut TRACE_WIN };
                        // Unprocessed dirty pages force a re-gather: the
                        // buffer may predate the store (a fresh gather reads
                        // current RAM, so it is always safe to rebuild).
                        if m.code_has_dirty() {
                            wins.clear();
                        }
                        let mg = m.cpu().map_generation();
                        let bg = unsafe { BOOT_GEN };
                        let hit = wins.iter().position(|w| {
                            w.aspace == aspace
                                && w.map_gen == mg
                                && w.boot_gen == bg
                                && w.first_va == first_va
                        });
                        let npages = if single_page { 1 } else { TRACE_WIN_PAGES };
                        let wi = match hit {
                            Some(i) => i,
                            None => {
                                let mut w = TraceWin {
                                    aspace,
                                    map_gen: mg,
                                    boot_gen: bg,
                                    first_va,
                                    pages: Vec::new(),
                                    buf: vec![0u8; (npages * 0x1000) as usize],
                                };
                                for k in 0..npages {
                                    let va = first_va + k * 0x1000;
                                    if let Some(p) = m.probe_fetch(va) {
                                        let pp = p & !0xfff;
                                        if let Some(range) = m.ram_range(pp, 0x1000) {
                                            let bo = (k * 0x1000) as usize;
                                            w.buf[bo..bo + 0x1000].copy_from_slice(&m.ram()[range]);
                                            w.pages.push((va, pp));
                                        }
                                    }
                                }
                                if wins.len() >= TRACE_WIN_CACHE {
                                    wins.remove(0);
                                }
                                wins.push(w);
                                wins.len() - 1
                            }
                        };
                        let w = &wins[wi];
                        let winpages = &w.pages;
                        unsafe { COMPILES_TICK += 1 };
                        let page_offset = usize::try_from(vpage - w.first_va)
                            .expect("trace window contains the current page");
                        let page_code = &w.buf[page_offset..page_offset + 0x1000];
                        'compile_hot: {
                            let confirmed_on = unsafe { CONFIRMED_BATCH_ON };
                            if !confirmed_on && page_is_in_ram {
                                match issue_page_module(m, jit, pc, pa_page, page_code, lay) {
                                    PageModuleIssue::Individual | PageModuleIssue::Awaiting => {}
                                    // Async completion requires returning to the
                                    // host microtask queue.
                                    PageModuleIssue::Issued => {
                                        break 'run;
                                    }
                                }
                            }

                            let structured_loop =
                                confirmed_on && rv64_jit::is_loop_at(&w.buf, w.first_va, pc, lay);
                            if confirmed_on && !structured_loop {
                                let translated = {
                                    let cache = &jit.cache;
                                    let hot = |target: u64| matches!(cache.get(&target), Some(Some(block)) if block.idx >= 0);
                                    let succ = &jit.succ;
                                    let next = |entry: u64| {
                                        let successor = succ[JitState::dslot(entry)];
                                        (successor.0 == entry
                                            && successor.2 >= unsafe { IC_EXTEND_TRIGGER })
                                        .then_some(successor.1)
                                    };
                                    rv64_jit::translate_confirmed_body(
                                        &w.buf, w.first_va, pc, lay, &hot, &next,
                                    )
                                };
                                let staged = translated.and_then(|(body, member)| {
                                    staged_block(m, aspace, winpages, body, member, missed_here)
                                });
                                if let Some(staged) = staged {
                                    jit.add_confirmed_coverage(&staged.pending);
                                    jit.confirmed_stage.push(staged);
                                    if submit_confirmed_stage(jit) {
                                        break 'run;
                                    }
                                    // The staged pc remains interpreted until
                                    // eight independently hot traces share one
                                    // module compile.
                                    break 'compile_hot;
                                }
                            }
                            // BATCH: compile this pc together with its fixed-
                            // target successors as one module whose members
                            // tail-call each other directly (~2ns/hop, no table
                            // import, O(1) registration). Falls back to the
                            // single-block path whenever a batch can't form.
                            let batch_t0 = unsafe { host_now_ms() };
                            let cell = unsafe {
                                let c = BATCH_CELL_NEXT;
                                BATCH_CELL_NEXT = (c + 1) % BATCH_CELLS;
                                c
                            };
                            let batch = if !confirmed_on
                                && unsafe { BATCH_ON }
                                && jit.cache.len() < unsafe { BATCH_POP_CAP }
                                && !w.pages.is_empty()
                            {
                                let mut blay = lay;
                                blay.batch_base_addr = batch_cell_addr(cell);
                                let cache = &jit.cache;
                                let hotmap = &jit.hot;
                                let hot =
                                    |t: u64| matches!(cache.get(&t), Some(Some(b)) if b.idx >= 0);
                                let wlo = w.first_va;
                                let whi = w.first_va + (TRACE_WIN_PAGES * 0x1000);
                                let pages = &w.pages;
                                // Members must be PROVEN hot: taking every exit
                                // target compiled ~24 blocks per tier-up, most
                                // never executed — a compile storm that ran
                                // python fib 35x slower (173s). Warm pcs only
                                // (half the tier-up threshold) keeps a batch to
                                // the successor set actually being executed.
                                let bar = unsafe { JIT_THRESHOLD >> BATCH_BAR_SHIFT };
                                // Already-compiled successors DO join (the batch
                                // supersedes them in the cache; the old block just
                                // becomes unreachable): requiring uncompiled pcs
                                // meant batches almost never formed with 2+
                                // members, since a hot pc's successors are
                                // normally compiled before it. Loop headers are
                                // excluded — their tight wasm regions beat any
                                // trace — as are superblock entries (n == 0).
                                // BATCH_PAGE: co-locate the hot pcs of the seed's
                                // OWN page. Successor-seeded batches only reached
                                // ~12% in-batch exits; if the per-dispatch cost is
                                // dominated by V8 instance switches (each block
                                // module is its own instance), packing a page's
                                // blocks into one instance pays on EVERY dispatch
                                // between them, links or not.
                                let seedpage = pc & !0xfff;
                                let page_mode = unsafe { BATCH_PAGE };
                                let want = |t: u64| {
                                    t >= wlo
                                        && t < whi
                                        && pages.iter().any(|&(va, _)| va == t & !0xfff)
                                        && hotmap.get(&t).is_some_and(|&c| c >= bar)
                                        && !matches!(cache.get(&t), Some(Some(b)) if b.n == 0)
                                        && !jit_work_contains_pc(jit, aspace, t)
                                        && (!page_mode || t & !0xfff == seedpage)
                                };
                                let succ = &jit.succ;
                                // Observed successor of a pc, when we have one.
                                let next = |t: u64| {
                                    let e = succ[JitState::dslot(t)];
                                    (e.0 == t).then_some(e.1)
                                };
                                rv64_jit::translate_batch_obs(
                                    &w.buf,
                                    w.first_va,
                                    pc,
                                    blay,
                                    &hot,
                                    &want,
                                    &next,
                                    unsafe { BATCH_CAP },
                                )
                            } else {
                                None
                            };
                            if let Some((wasm, members)) = batch {
                                unsafe {
                                    SB_BUILD_MS += host_now_ms() - batch_t0;
                                    SB_LAST_ICOUNT = m.cpu().insn_count;
                                }
                                // RATE GOVERNOR. The gates that separate a
                                // workload batching PAYS for from one it does
                                // not are neither population nor footprint
                                // (both accumulate kernel/boot code and fire
                                // for everyone) — it is how FAST batches are
                                // demanded. nbench ASSIGNMENT wants a few dozen
                                // over tens of billions of instructions; CPython
                                // wants thousands inside its first second, and
                                // pays a batch compile per tier-up for code it
                                // never re-enters (python fib 3.7s -> 180s).
                                // Once the observed rate proves that shape,
                                // batching switches off for the rest of the run.
                                unsafe {
                                    // Deferring this verdict until the guest is
                                    // warm was tried and is WORSE: python's
                                    // storm resumes unchecked (all runs time
                                    // out) while ASSIGNMENT still gains nothing.
                                    // Judging from the first batch on is what
                                    // produced python's MATCH.
                                    let gi = (m.cpu().insn_count / 1_000_000_000).max(1);
                                    if BATCHES > 64 && BATCHES / gi > BATCH_RATE_CAP {
                                        BATCH_ON = false;
                                    }
                                }
                                if members.len() >= 2 && full_system_jit_issue_allowed() {
                                    let mut pending_members = Vec::with_capacity(members.len());
                                    let mut valid_batch = true;
                                    for mb in members {
                                        let (lo, hi) = if mb.span == (0, 0) {
                                            (mb.pc, mb.pc + 2)
                                        } else {
                                            mb.span
                                        };
                                        let mut mpa = 0u64;
                                        let mut spanned = Vec::new();
                                        let mut va = lo & !0xfff;
                                        while va <= (hi - 1) & !0xfff {
                                            let Some(&(_, pp)) =
                                                w.pages.iter().find(|&&(v, _)| v == va)
                                            else {
                                                valid_batch = false;
                                                break;
                                            };
                                            if va == mb.pc & !0xfff {
                                                mpa = pp + (mb.pc & 0xfff);
                                            }
                                            spanned.push((va, pp));
                                            va += 0x1000;
                                        }
                                        if !valid_batch || mpa == 0 {
                                            valid_batch = false;
                                            break;
                                        }
                                        let block = JitBlock {
                                            fp: mb.uses_fp,
                                            idx: -1,
                                            n: mb.n_insns,
                                            mix: mb.trace_mix,
                                            mem: mb.trace_mem,
                                            control: mb.trace_control,
                                            alu: mb.trace_alu,
                                            pa: mpa,
                                            last_used: 0,
                                        };
                                        let Some(block) = pending_block(
                                            m, aspace, mb.pc, block, spanned, mb.seeds, false,
                                        ) else {
                                            valid_batch = false;
                                            break;
                                        };
                                        pending_members.push(block);
                                    }
                                    if valid_batch && pending_members.len() >= 2 {
                                        for member in &pending_members {
                                            for &(_, pp) in &member.pages {
                                                m.code_mark_page(pp);
                                            }
                                        }
                                        let sequence = unsafe {
                                            let sequence = NEXT_BATCH_SEQUENCE;
                                            NEXT_BATCH_SEQUENCE =
                                                NEXT_BATCH_SEQUENCE.wrapping_add(1);
                                            BATCH_CELL_SEQUENCE[cell] = sequence;
                                            sequence
                                        };
                                        unsafe { JIT_OUT = wasm };
                                        if submit_pending_jit(PendingJitKind::Batch(PendingBatch {
                                            cell,
                                            sequence,
                                            members: pending_members,
                                            page_module: None,
                                        }))
                                        .is_some()
                                        {
                                            break 'run;
                                        }
                                    }
                                }
                            }
                            let blk = {
                                // Hotness oracle for branch-direction bias: a
                                // compiled (non-blacklisted) target is proven-hot.
                                let cache = &jit.cache;
                                let hot =
                                    |t: u64| matches!(cache.get(&t), Some(Some(b)) if b.idx >= 0);
                                // Inline-cache oracle: the target this pc's
                                // indirect jump was last observed to take.
                                let succ = &jit.succ;
                                let next = |t: u64| {
                                    let e = succ[JitState::dslot(t)];
                                    (e.0 == t && e.2 >= unsafe { IC_EXTEND_TRIGGER }).then_some(e.1)
                                };
                                rv64_jit::translate_block_ic(
                                    &w.buf,
                                    w.first_va,
                                    pc,
                                    lay,
                                    &hot,
                                    &|_| None,
                                    &next,
                                )
                            };
                            let entry = blk.and_then(|blk| {
                                // Pages the emitted code actually came from
                                // ((0,0) span = wholly within [pc, pc+len)).
                                let (lo, hi) = if blk.span == (0, 0) {
                                    (pc, pc + blk.len.max(2))
                                } else {
                                    blk.span
                                };
                                let mut spanned: Vec<(u64, u64)> = Vec::new();
                                let mut va = lo & !0xfff;
                                while va <= (hi - 1) & !0xfff {
                                    let Some(&(_, pp)) = winpages.iter().find(|&&(v, _)| v == va)
                                    else {
                                        return None; // span escaped the window (impossible)
                                    };
                                    spanned.push((va, pp));
                                    va += 0x1000;
                                }
                                for &(_, pp) in &spanned {
                                    m.code_mark_page(pp);
                                }
                                let block = JitBlock {
                                    fp: blk.uses_fp,
                                    idx: -1,
                                    n: blk.n_insns,
                                    mix: blk.trace_mix,
                                    mem: blk.trace_mem,
                                    control: blk.trace_control,
                                    alu: blk.trace_alu,
                                    pa,
                                    last_used: 0,
                                };
                                let pending = pending_block(
                                    m,
                                    aspace,
                                    pc,
                                    block,
                                    spanned,
                                    blk.seeds,
                                    missed_here && block.n < unsafe { TRACE_KEEP_MIN },
                                )?;
                                unsafe { JIT_OUT = blk.wasm };
                                submit_pending_jit(PendingJitKind::Block(pending))?;
                                Some(())
                            });
                            match entry {
                                Some(()) => break 'run,
                                // Untranslatable at THESE code bytes: blacklist with
                                // a pa-stamped sentinel (idx = -1). It's re-verified
                                // like a real block (map_gen / dispatch probe) so it
                                // survives context switches without poisoning a
                                // different address space at the same va, and the
                                // dirty-page tracker naturally drops it if the code
                                // bytes are overwritten.
                                None => {
                                    // Pending capacity is temporary. Do not poison a
                                    // valid pc merely because the async queue is full.
                                    if !full_system_jit_issue_allowed() {
                                        continue 'run;
                                    }
                                    if missed_here {
                                        *jit.sb_missed.entry((aspace, vpage)).or_insert(0) += 1;
                                        unsafe { SB_INDIV += 1 };
                                    }
                                    m.code_mark_page(pa);
                                    let jb = JitBlock {
                                        fp: false,
                                        idx: -1,
                                        n: 0,
                                        mix: [0; 5],
                                        mem: [0; 10],
                                        control: [0; 3],
                                        alu: [0; 5],
                                        pa,
                                        last_used: 0,
                                    };
                                    jit.cache_insert(pc, Some(jb));
                                }
                            }
                        }
                    }
                }
            }
        }

        // --- decoded interpreter + devices ---
        // One code-page cache serves both cold interpretation and JIT fallback.
        // It predecodes instructions, runs to a basic-block boundary, and overlays
        // compiled entries during the block scan. No path re-enters Cpu::run(1)
        // merely to ask whether the next PC is compiled.
        let icount_before = m.cpu().insn_count;
        let tier_up = unsafe { JIT_THRESHOLD != u32::MAX }
            && jit_compilation_allowed()
            && full_system_jit_issue_allowed();
        let outcome = m.run_interpreter_cached(remaining.min(4096), jit, tier_up);
        unsafe {
            SLICE_CALLS += 1;
            SLICE_INSNS += outcome.retired;
            if DPROF_ON && IHIST_LAST != usize::MAX {
                // Charge the whole interpreted stretch to the instruction where
                // compiled execution yielded to the shared page cache.
                IHIST_INSNS[IHIST_LAST] += m.cpu().insn_count - icount_before;
                IHIST_LAST = usize::MAX;
            }
        }
        remaining = remaining.saturating_sub(outcome.retired.max(1));
        if outcome.idle || m.pending_host_io() {
            break;
        }

        // Stream console output at quantum granularity, DURING execution —
        // buffering until virt_run returns skews benchmark timing: a marker
        // printed early in a slice would be timestamped after the whole slice.
        m.flush_host_io();
        if unsafe { JIT_ISSUES_THIS_RUN != 0 } || take_host_event() {
            break;
        }
    }

    m.flush_host_io();
    m.powered_off() as i32
}

/// Nested chain-transfer depth (see chain_next) and its bound: each hop
/// holds two wasm frames (the calling block + chain_next) until the chain
/// unwinds, so the cap bounds stack use; a chain that reaches it simply
/// returns to the host loop, which re-dispatches seamlessly.
static mut CHAIN_DEPTH: u32 = 0;
const CHAIN_DEPTH_CAP: u32 = 64;
static mut CHAIN_HOPS: u64 = 0;

/// Block-to-block transfer WITHOUT the shared-table import: generated trace
/// blocks call this main-module export as a FUNCTION import (env.chain_next
/// — the same wasm-to-wasm shape as env.tlb_fill, which thousands of block
/// modules already import with no penalty). Importing the function table
/// instead made every table.set O(importing instances) on this V8 —
/// quadratic registration across tcc's 7.5k blocks — which is why emitted
/// return_call_indirect chaining is off. Here the dispatch-line fast path
/// (pc match under the current map generation, blacklist, fuel) runs in
/// ONE place in Rust and the transfer is a plain indirect call through the
/// table the main module owns.
#[no_mangle]
pub extern "C" fn chain_next(context: i32) {
    unsafe {
        if context != FULL_SYSTEM_CONTEXT_HANDLE
            || !FULL_SYSTEM_DISPATCH_ACTIVE
            || CHAIN_DEPTH >= CHAIN_DEPTH_CAP
        {
            return;
        }
        let Some(mut context) = active_jit_context() else {
            return;
        };
        if context.pending_host_io() {
            return;
        }
        let next_idx = {
            let (cpu, jit) = context.dispatch_parts();
            // Fuel: the cumulative retired cell against this dispatch's grant.
            if RETIRED_CELL >= FUEL_CELL {
                return;
            }
            let pc = cpu.pc;
            let line = jit.dispatch[JitState::dslot(pc)];
            if line.pc != pc || line.gen != cpu.map_generation() || line.idx < 0 {
                return; // miss/blacklist/stale: the host loop owns the slow path
            }
            line.idx & !IDX_TAG_MASK
        };
        CHAIN_DEPTH += 1;
        CHAIN_HOPS += 1;
        let f: extern "C" fn(i32) = core::mem::transmute(next_idx as usize);
        f(FULL_SYSTEM_CONTEXT_HANDLE);
        CHAIN_DEPTH -= 1;
    }
}

/// Region-function modules issued but not yet landed. The host's run loop
/// should yield to its event loop when this is nonzero: module compilation
/// resolves on the microtask queue. A loop that never yields leaves finished
/// code waiting tens of millions of instructions, so the host must return at
/// each execution slice.
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn sys_pending_builds() -> u32 {
    unsafe { PENDING_JIT.len() as u32 }
}

/// Async JIT completion (called by JS between `virt_run` calls, never
/// during wasm execution). Validates that the machine, the code page, and
/// the va→pa mapping are still the ones the compile was issued against
/// before repointing the page's entries at the new function.
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn sys_jit_ready(ticket: u64, base: i32, slot_count: u32) {
    unsafe {
        if let Some(machine) = VIRT.as_mut() {
            complete_jit(machine, ticket, base, slot_count);
            return;
        }
        for offset in 0..slot_count {
            let idx = base.checked_add(offset as i32).unwrap_or(-1);
            if idx >= 0 {
                register_table_slot(idx);
                retire_table_slot(idx, false);
            }
        }
    }
}

/// Compatibility entry for pre-async-region test modules. New generated
/// modules call sys_jit_ready with the complete contiguous slot run.
#[no_mangle]
pub extern "C" fn sys_sb_ready(ticket: u64, idx: i32) {
    sys_jit_ready(ticket, idx, 1);
}

#[allow(static_mut_refs)]
fn complete_jit<M: FullSystemJitMachine>(m: &mut M, ticket: u64, base: i32, slot_count: u32) {
    unsafe {
        let Some(pos) = PENDING_JIT.iter().position(|p| p.ticket == ticket) else {
            // JavaScript can finish a compile after a reboot cleared the
            // ticket. Adopt and retire the returned slot so it cannot leak.
            for offset in 0..slot_count {
                let idx = base.checked_add(offset as i32).unwrap_or(-1);
                if idx >= 0 {
                    register_table_slot(idx);
                    retire_table_slot(idx, false);
                }
            }
            return;
        };
        let p = PENDING_JIT.swap_remove(pos);
        if let Some(jit) = SYS_JIT.as_mut() {
            jit.remove_pending_coverage(&p.kind);
        }
        if p.slot_count() != slot_count {
            if let (Some(jit), PendingJitKind::Batch(batch)) = (SYS_JIT.as_mut(), &p.kind) {
                let _ = jit.finish_pending_page_module(p.ticket, batch, PageModuleFinish::Failed);
            }
            for offset in 0..slot_count {
                let idx = base.checked_add(offset as i32).unwrap_or(-1);
                if idx >= 0 {
                    register_table_slot(idx);
                    retire_table_slot(idx, false);
                }
            }
            return;
        }
        let Some(jit) = SYS_JIT.as_mut() else {
            for offset in 0..slot_count {
                let idx = base.checked_add(offset as i32).unwrap_or(-1);
                if idx >= 0 {
                    register_table_slot(idx);
                    retire_table_slot(idx, false);
                }
            }
            return;
        };
        let block_stale = |block: &PendingBlock| {
            block
                .pages
                .iter()
                .zip(&block.page_generations)
                .any(|(&(_, pp), &generation)| {
                    let page = (pp - rv64_system::RAM_BASE) >> 12;
                    !m.code_page_marked(page)
                        || m.code_page_dirty(page)
                        || m.code_page_generation(page) != Some(generation)
                })
        };
        let source_stale = p.boot_gen == BOOT_GEN
            && match &p.kind {
                PendingJitKind::Block(block) => block_stale(block),
                PendingJitKind::Batch(batch) => batch.members.iter().any(block_stale),
                PendingJitKind::Region(region) => {
                    region.pages.iter().zip(&region.page_generations).any(
                        |(&(_, pp), &generation)| {
                            let page = (pp - rv64_system::RAM_BASE) >> 12;
                            !m.code_page_marked(page)
                                || m.code_page_dirty(page)
                                || m.code_page_generation(page) != Some(generation)
                        },
                    )
                }
            };
        let current = p.boot_gen == BOOT_GEN
            && match &p.kind {
                PendingJitKind::Region(region) => {
                    jit.pending_superblock_is_current(p.ticket, region.aspace, &region.pages)
                }
                PendingJitKind::Batch(batch) => jit.page_module_pending_is_current(p.ticket, batch),
                PendingJitKind::Block(_) => true,
            };
        if !current {
            // A newer overlapping build owns at least one page. The older
            // module cannot install a coherent region, but it still releases
            // any non-overlapping claims that were not superseded.
            if let PendingJitKind::Region(region) = &p.kind {
                jit.finish_pending_superblock(p.ticket, region.aspace, &region.pages, false);
            }
            if let PendingJitKind::Batch(batch) = &p.kind {
                let _ =
                    jit.finish_pending_page_module(p.ticket, batch, PageModuleFinish::Cancelled);
            }
            if base >= 0 && source_stale {
                SB_STALE += 1;
            }
            for offset in 0..slot_count {
                let idx = base.checked_add(offset as i32).unwrap_or(-1);
                if idx >= 0 {
                    register_table_slot(idx);
                    retire_table_slot(idx, false);
                }
            }
            return;
        }
        if base == JIT_REGISTER_CAPACITY {
            if let PendingJitKind::Region(region) = &p.kind {
                jit.finish_pending_superblock(p.ticket, region.aspace, &region.pages, false);
            }
            if let PendingJitKind::Batch(batch) = &p.kind {
                let _ =
                    jit.finish_pending_page_module(p.ticket, batch, PageModuleFinish::Cancelled);
            }
            handle_jit_capacity(jit);
            return;
        }
        if base < 0 {
            if let PendingJitKind::Region(region) = &p.kind {
                jit.finish_pending_superblock(p.ticket, region.aspace, &region.pages, false);
            }
            if let PendingJitKind::Batch(batch) = &p.kind {
                let _ = jit.finish_pending_page_module(p.ticket, batch, PageModuleFinish::Failed);
            }
            return;
        }
        let slots = (0..slot_count).map(|offset| base + offset as i32);
        let owner = jit
            .track_owner(p.ticket, slots.clone())
            .expect("valid async JIT slot");
        // A page written while compiling makes the result stale. The write
        // generation closes the dirty-drain/re-mark ABA window: a page cannot
        // become apparently clean and accept an older compile.
        //
        // The va→pa mappings are deliberately NOT re-probed here: this callback
        // fires on the microtask queue at an arbitrary guest moment — usually
        // inside the kernel or another process, where a fetch probe of a user
        // va fails on privilege or resolves in the wrong address space. That
        // made 96% of finished superblocks drop on the floor (measured:
        // landed=4 of 127 on nbench). Instead the entries go into the
        // authoritative cache carrying their recorded pa, with NO dispatch
        // line: the first dispatch of each entry takes the cache path, which
        // probes the fetch mapping (and, for a multi-page region, every other
        // page too) before it runs or caches the line — the same verification
        // every block gets after a mapping event, deferred to a point where the
        // current address space is the one asking for the block.
        if source_stale {
            if let PendingJitKind::Region(region) = &p.kind {
                SB_STALE += 1;
                jit.finish_pending_superblock(p.ticket, region.aspace, &region.pages, false);
            }
            if let PendingJitKind::Batch(batch) = &p.kind {
                let _ =
                    jit.finish_pending_page_module(p.ticket, batch, PageModuleFinish::Cancelled);
            }
            jit.retire_unreferenced_slots(owner);
            return;
        }
        if let PendingJitKind::Region(region) = &p.kind {
            jit.finish_pending_superblock(p.ticket, region.aspace, &region.pages, true);
            SB_LANDED += 1;
            complete_region_landing(m, jit, base, region);
        } else if let PendingJitKind::Block(block) = &p.kind {
            complete_block_landing(m, jit, base, block);
        } else if let PendingJitKind::Batch(batch) = &p.kind {
            let prior_owner =
                jit.finish_pending_page_module(p.ticket, batch, PageModuleFinish::Landed(owner));
            complete_batch_landing(m, jit, base, batch);
            if let Some(prior_owner) = prior_owner.filter(|&prior| prior != owner) {
                jit.retire_owner(prior_owner, false);
            }
        }
        jit.retire_unreferenced_slots(owner);
    }
}

#[allow(static_mut_refs)]
fn complete_region_landing<M: FullSystemJitMachine>(
    _m: &mut M,
    jit: &mut JitState,
    idx: i32,
    p: &PendingRegion,
) {
    unsafe {
        if p.pages.len() > 1 {
            jit.regions.insert(idx, p.pages.clone());
        }
        // Start the exit profile that drives measured extension/demotion.
        jit.region_exits.insert(
            idx,
            RegionExits {
                aspace: p.aspace,
                lead: p.lead,
                pages: p.pages.clone(),
                total: 0,
                targets: Vec::new(),
                samples: 0,
                stay_sum: 0,
                entries: p.entries.clone(),
            },
        );
        for &e in &p.entries {
            if jit.page_module_owns_cached_entry(p.aspace, e) {
                continue;
            }
            // Sparse regions: find the entry's page by lookup (pages are in
            // dispatch order, not address order).
            let Some(pi) = p.pages.iter().position(|&(va, _)| va == e & !0xfff) else {
                continue;
            };
            // A long trace block keeps its pc: it already amortizes its
            // dispatch, and the function entry would trade that for a
            // register-union load per visit (see TRACE_KEEP_MIN).
            if matches!(jit.cache.get(&e), Some(Some(b))
                if b.n != 0 && !b.fp && b.n >= TRACE_KEEP_MIN)
            {
                continue;
            }
            let epa = p.pages[pi].1 + (e & 0xfff);
            let jb = JitBlock {
                fp: false,
                idx,
                n: 0,
                mix: [0; 5],
                mem: [0; 10],
                control: [0; 3],
                alu: [0; 5],
                pa: epa,
                last_used: next_jit_use_stamp(),
            };
            let prev = jit.cache_insert(e, Some(jb));
            SB_ENTRIES_IN += 1;
            if e == TRACE_PC {
                TRACE_SB_INSTALL += 1;
            }
            if matches!(prev, Some(Some(b)) if b.n != 0) {
                SB_REPLACED += 1;
            }
            // Invalidate any line still pointing at the old individual block.
            let slot = JitState::dslot(e);
            if jit.dispatch[slot].pc == e {
                jit.dispatch[slot].pc = NO_PC;
            }
        }
    }
}

fn block_should_replace_region(
    current: Option<&Option<JitBlock>>,
    block: JitBlock,
    keep: u32,
) -> bool {
    !matches!(current, Some(Some(current)) if current.n == 0) || (!block.fp && block.n >= keep)
}

#[allow(static_mut_refs)]
fn complete_block_landing<M: FullSystemJitMachine>(
    m: &mut M,
    jit: &mut JitState,
    idx: i32,
    p: &PendingBlock,
) {
    if jit.page_module_owns_cached_entry(p.aspace, p.pc) {
        return;
    }
    if !block_should_replace_region(jit.cache.get(&p.pc), p.block, unsafe { TRACE_KEEP_MIN }) {
        return;
    }
    let mut block = p.block;
    block.idx = idx;
    block.last_used = next_jit_use_stamp();
    for &(_, pp) in &p.pages {
        m.code_mark_page(pp);
    }
    if p.pages.len() > 1 {
        jit.regions.insert(idx, p.pages.clone());
    }
    if unsafe { SYS_SUPERBLOCK } {
        for &sd in &p.seeds {
            let e = jit.page_entries.entry((p.aspace, sd & !0xfff)).or_default();
            if let Err(i) = e.binary_search(&sd) {
                e.insert(i, sd);
            }
        }
    }
    let previous = jit.cache_insert(p.pc, Some(block));
    let slot = JitState::dslot(p.pc);
    if jit.dispatch[slot].pc == p.pc {
        jit.dispatch[slot].pc = NO_PC;
    }
    if p.missed_superblock && !matches!(previous, Some(Some(current)) if current.n == 0) {
        *jit.sb_missed.entry((p.aspace, p.pc & !0xfff)).or_insert(0) += 1;
        unsafe { SB_INDIV += 1 };
    }
}

#[allow(static_mut_refs)]
fn complete_batch_landing<M: FullSystemJitMachine>(
    _m: &mut M,
    jit: &mut JitState,
    base: i32,
    p: &PendingBatch,
) {
    let page_module = p.page_module.is_some();
    unsafe {
        if !page_module && BATCH_CELL_SEQUENCE[p.cell] == p.sequence {
            BATCH_BASE_POOL[p.cell] = base as u32;
        }
        BATCHES += 1;
        BATCH_MEMBERS += p.members.len() as u64;
        if p.page_module.is_some() {
            PAGE_MODULES_LANDED += 1;
            PAGE_MODULE_MEMBERS += p.members.len() as u64;
        }
    }
    for (offset, member) in p.members.iter().enumerate() {
        if !page_module && jit.page_module_owns_cached_entry(member.aspace, member.pc) {
            continue;
        }
        if !page_module
            && !block_should_replace_region(jit.cache.get(&member.pc), member.block, unsafe {
                TRACE_KEEP_MIN
            })
        {
            continue;
        }
        let mut block = member.block;
        block.idx = if page_module {
            base
        } else {
            base + offset as i32
        };
        block.last_used = next_jit_use_stamp();
        if member.pages.len() > 1 {
            jit.regions.insert(block.idx, member.pages.clone());
        }
        if unsafe { SYS_SUPERBLOCK } {
            for &sd in &member.seeds {
                let e = jit
                    .page_entries
                    .entry((member.aspace, sd & !0xfff))
                    .or_default();
                if let Err(i) = e.binary_search(&sd) {
                    e.insert(i, sd);
                }
            }
        }
        jit.cache_insert(member.pc, Some(block));
        let slot = JitState::dslot(member.pc);
        if jit.dispatch[slot].pc == member.pc {
            jit.dispatch[slot].pc = NO_PC;
        }
    }
}

/// Diagnostic: re-run the superblock leader analysis for a page in the CURRENT
/// address space. which: 0 = leaders discovered, 1 = leaders dropped as loop
/// headers, 2 = hot pcs recorded, 3 = hot pcs that survive as leaders,
/// 4 = hot pcs dropped as loop headers. u64::MAX = page not resolvable now.
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn sb_analyze(vpage: u64, which: u32) -> u64 {
    unsafe {
        let Some(jit) = SYS_JIT.as_ref() else {
            return u64::MAX;
        };
        VIRT.as_mut().map_or(u64::MAX, |machine| {
            analyze_superblock(machine, jit, vpage, which)
        })
    }
}

fn analyze_superblock<M: FullSystemJitMachine>(
    m: &mut M,
    jit: &JitState,
    vpage: u64,
    which: u32,
) -> u64 {
    let Some(pa) = m.probe_fetch(vpage) else {
        return u64::MAX;
    };
    let Some(range) = m.ram_range(pa & !0xfff, 0x1000) else {
        return u64::MAX;
    };
    let code = &m.ram()[range];
    let lay = jit_layout(m.cpu());
    let empty = Vec::new();
    let aspace = m.cpu().sys.as_ref().map_or(0, |c| c.satp);
    let seeds = jit.page_entries.get(&(aspace, vpage)).unwrap_or(&empty);
    let leaders = rv64_jit::discover_page_leaders(code, vpage, vpage, 0x1000, seeds, 512);
    let is_loop = |e: u64| rv64_jit::is_loop_at(code, vpage, e, lay);
    if which >= 5 {
        let keep: Vec<u64> = leaders.iter().copied().filter(|&e| !is_loop(e)).collect();
        let (rm, wm, fr, fw) =
            rv64_jit::scan_regs_super_pub(code, vpage, vpage + 0x1000, &keep, &lay);
        return match which {
            5 => ((rm | wm) & !1).count_ones() as u64,
            _ => (fr | fw).count_ones() as u64,
        };
    }
    match which {
        0 => leaders.len() as u64,
        1 => leaders.iter().filter(|&&e| is_loop(e)).count() as u64,
        2 => seeds.len() as u64,
        3 => seeds
            .iter()
            .filter(|&&e| leaders.contains(&e) && !is_loop(e))
            .count() as u64,
        _ => seeds.iter().filter(|&&e| is_loop(e)).count() as u64,
    }
}

/// Diagnostic for ONE pc: 0 = is_loop_at (excluded from superblocks),
/// 1 = instructions an individual block covers, 2 = cached (1) / blacklisted
/// (2) / absent (0).
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn sb_analyze_pc(pc: u64, which: u32) -> u64 {
    unsafe {
        let Some(jit) = SYS_JIT.as_ref() else {
            return u64::MAX;
        };
        VIRT.as_mut().map_or(u64::MAX, |machine| {
            analyze_superblock_pc(machine, jit, pc, which)
        })
    }
}

fn analyze_superblock_pc<M: FullSystemJitMachine>(
    m: &mut M,
    jit: &JitState,
    pc: u64,
    which: u32,
) -> u64 {
    if which == 2 {
        return match jit.cache.get(&pc) {
            Some(Some(b)) => {
                if b.n == 0 {
                    3 // superblock entry
                } else {
                    1
                }
            }
            Some(None) => 2,
            None => 0,
        };
    }
    let Some(pa) = m.probe_fetch(pc) else {
        return u64::MAX;
    };
    let Some(range) = m.ram_range(pa, 4) else {
        return u64::MAX;
    };
    let off = range.start;
    let end = ((off + 1024).min(off | 0xfff) + 1).min(m.ram().len());
    let lay = jit_layout(m.cpu());
    let code = &m.ram()[off..end];
    match which {
        0 => rv64_jit::is_loop_at(code, pc, pc, lay) as u64,
        3 => u32::from_le_bytes([code[0], code[1], code[2], code[3]]) as u64,
        _ => rv64_jit::translate_block(code, pc, pc, lay).map_or(0, |b| b.n_insns as u64),
    }
}

/// Diagnostic: superblock state of a code page — bit0 superblocked,
/// bit1 pending-async, bits 8.. = discovered hot entry count.
#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn sb_debug(vpage: u64) -> u64 {
    unsafe {
        let Some(jit) = SYS_JIT.as_ref() else {
            return 0;
        };
        let aspace = VIRT
            .as_ref()
            .map(|machine| machine.cpu.sys.as_ref().map_or(0, |cpu| cpu.satp));
        let Some(aspace) = aspace else { return 0 };
        let mut v = 0u64;
        if jit.superblocked.contains(&(aspace, vpage)) {
            v |= 1;
        }
        if jit.pending_superblocks.contains_key(&(aspace, vpage)) {
            v |= 2;
        }
        v |= (jit
            .page_entries
            .get(&(aspace, vpage))
            .map_or(0, |e| e.len()) as u64)
            << 8;
        // bits 24..31 = superblock compiles, 32..39 = uncovered hot pcs since
        v |= (jit.sb_gen.get(&(aspace, vpage)).map_or(0, |&(_, c, _)| c) as u64 & 0xff) << 24;
        v |= (jit.sb_missed.get(&(aspace, vpage)).copied().unwrap_or(0) as u64 & 0xff) << 32;
        v
    }
}

/// Trace one guest pc through the compile pipeline: jit_stat 27 = times it was
/// installed as a superblock entry, 28 = individual blocks built for it,
/// 29 = times it was a superblock seed, 30 = times it survived leader retain.
#[no_mangle]
pub extern "C" fn sb_trace_pc(pc: u64) {
    unsafe { TRACE_PC = pc };
}

/// Enable the hardware fused-madd FMADD path (host proves fusedness first).
#[no_mangle]
pub extern "C" fn jit_set_hw_fma(on: u32) {
    rv64_jit::set_hw_fma(on != 0);
}

/// Enable direct block-to-block tail-call chaining (host feature-detects
/// wasm tail-call support first). Loop/region chain sites follow this
/// directly (their successor sets are cyclic and stay monomorphic); trace
/// exits additionally require a small block population — flipped per
/// compile from the live cache size (see CHAIN_POP_CAP).
#[no_mangle]
pub extern "C" fn jit_set_tailcall(on: u32) {
    rv64_jit::set_chain(on != 0);
}
/// A/B: trace definedness tracking (rv64_jit::set_defined_track).
#[no_mangle]
pub extern "C" fn jit_set_defined(on: u32) {
    rv64_jit::set_defined_track(on != 0);
}
/// A/B: rotated-nest loop regions (see rv64_jit::set_rotated_nests).
#[no_mangle]
pub extern "C" fn jit_set_rotated_nests(on: u32) {
    rv64_jit::set_rotated_nests(on != 0);
}
/// Base table index of the most recently REGISTERED batch (see
/// JitLayout::batch_base_addr): the emitted intra-batch link checks
/// `line.idx == base + j` against this cell, which the host writes right
/// after the batch's functions land in the table.
/// Per-batch base cells. A single global cell was wrong: every batch's
/// emitted links read the same address, so as soon as a second batch
/// registered, the first batch's freshness checks compared against the
/// wrong base — silently defeating the check they exist to perform. Each
/// batch gets its own slot (address baked into its emitted code), rotating
/// through a fixed pool so addresses are stable for the module's lifetime.
const BATCH_CELLS: usize = 4096;
static mut BATCH_BASE_POOL: [u32; BATCH_CELLS] = [0; BATCH_CELLS];
static mut BATCH_CELL_SEQUENCE: [u64; BATCH_CELLS] = [0; BATCH_CELLS];
static mut NEXT_BATCH_SEQUENCE: u64 = 1;
static mut BATCH_CELL_NEXT: usize = 0;
/// Batch compilation (see rv64_jit::translate_batch). Members transfer by
/// DIRECT tail call inside one module — the only chaining shape that avoids
/// both historical blockers (no table import, so registration stays O(1);
/// no host round-trip per hop). It WORKS: 766 batches / 2965 members on an
/// in-guest `tcc -c`, host dispatches down 12%.
///
/// DEFAULT OFF anyway, on an interleaved 3-round A/B (the only method that
/// survives this host's boot lottery): compile 4356/4075/4069 without vs
/// 4558/4169/4336 with — batching loses every round. Reason, measured: only
/// ~12% of exits stay inside a batch, so the other ~88% pay the link's
/// guard (dispatch-line pc + map-generation + fuel) for nothing, and that
/// costs slightly more than the saved host round-trips return. Raising the
/// cap makes it worse (bigger modules, no better hit rate: 32 -> 4195,
/// 64 -> 4546). What would flip it: hit rate, not hop cost — batches formed
/// from OBSERVED dispatch sequences (trace trees) rather than static exit
/// seeds. Machinery is fully tested and one flag away.
/// Batching is ON, but only while the code cache is SMALL. Measured:
/// batching lifts nbench ASSIGNMENT from 8.37 to 9.14-10.13 iter/s
/// (uncontended), and destroys CPython — python fib went from 3.7s to 180s,
/// because a workload with a five-figure block population pays a batch
/// compile per tier-up for code it never re-enters. The separating property
/// is the population itself, not the workload: below the cap a batch's
/// members are a large fraction of all hot code, above it they are noise.
/// DEFAULT OFF on the evidence: batching's only beneficiary is nbench
/// ASSIGNMENT (+9..21%), which remains a LOSS either way (best draw 0.944x),
/// while it costs NUMERIC SORT ~25% (285.6 vs 358.5 iter/s under identical
/// contention) — enough to drop that row from MATCH to LOSS on the
/// authoritative scorecard. A mechanism that cannot flip the row it helps
/// and does flip a row it hurts is not worth shipping on. The machinery,
/// the IC composition and the rate governor stay behind jit_set_batch(1).
static mut BATCH_ON: bool = false;
/// Coalesce only entries that independently crossed the tier-up threshold.
/// Unlike speculative batches and page modules, this changes module packaging
/// without predicting successors or compiling cold page leaders.
static mut CONFIRMED_BATCH_ON: bool = false;
static mut CONFIRMED_BATCH_TARGET: usize = 8;

#[no_mangle]
pub extern "C" fn jit_set_confirmed_batch(on: u32) {
    unsafe { CONFIRMED_BATCH_ON = on != 0 }
}

#[no_mangle]
pub extern "C" fn jit_set_confirmed_batch_target(target: u32) {
    // A one-function batch exports `r0`, while the host's one-slot ABI expects
    // `run`. Keep confirmed batches genuinely multi-function.
    unsafe { CONFIRMED_BATCH_TARGET = target.clamp(2, 64) as usize }
}
/// Pack a hot code page into one multi-function Wasm module. Each discovered
/// leader keeps its own small register set; fixed edges use direct tail calls
/// and dynamic page-local edges pass through an internal router. The host
/// enables this only after validating Wasm tail-call support.
static mut PAGE_MODULES_ON: bool = false;
// Two independently hot roots amortize the page module's compilation cost.
const PAGE_MODULE_THRESHOLD: usize = 2;
const PAGE_MODULE_WARM_THRESHOLD: usize = 4;
// Dense modules cost more to compile than they save in host registrations.
const PAGE_MODULE_MAX_LEADERS: usize = 64;
static mut PAGE_MODULES_ISSUED: u64 = 0;
static mut PAGE_MODULES_LANDED: u64 = 0;
static mut PAGE_MODULE_MEMBERS: u64 = 0;

#[no_mangle]
pub extern "C" fn jit_set_page_modules(on: u32) {
    unsafe { PAGE_MODULES_ON = on != 0 }
}
/// Blocks in the cache beyond which batching stops (see BATCH_ON).
/// NOTE: population alone is NOT a sufficient gate — python fib still ran
/// 180s with a 4096-block cap, because the storm happens during warm-up
/// while the cache is still small. Batch builds are therefore ALSO charged
/// to the measured build-time budget (SB_BUILD_MS), which is what actually
/// bounds a workload that wants a batch per tier-up.
static mut BATCH_POP_CAP: usize = 4096;
/// Distinct hot code PAGES beyond which batching stops. This is the
/// footprint signal that actually separates the two behaviours: nbench
/// ASSIGNMENT's hot code is a handful of pages and its batches cover most
/// of what runs, while CPython spreads over dozens and its batches are
/// noise it pays for per tier-up. A build-time budget was tried and is too
/// blunt — tight enough to save python (180s -> 3.7s) also erased
/// ASSIGNMENT's gain (10.1 -> 8.3).
static mut BATCH_PAGE_CAP: usize = 64;
/// Batches per billion retired instructions above which batching is judged
/// unprofitable and switched off for the run (see the rate governor).
static mut BATCH_RATE_CAP: u64 = 200;
#[no_mangle]
pub extern "C" fn jit_set_batch_rate_cap(v: u32) {
    unsafe { BATCH_RATE_CAP = v as u64 }
}
#[no_mangle]
pub extern "C" fn jit_set_batch_page_cap(v: u32) {
    unsafe { BATCH_PAGE_CAP = v as usize }
}
#[no_mangle]
pub extern "C" fn jit_set_batch_pop_cap(v: u32) {
    unsafe { BATCH_POP_CAP = v as usize }
}
/// Consecutive identical successors before a trace is recompiled with an
/// inline cache through its terminating indirect jump. 256 measured best:
/// the extension costs one recompile per pc, so a higher bar spends that
/// only on genuinely stable edges (python fib 3520 -> 3356ms, compile
/// 4505 -> 4468ms against trigger 64).
static mut IC_EXTEND_TRIGGER: u32 = 256;
static mut IC_EXTENDS: u64 = 0;
#[no_mangle]
pub extern "C" fn jit_set_ic_trigger(v: u32) {
    unsafe { IC_EXTEND_TRIGGER = if v == 0 { u32::MAX } else { v } }
}
static mut BATCH_CAP: usize = 32;
static mut BATCH_PAGE: bool = false;
#[no_mangle]
pub extern "C" fn jit_set_batch_page(on: u32) {
    unsafe { BATCH_PAGE = on != 0 }
}
static mut BATCH_BAR_SHIFT: u32 = 1;
#[no_mangle]
pub extern "C" fn jit_set_batch_cap(v: u32) {
    unsafe { BATCH_CAP = v as usize }
}
#[no_mangle]
pub extern "C" fn jit_set_batch_bar_shift(v: u32) {
    unsafe { BATCH_BAR_SHIFT = v }
}
static mut BATCHES: u64 = 0;
static mut BATCH_MEMBERS: u64 = 0;
#[no_mangle]
pub extern "C" fn jit_set_batch(on: u32) {
    unsafe { BATCH_ON = on != 0 }
}
#[allow(static_mut_refs)]
fn batch_cell_addr(i: usize) -> u32 {
    unsafe { &BATCH_BASE_POOL[i % BATCH_CELLS] as *const u32 as u32 }
}

/// Live chain kill switch (see JitLayout::chain_off_addr): nonzero disables
/// every emitted chain transfer. Driven by CODE-CHURN RATE at quantum
/// boundaries: a workload still compiling new blocks (tcc churns ~7.5k
/// blocks across its entire run; CPython warms for seconds) has an
/// unstable, megamorphic chain graph that V8's ICs cannot serve — measured
/// 2-2.9x slower chained. A warmed workload (nbench kernels self-time for
/// minutes after a burst of compiles) has a stable graph where chained
/// hops cost ~2ns and measured up to +23%. Population caps cannot separate
/// the two (cumulative counts overlap); churn does.
static mut CHAIN_OFF_CELL: u32 = 0;
static mut COMPILES_TICK: u64 = 0;
fn chain_off_addr() -> u32 {
    (&raw const CHAIN_OFF_CELL) as u32
}

/// Online chain controller: no static rule separates workloads whose chain
/// graph V8 serves at ~2ns/hop (warm nbench kernels: +23% and an
/// ASSIGNMENT row that flips to a WIN) from those it cannot (tcc's 7.5k-
/// block soup: 2-2.9x slower; population, churn and per-site target-kind
/// gates all failed to split them). So MEASURE: alternate ON/OFF probe
/// windows of PROBE_QUANTA boundaries, compare wall-ns per retired
/// instruction, lock the faster setting for LOCK_QUANTA, then re-probe
/// (workloads change phases). Compiling a new block unlocks immediately.
struct ChainCtl {
    state: u8, // 0 = probing ON, 1 = probing OFF, 2 = locked
    quanta: u32,
    t0_ms: f64,
    retired0: u64,
    ns_per_insn: [f64; 2],
    locked_off: u32,
}
static mut CHAIN_CTL: ChainCtl = ChainCtl {
    state: 0,
    quanta: 0,
    t0_ms: 0.0,
    retired0: 0,
    ns_per_insn: [0.0; 2],
    locked_off: 0,
};
const PROBE_QUANTA: u32 = 8;
const LOCK_QUANTA: u32 = 256;

#[allow(static_mut_refs)]
fn chain_ctl_boundary(retired_total: u64) {
    unsafe {
        let ctl = &mut CHAIN_CTL;
        ctl.quanta += 1;
        let now = host_now_ms();
        match ctl.state {
            0 | 1 => {
                if ctl.quanta >= PROBE_QUANTA {
                    let insns = retired_total.wrapping_sub(ctl.retired0).max(1);
                    ctl.ns_per_insn[ctl.state as usize] = (now - ctl.t0_ms) * 1e6 / insns as f64;
                    if ctl.state == 0 {
                        ctl.state = 1;
                        CHAIN_OFF_CELL = 1;
                    } else {
                        // Verdict: lock the faster setting.
                        ctl.locked_off = (ctl.ns_per_insn[1] < ctl.ns_per_insn[0]) as u32;
                        CHAIN_OFF_CELL = ctl.locked_off;
                        ctl.state = 2;
                    }
                    ctl.quanta = 0;
                    ctl.t0_ms = now;
                    ctl.retired0 = retired_total;
                }
            }
            _ => {
                if ctl.quanta >= LOCK_QUANTA {
                    ctl.state = 0;
                    CHAIN_OFF_CELL = 0;
                    ctl.quanta = 0;
                    ctl.t0_ms = now;
                    ctl.retired0 = retired_total;
                }
            }
        }
    }
}

/// Trace aggressiveness for individual blocks (see rv64_jit::set_trace_level):
/// 0 = classic basic blocks, 1 = branch side-exits, 2 = +call following,
/// 3 = +return following (default).
#[no_mangle]
pub extern "C" fn jit_set_trace_level(l: u32) {
    rv64_jit::set_trace_level(l);
}

/// Select host-filled TLB misses inside compiled blocks.
/// `0` forces the old bail-to-interpreter path, `1` forces refill, and `2`
/// restores the adaptive product policy.
#[no_mangle]
pub extern "C" fn jit_set_tlb_fill(mode: u32) {
    unsafe {
        TLB_FILL_POLICY = match mode {
            0 => TLB_FILL_OFF,
            1 => TLB_FILL_ON,
            _ => TLB_FILL_AUTO,
        };
    }
    reset_tlb_fill_policy();
}

/// Fused-TLB refill for compiled blocks: called from generated code (a
/// wasm->wasm call through the module's `env.tlb_fill` import) when an inline
/// probe misses. Returns the offset such that `linear = va + off`, or -1 when
/// the access can't be served inline (unmapped, permission fault, MMIO, or a
/// page holding compiled code) — the block then bails and the interpreter
/// re-executes the instruction, raising the exact architectural fault.
///
/// Reentrancy: this runs synchronously inside `call_block`. The explicit
/// context owns the concrete CPU/bus callback for that call, so this ABI does
/// not consult a global machine or assume one machine layout.
#[no_mangle]
pub extern "C" fn jit_tlb_fill(context: i32, va: u64, store: u32) -> i64 {
    unsafe {
        if context != FULL_SYSTEM_CONTEXT_HANDLE || !FULL_SYSTEM_DISPATCH_ACTIVE {
            return -1;
        }
        let Some(mut context) = active_jit_context() else {
            return -1;
        };
        TLB_FILLS += 1;
        context.fill_tlb(va, store)
    }
}

// ---- Generated JIT module transfer -----------------------------------------

static mut JIT_OUT: Vec<u8> = Vec::new();

#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn jit_out_ptr() -> *const u8 {
    unsafe { JIT_OUT.as_ptr() }
}

#[no_mangle]
#[allow(static_mut_refs)]
pub extern "C" fn jit_out_len() -> u32 {
    unsafe {
        if MEMPROF_MODE & 4 != 0 {
            let len = JIT_OUT.len() as u64;
            MEMPROF[9] += len;
            MEMPROF[10] += 1;
            let bucket = if len <= 1024 {
                0
            } else if len <= 4096 {
                1
            } else if len <= 16384 {
                2
            } else {
                3
            };
            MEMPROF[11 + bucket] += 1;
            MEMPROF[15 + bucket] += len;
        }
        JIT_OUT.len() as u32
    }
}
