//! virtio-mmio (version 2) transport with split virtqueues, block, and network
//! backends.

use crate::{checked_ram_range, JitPageState};

/// Device backends the transport can host.
pub enum Backend {
    /// virtio-blk (device id 2), backed by an in-memory disk image.
    Block { disk: Vec<u8> },
    /// virtio-blk backed by a native file service. The device keeps only the
    /// descriptor that is waiting for the host operation; the file remains
    /// outside Wasm memory.
    ExternalBlock {
        size: u64,
        pending: Option<PendingBlockRequest>,
    },
    /// virtio-net (device id 1). RX = queue 0, TX = queue 1.
    ///
    /// The device works purely at layer 2: it moves whole Ethernet frames
    /// between the guest's queues and these two mailboxes, and knows nothing
    /// about ARP, IP or TCP. Where the frames actually go is the host layer's
    /// problem — see `ws.rs` and `web/rv64.js` for the WebSocket relay.
    Net {
        mac: [u8; 6],
        /// Frames from the host, awaiting an RX buffer from the guest.
        inbox: Vec<Vec<u8>>,
        /// Frames the guest has sent, awaiting collection by the host.
        outbox: Vec<Vec<u8>>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingBlockRequest {
    pub id: u64,
    pub kind: BlockRequestKind,
    pub offset: u64,
    pub data: Vec<u8>,
    length: u64,
    read_buffers: Vec<(u64, u32)>,
    status_addr: u64,
    queue_index: usize,
    head: u16,
    used_index_addr: u64,
    used_entry: u64,
    used_idx: u16,
}

impl PendingBlockRequest {
    pub fn len(&self) -> u64 {
        self.length
    }

    pub fn is_empty(&self) -> bool {
        self.length == 0
    }

    pub fn data(&self) -> &[u8] {
        &self.data
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockRequestKind {
    Read,
    Write,
    Flush,
}

const MAX_QUEUES: usize = 2;

/// Bytes of `struct virtio_net_hdr_v1` in front of every frame in both
/// directions. 12 rather than 10 because we negotiate `VIRTIO_F_VERSION_1`,
/// which makes the header carry `num_buffers` — Linux sizes `vi->hdr_len` on
/// exactly that condition, so a 10-byte header would desynchronise every frame.
const NET_HDR_LEN: usize = 12;

/// Largest frame we will move: Ethernet MTU + header + VLAN slack.
const NET_MAX_FRAME: usize = 1600;

/// MAC handed to the guest unless the caller picks one. Locally administered
/// (bit 1 of the first octet), unicast. Two guests sharing one relay must NOT
/// share this — give the second a different address.
pub const DEFAULT_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0x12, 0x34, 0x56];

/// How many frames may queue up in either direction before we drop. A guest
/// that stops posting RX buffers, or a host that stops collecting, must not
/// grow our memory without bound.
const NET_QUEUE_LIMIT: usize = 256;

fn vio_dbg() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var("RV_PLIC_DEBUG").is_ok())
}

#[derive(Default, Clone)]
struct Queue {
    ready: u32,
    num: u32,
    desc: u64,
    avail: u64,
    used: u64,
    last_avail_idx: u16,
}

pub struct VirtioDev {
    pub backend: Backend,
    status: u32,
    device_features_sel: u32,
    driver_features_sel: u32,
    queue_sel: u32,
    queues: [Queue; MAX_QUEUES],
    /// bit 0: used-ring update pending
    pub int_status: u32,
}

// virtio-blk request types
const VIRTIO_BLK_T_IN: u32 = 0; // read
const VIRTIO_BLK_T_OUT: u32 = 1; // write
const VIRTIO_BLK_T_FLUSH: u32 = 4;
const VIRTIO_BLK_F_SIZE_MAX: u32 = 1;
const VIRTIO_BLK_F_SEG_MAX: u32 = 2;
const MAX_DISK_OPERATION: u64 = 64 * 1024;
const MAX_DISK_SEGMENTS: u32 = 1;

const SECTOR: usize = 512;

impl VirtioDev {
    pub fn new(backend: Backend) -> VirtioDev {
        VirtioDev {
            backend,
            status: 0,
            device_features_sel: 0,
            driver_features_sel: 0,
            queue_sel: 0,
            queues: Default::default(),
            int_status: 0,
        }
    }

    pub fn device_id(&self) -> u32 {
        match self.backend {
            Backend::Block { .. } | Backend::ExternalBlock { .. } => 2,
            Backend::Net { .. } => 1,
        }
    }

    /// Feature bits for word `sel` (0 = bits 0-31, 1 = bits 32-63).
    fn device_features(&self, sel: u32) -> u32 {
        match sel {
            // bit 32: VIRTIO_F_VERSION_1 — modern queue layout.
            1 => 1,
            0 => match self.backend {
                // bit 5: VIRTIO_NET_F_MAC — the MAC in config space is ours to
                // give. Offering nothing else keeps the driver off checksum
                // offload, GSO and mergeable RX buffers, none of which a
                // frame-shuffling device benefits from.
                Backend::Net { .. } => 1 << 5,
                Backend::ExternalBlock { .. } => {
                    (1 << VIRTIO_BLK_F_SIZE_MAX) | (1 << VIRTIO_BLK_F_SEG_MAX) | (1 << 9)
                    // SIZE_MAX, SEG_MAX, FLUSH
                }
                _ => 0,
            },
            _ => 0,
        }
    }

    /// Ring depth advertised to the driver.
    fn queue_num_max(&self) -> u32 {
        match self.backend {
            // Modern virtio_net stops a TX queue unless it has enough free
            // descriptors for a maximally fragmented skb plus its header.
            // A 16-entry ring can transmit once and then remain stopped even
            // after we publish the used entry; 256 is the conventional size.
            Backend::Net { .. } => 256,
            _ => 16,
        }
    }

    /// True when this device's interrupt line should be raised.
    pub fn irq_pending(&self) -> bool {
        self.int_status != 0
    }

    /// (ready, num, avail_addr, used_addr, my last_avail_idx) for queue `qi`.
    pub fn queue_debug(&self, qi: usize) -> Option<(u32, u32, u64, u64, u16)> {
        self.queues
            .get(qi)
            .filter(|q| q.ready != 0)
            .map(|q| (q.ready, q.num, q.avail, q.used, q.last_avail_idx))
    }

    pub fn read(&mut self, offset: u64) -> u32 {
        match offset {
            0x000 => 0x7472_6976, // magic "virt"
            0x004 => 2,           // version
            0x008 => self.device_id(),
            0x00c => 0xffff, // vendor
            0x010 => self.device_features(self.device_features_sel),
            0x034 => self.queue_num_max(),
            0x044 => self.q().ready,
            0x060 => self.int_status,
            0x070 => self.status,
            0x0fc => 0, // config generation
            _ if offset >= 0x100 => {
                let o = offset - 0x100;
                u32::from_le_bytes([
                    self.config_u8(o),
                    self.config_u8(o + 1),
                    self.config_u8(o + 2),
                    self.config_u8(o + 3),
                ])
            }
            _ => 0,
        }
    }

    /// MMIO read of `size` bytes (1, 2 or 4).
    pub fn read_sized(&mut self, offset: u64, size: u32) -> u32 {
        if offset >= 0x100 && size < 4 {
            let o = offset - 0x100;
            let mut v = 0u32;
            for i in 0..size as u64 {
                v |= (self.config_u8(o + i) as u32) << (8 * i);
            }
            return v;
        }
        self.read(offset)
    }

    /// Returns true if the write requires queue processing (a notify).
    pub fn write(&mut self, offset: u64, val: u32) -> Option<u32> {
        match offset {
            0x014 => self.device_features_sel = val,
            0x024 => self.driver_features_sel = val,
            0x030 => self.queue_sel = val.min(MAX_QUEUES as u32 - 1),
            0x038 => {
                let max = self.queue_num_max();
                self.qm().num = val.min(max);
            }
            0x044 => self.qm().ready = val & 1,
            0x050 => return Some(val), // QueueNotify -> process queue `val`
            0x064 => {
                if vio_dbg() {
                    eprintln!("[vio] ACK int_status {:#x} &= !{:#x}", self.int_status, val);
                }
                self.int_status &= !val;
            }
            0x070 => {
                self.status = val;
                if val == 0 {
                    // reset
                    self.queues = Default::default();
                    self.int_status = 0;
                    if let Backend::ExternalBlock { pending, .. } = &mut self.backend {
                        *pending = None;
                    }
                }
            }
            0x080 => set_lo(&mut self.qm().desc, val),
            0x084 => set_hi(&mut self.qm().desc, val),
            0x090 => set_lo(&mut self.qm().avail, val),
            0x094 => set_hi(&mut self.qm().avail, val),
            0x0a0 => set_lo(&mut self.qm().used, val),
            0x0a4 => set_hi(&mut self.qm().used, val),
            _ => {}
        }
        None
    }

    fn q(&self) -> &Queue {
        &self.queues[self.queue_sel as usize]
    }
    fn qm(&mut self) -> &mut Queue {
        &mut self.queues[self.queue_sel as usize]
    }

    /// One byte of device-specific config space (`off` is relative to 0x100).
    fn config_u8(&self, off: u64) -> u8 {
        match &self.backend {
            Backend::Block { disk } => {
                // struct virtio_blk_config { le64 capacity; ... } in sectors.
                let sectors = (disk.len() / SECTOR) as u64;
                sectors
                    .to_le_bytes()
                    .get(off as usize)
                    .copied()
                    .unwrap_or(0)
            }
            Backend::ExternalBlock { size, .. } => {
                let sectors = (size / SECTOR as u64).to_le_bytes();
                let size_max = (MAX_DISK_OPERATION as u32).to_le_bytes();
                let seg_max = MAX_DISK_SEGMENTS.to_le_bytes();
                match off {
                    0..=7 => sectors[off as usize],
                    8..=11 => size_max[(off - 8) as usize],
                    12..=15 => seg_max[(off - 12) as usize],
                    _ => 0,
                }
            }
            Backend::Net { mac, .. } => {
                // struct virtio_net_config { u8 mac[6]; le16 status; ... }
                // status stays 0: VIRTIO_NET_F_STATUS is not offered, so the
                // driver assumes the link is up and never reads it.
                mac.get(off as usize).copied().unwrap_or(0)
            }
        }
    }

    /// Queue a frame from the host for delivery to the guest. Dropped if the
    /// guest is not draining — the same thing a real NIC does when its ring
    /// backs up.
    pub fn net_input(&mut self, frame: &[u8]) {
        if let Backend::Net { inbox, .. } = &mut self.backend {
            if frame.len() <= NET_MAX_FRAME && inbox.len() < NET_QUEUE_LIMIT {
                inbox.push(frame.to_vec());
            }
        }
    }

    /// Collect the frames the guest has transmitted.
    pub fn net_take_output(&mut self) -> Vec<Vec<u8>> {
        if let Backend::Net { outbox, .. } = &mut self.backend {
            core::mem::take(outbox)
        } else {
            Vec::new()
        }
    }

    /// True when this device has inbound frames waiting for an RX buffer.
    pub fn net_rx_pending(&self) -> bool {
        matches!(&self.backend, Backend::Net { inbox, .. } if !inbox.is_empty())
    }

    // ---- virtqueue processing ------------------------------------------

    /// Process queue `qi` (after a notify, or when console input arrives).
    /// `ram`/`ram_base` give access to guest physical memory.
    pub fn process(&mut self, qi: usize, ram: &mut [u8], ram_base: u64, jit: &mut JitPageState) {
        if qi >= MAX_QUEUES || self.queues[qi].ready == 0 {
            if vio_dbg() {
                eprintln!(
                    "[vio] notify q{qi} BAILED ready={}",
                    self.queues.get(qi).map_or(0, |q| q.ready)
                );
            }
            return;
        }
        if matches!(
            &self.backend,
            Backend::ExternalBlock {
                pending: Some(_),
                ..
            }
        ) {
            return;
        }
        let mut serviced = 0u32;
        loop {
            let q = self.queues[qi].clone();
            if q.num == 0 {
                return;
            }
            let Some(avail_idx) = q
                .avail
                .checked_add(2)
                .and_then(|addr| read16(ram, ram_base, addr))
            else {
                return;
            };
            if q.last_avail_idx == avail_idx {
                if vio_dbg() {
                    eprintln!("[vio] notify q{qi} done serviced={serviced} last_avail={} avail_idx={avail_idx}",
                        q.last_avail_idx);
                }
                break;
            }
            let slot = (q.last_avail_idx as u64) % (q.num as u64);
            let Some(head) = q
                .avail
                .checked_add(4 + slot * 2)
                .and_then(|addr| read16(ram, ram_base, addr))
            else {
                return;
            };

            // Walk the descriptor chain.
            let chain = (|| {
                let mut chain: Vec<(u64, u32, bool)> = Vec::new();
                let mut di = head as u64;
                for _ in 0..q.num {
                    let base = q.desc.checked_add(di.checked_mul(16)?)?;
                    let addr = read64(ram, ram_base, base)?;
                    let len = read32(ram, ram_base, base.checked_add(8)?)?;
                    let flags = read16(ram, ram_base, base.checked_add(12)?)?;
                    chain.push((addr, len, flags & 2 != 0));
                    if flags & 1 == 0 {
                        break;
                    }
                    di = read16(ram, ram_base, base.checked_add(14)?)? as u64;
                }
                Some(chain)
            })();
            let Some(chain) = chain else {
                return;
            };

            // Validate the used-ring destination before the backend performs
            // any externally visible work.
            let Some(used_index_addr) = q.used.checked_add(2) else {
                return;
            };
            let Some(used_idx) = read16(ram, ram_base, used_index_addr) else {
                return;
            };
            let uslot = (used_idx as u64) % (q.num as u64);
            let Some(used_entry) = q.used.checked_add(4 + uslot * 8) else {
                return;
            };
            if checked_ram_range(ram.len(), ram_base, used_entry, 8).is_none() {
                return;
            }

            let written = self.service(qi, &chain, ram, ram_base, jit);
            if written.is_none() {
                // Not serviceable now (e.g. console RX with no input):
                // leave the descriptor for later.
                if let Backend::ExternalBlock {
                    pending: Some(pending),
                    ..
                } = &mut self.backend
                {
                    pending.head = head;
                    pending.used_index_addr = used_index_addr;
                    pending.used_entry = used_entry;
                    pending.used_idx = used_idx;
                }
                break;
            }

            // Publish to the used ring.
            write32(ram, ram_base, used_entry, head as u32, jit);
            write32(ram, ram_base, used_entry + 4, written.unwrap(), jit);
            write16(
                ram,
                ram_base,
                used_index_addr,
                used_idx.wrapping_add(1),
                jit,
            );

            self.queues[qi].last_avail_idx = self.queues[qi].last_avail_idx.wrapping_add(1);
            self.int_status |= 1;
            serviced += 1;
        }
    }

    /// Return the one host operation that is waiting for completion.
    pub fn pending_block_request(&self) -> Option<PendingBlockRequest> {
        match &self.backend {
            Backend::ExternalBlock { pending, .. } => pending.clone(),
            _ => None,
        }
    }

    pub fn has_pending_block_request(&self) -> bool {
        matches!(
            &self.backend,
            Backend::ExternalBlock {
                pending: Some(_),
                ..
            }
        )
    }

    /// Complete a native disk operation and publish its guest descriptor.
    /// `data` is required only for reads; writes and flushes use an empty body.
    pub fn complete_block_request(
        &mut self,
        id: u64,
        data: &[u8],
        ok: bool,
        ram: &mut [u8],
        ram_base: u64,
        jit: &mut JitPageState,
    ) -> bool {
        let Backend::ExternalBlock { pending, .. } = &mut self.backend else {
            return false;
        };
        let Some(request) = pending.take() else {
            return false;
        };
        if request.id != id {
            *pending = Some(request);
            return false;
        }

        // A completion is one commit. Validate every destination before the
        // first guest write so an invalid scatter list cannot produce a
        // partially updated read buffer.
        let read_targets_valid = request.kind != BlockRequestKind::Read
            || request.read_buffers.iter().all(|&(addr, len)| {
                checked_ram_range(ram.len(), ram_base, addr, len as usize).is_some()
            });
        let completion_targets_valid = read_targets_valid
            && checked_ram_range(ram.len(), ram_base, request.status_addr, 1).is_some()
            && checked_ram_range(ram.len(), ram_base, request.used_entry, 8).is_some()
            && checked_ram_range(ram.len(), ram_base, request.used_index_addr, 2).is_some();
        if !completion_targets_valid {
            *pending = Some(request);
            return false;
        }

        let data_valid = match request.kind {
            BlockRequestKind::Read => data.len() as u64 == request.length,
            BlockRequestKind::Write | BlockRequestKind::Flush => data.is_empty(),
        };
        let mut success = ok && data_valid;
        if success && request.kind == BlockRequestKind::Read {
            let mut copied = 0usize;
            for &(addr, len) in &request.read_buffers {
                let end = copied + len as usize;
                success &= guest_write(ram, ram_base, addr, &data[copied..end], jit);
                copied = end;
            }
        }
        success &= guest_write(
            ram,
            ram_base,
            request.status_addr,
            &[if success { 0 } else { 1 }],
            jit,
        );
        write32(ram, ram_base, request.used_entry, request.head as u32, jit);
        let written = if request.kind == BlockRequestKind::Read && success {
            request.length as u32 + 1
        } else {
            1
        };
        write32(ram, ram_base, request.used_entry + 4, written, jit);
        write16(
            ram,
            ram_base,
            request.used_index_addr,
            request.used_idx.wrapping_add(1),
            jit,
        );
        self.queues[request.queue_index].last_avail_idx = self.queues[request.queue_index]
            .last_avail_idx
            .wrapping_add(1);
        self.int_status |= 1;
        true
    }

    /// Service one descriptor chain; returns bytes written to guest buffers,
    /// or None if the request can't be serviced yet.
    fn service(
        &mut self,
        qi: usize,
        chain: &[(u64, u32, bool)],
        ram: &mut [u8],
        ram_base: u64,
        jit: &mut JitPageState,
    ) -> Option<u32> {
        match &mut self.backend {
            Backend::Block { disk } => {
                // Layout: header (16B, read-only) | data buffers | status (1B, writable)
                let (hdr_addr, ..) = *chain.first()?;
                let Some(header) = guest_slice(ram, ram_base, hdr_addr, 16) else {
                    return Some(0);
                };
                let req_type = u32::from_le_bytes(header[..4].try_into().unwrap());
                let sector = u64::from_le_bytes(header[8..16].try_into().unwrap());
                let mut pos = usize::try_from(sector)
                    .ok()
                    .and_then(|sector| sector.checked_mul(SECTOR));
                let mut written = 0u32;
                let mut ok = true;

                for &(addr, len_u32, writable) in chain.get(1..chain.len().saturating_sub(1))? {
                    let len = len_u32 as usize;
                    let disk_range = pos
                        .and_then(|start| start.checked_add(len).map(|end| (start, end)))
                        .filter(|&(_, end)| end <= disk.len());
                    match req_type {
                        VIRTIO_BLK_T_IN if writable => {
                            if !disk_range.is_some_and(|(start, end)| {
                                guest_write(ram, ram_base, addr, &disk[start..end], jit)
                            }) {
                                ok = false;
                            }
                            written += len as u32;
                        }
                        VIRTIO_BLK_T_OUT if !writable => {
                            let source = guest_slice(ram, ram_base, addr, len_u32);
                            if let (Some((start, end)), Some(source)) = (disk_range, source) {
                                disk[start..end].copy_from_slice(source);
                            } else {
                                ok = false;
                            }
                        }
                        _ => ok = false,
                    }
                    pos = pos.and_then(|position| position.checked_add(len));
                }

                // status byte in the last descriptor
                if let Some(&(saddr, _, _)) = chain.last() {
                    let _ = guest_write(ram, ram_base, saddr, &[if ok { 0 } else { 1 }], jit);
                    written += 1;
                }
                Some(written)
            }
            Backend::ExternalBlock { size, pending } => {
                if qi != 0 {
                    return Some(0);
                }
                if pending.is_some() {
                    return None;
                }
                let Some(&(status_addr, status_len, status_writable)) = chain.last() else {
                    return Some(0);
                };
                if status_len < 1
                    || !status_writable
                    || checked_ram_range(ram.len(), ram_base, status_addr, 1).is_none()
                {
                    return Some(0);
                }
                let reject = |ram: &mut [u8], jit: &mut JitPageState| {
                    let _ = guest_write(ram, ram_base, status_addr, &[1], jit);
                    Some(1)
                };
                let Some(&(hdr_addr, hdr_len, hdr_writable)) = chain.first() else {
                    return Some(0);
                };
                if chain.len() < 2 || hdr_len < 16 || hdr_writable {
                    return reject(ram, jit);
                }
                let Some(header) = guest_slice(ram, ram_base, hdr_addr, 16) else {
                    return reject(ram, jit);
                };
                let req_type = u32::from_le_bytes(header[..4].try_into().unwrap());
                let kind = match req_type {
                    VIRTIO_BLK_T_IN => BlockRequestKind::Read,
                    VIRTIO_BLK_T_OUT => BlockRequestKind::Write,
                    VIRTIO_BLK_T_FLUSH => BlockRequestKind::Flush,
                    _ => return reject(ram, jit),
                };
                let sector = u64::from_le_bytes(header[8..16].try_into().unwrap());
                let Some(offset) = sector.checked_mul(SECTOR as u64) else {
                    return reject(ram, jit);
                };
                let body = chain.get(1..chain.len().saturating_sub(1))?;
                let mut body_len = 0u64;
                let mut read_buffers = Vec::new();
                for &(addr, len, writable) in body {
                    let descriptor_valid = match kind {
                        BlockRequestKind::Read => {
                            let valid = writable
                                && checked_ram_range(ram.len(), ram_base, addr, len as usize)
                                    .is_some();
                            if valid {
                                read_buffers.push((addr, len));
                            }
                            valid
                        }
                        BlockRequestKind::Write => {
                            !writable && guest_slice(ram, ram_base, addr, len).is_some()
                        }
                        BlockRequestKind::Flush => len == 0,
                    };
                    if !descriptor_valid {
                        return reject(ram, jit);
                    }
                    let Some(next) = body_len.checked_add(len as u64) else {
                        return reject(ram, jit);
                    };
                    if next > MAX_DISK_OPERATION {
                        return reject(ram, jit);
                    }
                    body_len = next;
                }
                if kind == BlockRequestKind::Flush && chain.len() != 2 {
                    return reject(ram, jit);
                }
                let range_end = match kind {
                    BlockRequestKind::Flush => 0,
                    BlockRequestKind::Read | BlockRequestKind::Write => {
                        let Some(end) = offset.checked_add(body_len) else {
                            return reject(ram, jit);
                        };
                        end
                    }
                };
                if range_end > *size
                    || (kind != BlockRequestKind::Flush && body_len == 0)
                    || (kind == BlockRequestKind::Flush && body_len != 0)
                {
                    return reject(ram, jit);
                }
                let data = if kind == BlockRequestKind::Write {
                    let mut data = Vec::with_capacity(body_len as usize);
                    for &(addr, len, _) in body {
                        let source = guest_slice(ram, ram_base, addr, len)
                            .expect("write source was validated");
                        data.extend_from_slice(source);
                    }
                    data
                } else {
                    Vec::new()
                };
                let id = NEXT_BLOCK_REQUEST_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                *pending = Some(PendingBlockRequest {
                    id,
                    kind,
                    offset,
                    data,
                    length: body_len,
                    read_buffers,
                    status_addr,
                    queue_index: qi,
                    head: 0,
                    used_index_addr: 0,
                    used_entry: 0,
                    used_idx: 0,
                });
                None
            }
            Backend::Net { inbox, outbox, .. } => {
                if qi == 0 {
                    // RX: hand the oldest inbound frame to the guest, prefixed
                    // by a zeroed virtio_net_hdr_v1. No frame means we leave the
                    // buffer on the ring for later rather than consuming it.
                    if inbox.is_empty() {
                        return None;
                    }
                    let frame = &inbox[0];
                    let need = NET_HDR_LEN + frame.len();
                    let capacity: usize = chain
                        .iter()
                        .filter(|&&(_, _, writable)| writable)
                        .map(|&(_, len, _)| len as usize)
                        .sum();
                    if capacity < need {
                        // The frame cannot fit the buffer the guest offered.
                        // Drop it: keeping it would wedge the queue forever.
                        inbox.remove(0);
                        return None;
                    }
                    let mut hdr = [0u8; NET_HDR_LEN];
                    // num_buffers = 1: this frame occupies exactly one chain.
                    hdr[10] = 1;
                    let mut written = 0usize;
                    let mut src: Vec<u8> = hdr.to_vec();
                    src.extend_from_slice(frame);
                    for &(addr, len, writable) in chain {
                        if !writable || written >= src.len() {
                            continue;
                        }
                        let n = (src.len() - written).min(len as usize);
                        if !guest_write(ram, ram_base, addr, &src[written..written + n], jit) {
                            break;
                        }
                        written += n;
                    }
                    inbox.remove(0);
                    Some(written as u32)
                } else {
                    // TX: reassemble the frame from the readable descriptors and
                    // strip the header the guest prepended.
                    let mut frame = Vec::new();
                    for &(addr, len, writable) in chain {
                        if writable {
                            continue;
                        }
                        match guest_slice(ram, ram_base, addr, len) {
                            Some(s) => frame.extend_from_slice(s),
                            None => return Some(0),
                        }
                    }
                    if frame.len() > NET_HDR_LEN
                        && frame.len() - NET_HDR_LEN <= NET_MAX_FRAME
                        && outbox.len() < NET_QUEUE_LIMIT
                    {
                        outbox.push(frame[NET_HDR_LEN..].to_vec());
                    }
                    Some(0)
                }
            }
        }
    }
}

static NEXT_BLOCK_REQUEST_ID: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

/// Guest-physical view of a descriptor, or `None` if it does not lie entirely
/// within RAM.
fn guest_slice(ram: &[u8], ram_base: u64, addr: u64, len: u32) -> Option<&[u8]> {
    let range = checked_ram_range(ram.len(), ram_base, addr, len as usize)?;
    Some(&ram[range])
}

fn guest_slice_mut(ram: &mut [u8], ram_base: u64, addr: u64, len: usize) -> Option<&mut [u8]> {
    let range = checked_ram_range(ram.len(), ram_base, addr, len)?;
    Some(&mut ram[range])
}

/// The only device-to-guest memory write primitive. Keeping DMA observation at
/// this boundary prevents a new backend from silently bypassing JIT code-page
/// invalidation.
fn guest_write(
    ram: &mut [u8],
    ram_base: u64,
    addr: u64,
    bytes: &[u8],
    jit: &mut JitPageState,
) -> bool {
    let Some(dst) = guest_slice_mut(ram, ram_base, addr, bytes.len()) else {
        return false;
    };
    dst.copy_from_slice(bytes);
    jit.note_write(addr, bytes.len());
    true
}

fn set_lo(v: &mut u64, val: u32) {
    *v = (*v & !0xffff_ffff) | val as u64;
}
fn set_hi(v: &mut u64, val: u32) {
    *v = (*v & 0xffff_ffff) | ((val as u64) << 32);
}

fn read16(ram: &[u8], base: u64, addr: u64) -> Option<u16> {
    Some(u16::from_le_bytes(
        guest_slice(ram, base, addr, 2)?.try_into().ok()?,
    ))
}
fn read32(ram: &[u8], base: u64, addr: u64) -> Option<u32> {
    Some(u32::from_le_bytes(
        guest_slice(ram, base, addr, 4)?.try_into().ok()?,
    ))
}
fn read64(ram: &[u8], base: u64, addr: u64) -> Option<u64> {
    Some(u64::from_le_bytes(
        guest_slice(ram, base, addr, 8)?.try_into().ok()?,
    ))
}
fn write16(ram: &mut [u8], base: u64, addr: u64, v: u16, jit: &mut JitPageState) {
    let _ = guest_write(ram, base, addr, &v.to_le_bytes(), jit);
}
fn write32(ram: &mut [u8], base: u64, addr: u64, v: u32, jit: &mut JitPageState) {
    let _ = guest_write(ram, base, addr, &v.to_le_bytes(), jit);
}

#[cfg(test)]
mod tests {
    use super::*;

    // A small guest RAM with a hand-built split virtqueue, so these tests
    // exercise the same path a real driver takes: descriptor chain in, reply
    // scattered into the writable descriptors, entry on the used ring.
    const BASE: u64 = 0x8000_0000;
    const DESC: usize = 0x1000;
    const AVAIL: usize = 0x2000;
    const USED: usize = 0x3000;
    const REQ: usize = 0x4000;
    const NUM: u32 = 8;

    /// Ring addresses for one queue. Queue 0 lives at the low set; a
    /// two-queue device (net) puts queue 1 at the high set.
    struct Ring {
        desc: usize,
        avail: usize,
        used: usize,
    }
    const RING0: Ring = Ring {
        desc: DESC,
        avail: AVAIL,
        used: USED,
    };
    const RING1: Ring = Ring {
        desc: 0x6000,
        avail: 0x7000,
        used: 0x8000,
    };

    fn setup_ring(dev: &mut VirtioDev, qi: u32, r: &Ring) {
        dev.write(0x030, qi); // queue_sel
        dev.write(0x038, NUM); // queue_num
        dev.write(0x080, (BASE + r.desc as u64) as u32);
        dev.write(0x084, ((BASE + r.desc as u64) >> 32) as u32);
        dev.write(0x090, (BASE + r.avail as u64) as u32);
        dev.write(0x094, ((BASE + r.avail as u64) >> 32) as u32);
        dev.write(0x0a0, (BASE + r.used as u64) as u32);
        dev.write(0x0a4, ((BASE + r.used as u64) >> 32) as u32);
        dev.write(0x044, 1); // queue_ready
    }

    fn put_desc_in(
        ram: &mut [u8],
        r: &Ring,
        i: usize,
        addr: usize,
        len: u32,
        flags: u16,
        next: u16,
    ) {
        let o = r.desc + i * 16;
        ram[o..o + 8].copy_from_slice(&(BASE + addr as u64).to_le_bytes());
        ram[o + 8..o + 12].copy_from_slice(&len.to_le_bytes());
        ram[o + 12..o + 14].copy_from_slice(&flags.to_le_bytes());
        ram[o + 14..o + 16].copy_from_slice(&next.to_le_bytes());
    }

    /// Put chain head `head` on the avail ring and publish index `n`.
    fn publish(ram: &mut [u8], r: &Ring, head: u16, n: u16) {
        let slot = (n as usize - 1) % NUM as usize;
        ram[r.avail + 4 + slot * 2..r.avail + 6 + slot * 2].copy_from_slice(&head.to_le_bytes());
        ram[r.avail + 2..r.avail + 4].copy_from_slice(&n.to_le_bytes());
    }

    /// (chain head, bytes written) from used-ring entry `n` (1-based).
    fn used_entry(ram: &[u8], r: &Ring, n: u16) -> (u32, u32) {
        let slot = (n as usize - 1) % NUM as usize;
        (
            u32_at(ram, r.used + 4 + slot * 8),
            u32_at(ram, r.used + 8 + slot * 8),
        )
    }

    fn u16_at(ram: &[u8], off: usize) -> u16 {
        u16::from_le_bytes(ram[off..off + 2].try_into().unwrap())
    }
    fn u32_at(ram: &[u8], off: usize) -> u32 {
        u32::from_le_bytes(ram[off..off + 4].try_into().unwrap())
    }

    fn process(dev: &mut VirtioDev, qi: usize, ram: &mut [u8]) {
        let mut jit = JitPageState::new(ram.len());
        dev.process(qi, ram, BASE, &mut jit);
    }

    #[test]
    fn block_config_reports_capacity_in_sectors() {
        let mut dev = VirtioDev::new(Backend::Block {
            disk: vec![0u8; 8 * 512],
        });
        assert_eq!(dev.read(0x100), 8);
        assert_eq!(dev.read(0x104), 0);
        assert_eq!(dev.read(0x034), 16);
    }

    fn block_device() -> (VirtioDev, Vec<u8>) {
        let mut disk = vec![0u8; 4 * SECTOR];
        disk[SECTOR..2 * SECTOR].fill(0x5a);
        (
            VirtioDev::new(Backend::Block { disk }),
            vec![0u8; 64 * 1024],
        )
    }

    fn submit_block(
        dev: &mut VirtioDev,
        ram: &mut [u8],
        request_type: u32,
        sector: u64,
        writable_data: bool,
    ) {
        setup_ring(dev, 0, &RING0);
        ram[REQ..REQ + 4].copy_from_slice(&request_type.to_le_bytes());
        ram[REQ + 8..REQ + 16].copy_from_slice(&sector.to_le_bytes());
        put_desc_in(ram, &RING0, 0, REQ, 16, 1, 1);
        put_desc_in(
            ram,
            &RING0,
            1,
            REQ + 0x100,
            SECTOR as u32,
            1 | if writable_data { 2 } else { 0 },
            2,
        );
        put_desc_in(ram, &RING0, 2, REQ + 0x300, 1, 2, 0);
        publish(ram, &RING0, 0, 1);
        process(dev, 0, ram);
    }

    fn submit_split_block(
        dev: &mut VirtioDev,
        ram: &mut [u8],
        request_type: u32,
        sector: u64,
        writable_data: bool,
        first_len: u32,
    ) {
        assert!(first_len < SECTOR as u32);
        setup_ring(dev, 0, &RING0);
        ram[REQ..REQ + 4].copy_from_slice(&request_type.to_le_bytes());
        ram[REQ + 8..REQ + 16].copy_from_slice(&sector.to_le_bytes());
        put_desc_in(ram, &RING0, 0, REQ, 16, 1, 1);
        put_desc_in(
            ram,
            &RING0,
            1,
            REQ + 0x100,
            first_len,
            1 | if writable_data { 2 } else { 0 },
            2,
        );
        put_desc_in(
            ram,
            &RING0,
            2,
            REQ + 0x200,
            SECTOR as u32 - first_len,
            1 | if writable_data { 2 } else { 0 },
            3,
        );
        put_desc_in(ram, &RING0, 3, REQ + 0x400, 1, 2, 0);
        publish(ram, &RING0, 0, 1);
        process(dev, 0, ram);
    }

    #[test]
    fn block_reads_a_sector_into_guest_memory() {
        let (mut dev, mut ram) = block_device();
        submit_block(&mut dev, &mut ram, VIRTIO_BLK_T_IN, 1, true);

        assert_eq!(&ram[REQ + 0x100..REQ + 0x100 + SECTOR], &[0x5a; SECTOR]);
        assert_eq!(ram[REQ + 0x300], 0);
        assert_eq!(u16_at(&ram, USED + 2), 1);
    }

    #[test]
    fn block_writes_persist_in_the_backing_image() {
        let (mut dev, mut ram) = block_device();
        ram[REQ + 0x100..REQ + 0x100 + SECTOR].fill(0xa5);
        submit_block(&mut dev, &mut ram, VIRTIO_BLK_T_OUT, 2, false);

        let Backend::Block { disk } = &dev.backend else {
            unreachable!()
        };
        assert_eq!(&disk[2 * SECTOR..3 * SECTOR], &[0xa5; SECTOR]);
        assert_eq!(ram[REQ + 0x300], 0);
    }

    #[test]
    fn block_rejects_requests_beyond_the_image() {
        let (mut dev, mut ram) = block_device();
        submit_block(&mut dev, &mut ram, VIRTIO_BLK_T_IN, 4, true);

        assert_eq!(ram[REQ + 0x300], 1);
        assert_eq!(u16_at(&ram, USED + 2), 1);
    }

    fn external_block_device() -> (VirtioDev, Vec<u8>) {
        (
            VirtioDev::new(Backend::ExternalBlock {
                size: 4 * SECTOR as u64,
                pending: None,
            }),
            vec![0u8; 64 * 1024],
        )
    }

    #[test]
    fn external_block_advertises_capacity_limits_and_flush() {
        let (mut dev, _) = external_block_device();
        assert_eq!(dev.read(0x100), 4);
        assert_eq!(dev.read(0x108), MAX_DISK_OPERATION as u32);
        assert_eq!(dev.read(0x10c), MAX_DISK_SEGMENTS);
        dev.write(0x014, 0);
        assert_eq!(
            dev.read(0x010),
            (1 << VIRTIO_BLK_F_SIZE_MAX) | (1 << VIRTIO_BLK_F_SEG_MAX) | (1 << 9),
            "VIRTIO_BLK_F_SIZE_MAX | VIRTIO_BLK_F_SEG_MAX | VIRTIO_BLK_F_FLUSH"
        );
        dev.write(0x014, 1);
        assert_eq!(dev.read(0x010), 1, "VIRTIO_F_VERSION_1");
    }

    #[test]
    fn external_block_read_commits_exact_host_data() {
        let (mut dev, mut ram) = external_block_device();
        ram[REQ + 0x100..REQ + 0x100 + SECTOR].fill(0xa5);
        submit_block(&mut dev, &mut ram, VIRTIO_BLK_T_IN, 1, true);

        let request = dev.pending_block_request().expect("pending read");
        assert_eq!(request.kind, BlockRequestKind::Read);
        assert_eq!(request.offset, SECTOR as u64);
        assert_eq!(request.len(), SECTOR as u64);
        assert_eq!(u16_at(&ram, USED + 2), 0, "descriptor stays pending");

        let mut jit = JitPageState::new(ram.len());
        let data = vec![0x5a; SECTOR];
        assert!(dev.complete_block_request(request.id, &data, true, &mut ram, BASE, &mut jit,));
        assert_eq!(&ram[REQ + 0x100..REQ + 0x100 + SECTOR], &data);
        assert_eq!(ram[REQ + 0x300], 0);
        assert_eq!(u16_at(&ram, USED + 2), 1);
        assert_eq!(used_entry(&ram, &RING0, 1), (0, SECTOR as u32 + 1));
        assert!(!dev.has_pending_block_request());
        assert!(dev.irq_pending());
        assert!(!dev.complete_block_request(request.id, &data, true, &mut ram, BASE, &mut jit,));
        assert_eq!(u16_at(&ram, USED + 2), 1, "completion publishes once");
    }

    #[test]
    fn external_block_failed_reads_do_not_modify_guest_data() {
        let cases = [
            ("short", vec![0x5a; SECTOR - 1], true),
            ("long", vec![0x5a; SECTOR + 1], true),
            ("host failure", vec![0x5a; SECTOR], false),
        ];

        for (case, data, ok) in cases {
            let (mut dev, mut ram) = external_block_device();
            ram[REQ + 0x100..REQ + 0x100 + SECTOR].fill(0xa5);
            submit_block(&mut dev, &mut ram, VIRTIO_BLK_T_IN, 1, true);
            let request = dev.pending_block_request().expect("pending read");
            let mut jit = JitPageState::new(ram.len());

            assert!(dev.complete_block_request(request.id, &data, ok, &mut ram, BASE, &mut jit,));
            assert_eq!(
                &ram[REQ + 0x100..REQ + 0x100 + SECTOR],
                &[0xa5; SECTOR],
                "{case} modified the guest read buffer"
            );
            assert_eq!(ram[REQ + 0x300], 1, "{case} must report IOERR");
            assert_eq!(u16_at(&ram, USED + 2), 1);
            assert_eq!(used_entry(&ram, &RING0, 1), (0, 1));
            assert!(!dev.has_pending_block_request());
        }
    }

    #[test]
    fn external_block_read_scatters_across_descriptors() {
        let (mut dev, mut ram) = external_block_device();
        let first_len = 173usize;
        submit_split_block(
            &mut dev,
            &mut ram,
            VIRTIO_BLK_T_IN,
            1,
            true,
            first_len as u32,
        );
        let request = dev.pending_block_request().expect("pending read");
        let data: Vec<u8> = (0..SECTOR).map(|index| (index % 251) as u8).collect();
        let mut jit = JitPageState::new(ram.len());

        assert!(dev.complete_block_request(request.id, &data, true, &mut ram, BASE, &mut jit,));
        assert_eq!(
            &ram[REQ + 0x100..REQ + 0x100 + first_len],
            &data[..first_len]
        );
        assert_eq!(
            &ram[REQ + 0x200..REQ + 0x200 + SECTOR - first_len],
            &data[first_len..]
        );
        assert_eq!(ram[REQ + 0x400], 0);
        assert_eq!(used_entry(&ram, &RING0, 1), (0, SECTOR as u32 + 1));
    }

    #[test]
    fn external_block_wrong_id_preserves_the_pending_request() {
        let (mut dev, mut ram) = external_block_device();
        ram[REQ + 0x100..REQ + 0x100 + SECTOR].fill(0xa5);
        submit_block(&mut dev, &mut ram, VIRTIO_BLK_T_IN, 1, true);
        let request = dev.pending_block_request().expect("pending read");
        let mut jit = JitPageState::new(ram.len());

        assert!(!dev.complete_block_request(
            request.id.wrapping_add(1),
            &[0x5a; SECTOR],
            true,
            &mut ram,
            BASE,
            &mut jit,
        ));
        assert_eq!(dev.pending_block_request(), Some(request.clone()));
        assert_eq!(&ram[REQ + 0x100..REQ + 0x100 + SECTOR], &[0xa5; SECTOR]);
        assert_eq!(u16_at(&ram, USED + 2), 0);

        assert!(dev.complete_block_request(
            request.id,
            &[0x5a; SECTOR],
            true,
            &mut ram,
            BASE,
            &mut jit,
        ));
        assert_eq!(u16_at(&ram, USED + 2), 1);
    }

    #[test]
    fn external_block_repeated_notify_preserves_the_pending_request() {
        let (mut dev, mut ram) = external_block_device();
        submit_block(&mut dev, &mut ram, VIRTIO_BLK_T_IN, 1, true);
        let request = dev.pending_block_request().expect("pending read");

        process(&mut dev, 0, &mut ram);

        assert_eq!(dev.pending_block_request(), Some(request.clone()));
        assert_eq!(u16_at(&ram, USED + 2), 0);
        let mut jit = JitPageState::new(ram.len());
        assert!(dev.complete_block_request(
            request.id,
            &[0x5a; SECTOR],
            true,
            &mut ram,
            BASE,
            &mut jit,
        ));
        assert_eq!(u16_at(&ram, USED + 2), 1);
        assert_eq!(used_entry(&ram, &RING0, 1), (0, SECTOR as u32 + 1));
    }

    #[test]
    fn external_block_reset_cancels_the_pending_request() {
        let (mut dev, mut ram) = external_block_device();
        submit_block(&mut dev, &mut ram, VIRTIO_BLK_T_IN, 1, true);
        let request = dev.pending_block_request().expect("pending read");
        dev.write(0x070, 0);

        assert!(!dev.has_pending_block_request());
        assert!(!dev.irq_pending());
        let mut jit = JitPageState::new(ram.len());
        assert!(!dev.complete_block_request(
            request.id,
            &[0x5a; SECTOR],
            true,
            &mut ram,
            BASE,
            &mut jit,
        ));
        assert_eq!(u16_at(&ram, USED + 2), 0);
    }

    #[test]
    fn external_block_write_and_flush_are_host_requests() {
        let (mut dev, mut ram) = external_block_device();
        ram[REQ + 0x100..REQ + 0x100 + SECTOR].fill(0xa5);
        submit_block(&mut dev, &mut ram, VIRTIO_BLK_T_OUT, 2, false);

        let write = dev.pending_block_request().expect("pending write");
        assert_eq!(write.kind, BlockRequestKind::Write);
        assert_eq!(write.offset, (2 * SECTOR) as u64);
        assert_eq!(write.data(), &[0xa5; SECTOR]);
        let mut jit = JitPageState::new(ram.len());
        assert!(dev.complete_block_request(write.id, &[], true, &mut ram, BASE, &mut jit,));
        assert_eq!(ram[REQ + 0x300], 0);

        ram[REQ..REQ + 4].copy_from_slice(&VIRTIO_BLK_T_FLUSH.to_le_bytes());
        ram[REQ + 8..REQ + 16].fill(0);
        put_desc_in(&mut ram, &RING0, 0, REQ, 16, 1, 2);
        put_desc_in(&mut ram, &RING0, 2, REQ + 0x300, 1, 2, 0);
        publish(&mut ram, &RING0, 0, 2);
        process(&mut dev, 0, &mut ram);

        let flush = dev.pending_block_request().expect("pending flush");
        assert_eq!(flush.kind, BlockRequestKind::Flush);
        assert!(flush.is_empty());
        assert!(dev.complete_block_request(flush.id, &[], true, &mut ram, BASE, &mut jit,));
        assert_eq!(u16_at(&ram, USED + 2), 2);
        assert_eq!(ram[REQ + 0x300], 0);
    }

    #[test]
    fn external_block_write_gathers_multiple_descriptors() {
        let (mut dev, mut ram) = external_block_device();
        let first_len = 173usize;
        ram[REQ + 0x100..REQ + 0x100 + first_len].fill(0x11);
        ram[REQ + 0x200..REQ + 0x200 + SECTOR - first_len].fill(0x22);
        submit_split_block(
            &mut dev,
            &mut ram,
            VIRTIO_BLK_T_OUT,
            2,
            false,
            first_len as u32,
        );

        let request = dev.pending_block_request().expect("pending write");
        assert_eq!(request.kind, BlockRequestKind::Write);
        assert_eq!(&request.data()[..first_len], &[0x11; 173]);
        assert_eq!(&request.data()[first_len..], &[0x22; SECTOR - 173]);
        let mut jit = JitPageState::new(ram.len());
        assert!(dev.complete_block_request(request.id, &[], true, &mut ram, BASE, &mut jit,));
        assert_eq!(ram[REQ + 0x400], 0);
        assert_eq!(used_entry(&ram, &RING0, 1), (0, 1));
    }

    #[test]
    fn external_block_rejects_invalid_flush_synchronously() {
        let (mut dev, mut ram) = external_block_device();
        submit_block(&mut dev, &mut ram, VIRTIO_BLK_T_FLUSH, 0, false);

        assert!(!dev.has_pending_block_request());
        assert_eq!(ram[REQ + 0x300], 1);
        assert_eq!(u16_at(&ram, USED + 2), 1);
        assert_eq!(used_entry(&ram, &RING0, 1), (0, 1));
    }

    #[test]
    fn external_block_flush_failure_reports_ioerr() {
        let (mut dev, mut ram) = external_block_device();
        setup_ring(&mut dev, 0, &RING0);
        ram[REQ..REQ + 4].copy_from_slice(&VIRTIO_BLK_T_FLUSH.to_le_bytes());
        put_desc_in(&mut ram, &RING0, 0, REQ, 16, 1, 1);
        put_desc_in(&mut ram, &RING0, 1, REQ + 0x300, 1, 2, 0);
        publish(&mut ram, &RING0, 0, 1);
        process(&mut dev, 0, &mut ram);

        let request = dev.pending_block_request().expect("pending flush");
        assert_eq!(request.kind, BlockRequestKind::Flush);
        let mut jit = JitPageState::new(ram.len());
        assert!(dev.complete_block_request(request.id, &[], false, &mut ram, BASE, &mut jit,));
        assert_eq!(ram[REQ + 0x300], 1);
        assert_eq!(u16_at(&ram, USED + 2), 1);
        assert_eq!(used_entry(&ram, &RING0, 1), (0, 1));
    }

    // ---- virtio-net ----

    const FRAME: usize = 0x9000; // guest TX frame buffer
    const RXBUF: usize = 0xa000; // guest RX buffer

    const TEST_MAC: [u8; 6] = [0x52, 0x54, 0x00, 0xab, 0xcd, 0xef];

    fn net_device() -> (VirtioDev, Vec<u8>) {
        let dev = VirtioDev::new(Backend::Net {
            mac: TEST_MAC,
            inbox: Vec::new(),
            outbox: Vec::new(),
        });
        (dev, vec![0u8; 64 * 1024])
    }

    #[test]
    fn advertises_a_net_device_and_its_mac() {
        let (mut dev, _) = net_device();
        assert_eq!(dev.read(0x008), 1); // virtio device id 1 = net
        dev.write(0x014, 0);
        assert_eq!(dev.read(0x010), 1 << 5, "VIRTIO_NET_F_MAC only");
        dev.write(0x014, 1);
        assert_eq!(dev.read(0x010), 1, "VIRTIO_F_VERSION_1");
        // The MAC is read byte-wise out of config space; without F_MAC (and a
        // readable MAC) the guest would invent a random address instead.
        let mac: Vec<u8> = (0..6).map(|i| dev.read_sized(0x100 + i, 1) as u8).collect();
        assert_eq!(&mac, &TEST_MAC);
        // status is only meaningful with VIRTIO_NET_F_STATUS, which we do not
        // offer — the guest treats the link as up unconditionally.
        assert_eq!(dev.read_sized(0x106, 2), 0);
        assert_eq!(dev.read(0x034), 256, "modern TX needs a non-tiny ring");
    }

    #[test]
    fn transmits_a_guest_frame_stripped_of_its_header() {
        let (mut dev, mut ram) = net_device();
        setup_ring(&mut dev, 1, &RING1);

        // The guest writes virtio_net_hdr_v1 then the Ethernet frame.
        let payload: Vec<u8> = (0..60u8).collect();
        ram[FRAME..FRAME + NET_HDR_LEN].fill(0);
        ram[FRAME + NET_HDR_LEN..FRAME + NET_HDR_LEN + payload.len()].copy_from_slice(&payload);
        put_desc_in(
            &mut ram,
            &RING1,
            0,
            FRAME,
            (NET_HDR_LEN + payload.len()) as u32,
            0,
            0,
        );
        publish(&mut ram, &RING1, 0, 1);

        assert_eq!(dev.write(0x050, 1), Some(1), "notify selects the TX queue");
        process(&mut dev, 1, &mut ram);

        assert_eq!(u16_at(&ram, RING1.used + 2), 1, "TX chain consumed");
        let out = dev.net_take_output();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0], payload, "header must be stripped, frame intact");
        assert!(dev.net_take_output().is_empty(), "drained once");
    }

    #[test]
    fn transmits_a_frame_split_across_descriptors() {
        let (mut dev, mut ram) = net_device();
        setup_ring(&mut dev, 1, &RING1);
        // Linux hands the header and the payload as separate sg entries.
        let payload: Vec<u8> = (0..100u8).map(|i| i ^ 0x5a).collect();
        ram[FRAME..FRAME + NET_HDR_LEN].fill(0);
        ram[RXBUF..RXBUF + payload.len()].copy_from_slice(&payload);
        put_desc_in(&mut ram, &RING1, 0, FRAME, NET_HDR_LEN as u32, 1, 1);
        put_desc_in(&mut ram, &RING1, 1, RXBUF, payload.len() as u32, 0, 0);
        publish(&mut ram, &RING1, 0, 1);
        process(&mut dev, 1, &mut ram);
        assert_eq!(dev.net_take_output()[0], payload);
    }

    #[test]
    fn receives_a_frame_into_a_guest_buffer() {
        let (mut dev, mut ram) = net_device();
        setup_ring(&mut dev, 0, &RING0);

        let frame: Vec<u8> = (0..74u8).map(|i| i.wrapping_mul(3)).collect();
        dev.net_input(&frame);
        assert!(dev.net_rx_pending());

        put_desc_in(&mut ram, &RING0, 0, RXBUF, 2048, 2 /* WRITE */, 0);
        publish(&mut ram, &RING0, 0, 1);
        process(&mut dev, 0, &mut ram);

        let (head, len) = used_entry(&ram, &RING0, 1);
        assert_eq!(head, 0);
        assert_eq!(len as usize, NET_HDR_LEN + frame.len());
        // num_buffers = 1 at offset 10. Linux sizes hdr_len from VERSION_1, so
        // a 10-byte header here would shift every frame by two bytes.
        assert_eq!(&ram[RXBUF..RXBUF + 10], &[0u8; 10]);
        assert_eq!(u16_at(&ram, RXBUF + 10), 1, "num_buffers");
        assert_eq!(
            &ram[RXBUF + NET_HDR_LEN..RXBUF + NET_HDR_LEN + frame.len()],
            &frame[..]
        );
        assert!(!dev.net_rx_pending(), "frame consumed");
        assert!(dev.irq_pending(), "RX must raise the interrupt");
    }

    #[test]
    fn dma_write_invalidates_a_compiled_guest_page() {
        let (mut dev, mut ram) = net_device();
        setup_ring(&mut dev, 0, &RING0);
        dev.net_input(&[1, 2, 3, 4]);
        put_desc_in(&mut ram, &RING0, 0, RXBUF, 2048, 2, 0);
        publish(&mut ram, &RING0, 0, 1);

        let mut jit = JitPageState::new(ram.len());
        let page = RXBUF as u64 >> 12;
        jit.mark_address(BASE + RXBUF as u64);
        dev.process(0, &mut ram, BASE, &mut jit);

        assert!(jit.is_dirty(page));
        assert_eq!(jit.page_generation(page), Some(1));
        assert_eq!(jit.take_dirty(), vec![page]);
    }

    #[test]
    fn dma_rejects_addresses_that_alias_low_ram_on_wasm32() {
        let mut ram = vec![0u8; 64];
        let mut jit = JitPageState::new(ram.len());
        jit.mark_address(BASE + 4);

        assert!(!guest_write(
            &mut ram,
            BASE,
            BASE + (1u64 << 32) + 4,
            &[1, 2, 3, 4],
            &mut jit,
        ));
        assert_eq!(&ram[4..8], &[0, 0, 0, 0]);
        assert!(!jit.has_dirty());
    }

    #[test]
    fn an_rx_buffer_with_no_frame_is_left_on_the_ring() {
        let (mut dev, mut ram) = net_device();
        setup_ring(&mut dev, 0, &RING0);
        put_desc_in(&mut ram, &RING0, 0, RXBUF, 2048, 2, 0);
        publish(&mut ram, &RING0, 0, 1);
        process(&mut dev, 0, &mut ram);
        // Nothing to deliver: the buffer stays available for the next frame
        // rather than being consumed empty.
        assert_eq!(u16_at(&ram, RING0.used + 2), 0);
        assert!(!dev.irq_pending());
        // It is still there when a frame turns up.
        dev.net_input(b"late frame");
        process(&mut dev, 0, &mut ram);
        assert_eq!(u16_at(&ram, RING0.used + 2), 1);
    }

    #[test]
    fn an_undersized_rx_buffer_drops_the_frame_instead_of_wedging() {
        let (mut dev, mut ram) = net_device();
        setup_ring(&mut dev, 0, &RING0);
        dev.net_input(&vec![7u8; 1500]);
        put_desc_in(&mut ram, &RING0, 0, RXBUF, 64, 2, 0);
        publish(&mut ram, &RING0, 0, 1);
        process(&mut dev, 0, &mut ram);
        // Keeping an unsendable frame at the head would stall the queue forever.
        assert!(!dev.net_rx_pending(), "oversized frame dropped");
        assert_eq!(u16_at(&ram, RING0.used + 2), 0, "buffer not consumed");
    }

    #[test]
    fn mailboxes_are_bounded() {
        let (mut dev, mut ram) = net_device();
        setup_ring(&mut dev, 1, &RING1);
        // A guest that never posts RX buffers must not grow host memory.
        for _ in 0..NET_QUEUE_LIMIT + 50 {
            dev.net_input(b"flood");
        }
        let Backend::Net { inbox, .. } = &dev.backend else {
            unreachable!()
        };
        assert_eq!(inbox.len(), NET_QUEUE_LIMIT);
        // Nor must a host that never collects.
        ram[FRAME..FRAME + NET_HDR_LEN + 4].fill(1);
        for n in 1..=(NET_QUEUE_LIMIT as u16 + 50) {
            put_desc_in(&mut ram, &RING1, 0, FRAME, (NET_HDR_LEN + 4) as u32, 0, 0);
            publish(&mut ram, &RING1, 0, n);
            process(&mut dev, 1, &mut ram);
        }
        assert_eq!(dev.net_take_output().len(), NET_QUEUE_LIMIT);
    }

    #[test]
    fn an_over_long_frame_is_refused() {
        let (mut dev, _) = net_device();
        dev.net_input(&vec![0u8; NET_MAX_FRAME + 1]);
        assert!(!dev.net_rx_pending(), "frame larger than NET_MAX_FRAME");
        dev.net_input(&vec![0u8; NET_MAX_FRAME]);
        assert!(dev.net_rx_pending(), "a frame at the limit is accepted");
    }
}
