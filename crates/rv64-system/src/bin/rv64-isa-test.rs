//! rv64-isa-test: run official riscv-tests ISA binaries against the machine.
//!
//! Usage: rv64-isa-test <test-elf>...
//! Each test is a bare-metal ELF (entry 0x80000000) that reports its result
//! by writing to the `tohost` symbol: 1 = pass, (n<<1)|1 = case n failed.

use rv64_core::{Cpu, FlatMemory};
use rv64_system::{checked_ram_range, RAM_BASE};

const ELF_HEADER_SIZE: usize = 64;
const PROGRAM_HEADER_SIZE: usize = 56;
const SECTION_HEADER_SIZE: usize = 64;
const SYMBOL_SIZE: usize = 24;
const PT_LOAD: u32 = 1;
const SHT_SYMTAB: u32 = 2;
const SHT_STRTAB: u32 = 3;

#[derive(Clone, Copy)]
struct Table {
    offset: usize,
    entry_size: usize,
    entry_count: usize,
}

struct Section {
    kind: u32,
    offset: u64,
    size: u64,
    link: u32,
    entry_size: u64,
}

struct Elf64<'a> {
    bytes: &'a [u8],
    entry: u64,
    programs: Table,
    sections: Table,
}

impl<'a> Elf64<'a> {
    fn parse(bytes: &'a [u8]) -> Result<Self, String> {
        if bytes.len() < ELF_HEADER_SIZE {
            return Err("ELF header is truncated".into());
        }
        if bytes.get(..4) != Some(b"\x7fELF") {
            return Err("not an ELF file".into());
        }
        if bytes[4] != 2 || bytes[5] != 1 {
            return Err("ELF must use the 64-bit little-endian format".into());
        }

        let programs = Self::table(
            bytes,
            read_u64(bytes, 32)?,
            read_u16(bytes, 54)?,
            read_u16(bytes, 56)?,
            PROGRAM_HEADER_SIZE,
            "program header table",
        )?;
        let sections = Self::table(
            bytes,
            read_u64(bytes, 40)?,
            read_u16(bytes, 58)?,
            read_u16(bytes, 60)?,
            SECTION_HEADER_SIZE,
            "section header table",
        )?;
        Ok(Self {
            bytes,
            entry: read_u64(bytes, 24)?,
            programs,
            sections,
        })
    }

    fn table(
        bytes: &[u8],
        offset: u64,
        entry_size: u16,
        entry_count: u16,
        minimum_entry_size: usize,
        name: &str,
    ) -> Result<Table, String> {
        let entry_size = usize::from(entry_size);
        let entry_count = usize::from(entry_count);
        if entry_count != 0 && entry_size < minimum_entry_size {
            return Err(format!("{name} entries are too small"));
        }
        let offset = usize::try_from(offset).map_err(|_| format!("{name} offset is too large"))?;
        let size = entry_size
            .checked_mul(entry_count)
            .ok_or_else(|| format!("{name} size overflows"))?;
        checked_file_range(bytes.len(), offset, size, name)?;
        Ok(Table {
            offset,
            entry_size,
            entry_count,
        })
    }

    fn table_entry(&self, table: Table, index: usize, name: &str) -> Result<usize, String> {
        if index >= table.entry_count {
            return Err(format!("{name} index is out of range"));
        }
        table
            .offset
            .checked_add(
                table
                    .entry_size
                    .checked_mul(index)
                    .ok_or_else(|| format!("{name} offset overflows"))?,
            )
            .ok_or_else(|| format!("{name} offset overflows"))
    }

    fn section(&self, index: usize) -> Result<Section, String> {
        let offset = self.table_entry(self.sections, index, "section")?;
        Ok(Section {
            kind: read_u32(self.bytes, offset + 4)?,
            offset: read_u64(self.bytes, offset + 24)?,
            size: read_u64(self.bytes, offset + 32)?,
            link: read_u32(self.bytes, offset + 40)?,
            entry_size: read_u64(self.bytes, offset + 56)?,
        })
    }

    fn section_bytes(&self, section: &Section, name: &str) -> Result<&'a [u8], String> {
        let offset =
            usize::try_from(section.offset).map_err(|_| format!("{name} offset is too large"))?;
        let size =
            usize::try_from(section.size).map_err(|_| format!("{name} size is too large"))?;
        let range = checked_file_range(self.bytes.len(), offset, size, name)?;
        Ok(&self.bytes[range])
    }

    fn load_segments(&self, ram: &mut [u8]) -> Result<(), String> {
        for index in 0..self.programs.entry_count {
            let header = self.table_entry(self.programs, index, "program header")?;
            if read_u32(self.bytes, header)? != PT_LOAD {
                continue;
            }
            let file_offset = read_u64(self.bytes, header + 8)?;
            let physical_address = read_u64(self.bytes, header + 24)?;
            let file_size = read_u64(self.bytes, header + 32)?;
            let memory_size = read_u64(self.bytes, header + 40)?;
            if file_size > memory_size {
                return Err(format!(
                    "load segment {index} has a file size larger than its memory size"
                ));
            }

            let source_offset = usize::try_from(file_offset)
                .map_err(|_| format!("load segment {index} file offset is too large"))?;
            let source_size = usize::try_from(file_size)
                .map_err(|_| format!("load segment {index} file size is too large"))?;
            let source =
                checked_file_range(self.bytes.len(), source_offset, source_size, "load segment")?;
            let destination_size = usize::try_from(memory_size)
                .map_err(|_| format!("load segment {index} memory size is too large"))?;
            let destination =
                checked_ram_range(ram.len(), RAM_BASE, physical_address, destination_size)
                    .ok_or_else(|| format!("load segment {index} lies outside guest RAM"))?;
            let file_end = destination
                .start
                .checked_add(source_size)
                .ok_or_else(|| format!("load segment {index} destination overflows"))?;
            ram[destination.start..file_end].copy_from_slice(&self.bytes[source]);
            ram[file_end..destination.end].fill(0);
        }
        Ok(())
    }

    fn find_symbol(&self, wanted: &str) -> Result<Option<u64>, String> {
        for section_index in 0..self.sections.entry_count {
            let symbols = self.section(section_index)?;
            if symbols.kind != SHT_SYMTAB {
                continue;
            }
            let entry_size = usize::try_from(symbols.entry_size)
                .map_err(|_| "symbol table entry size is too large".to_string())?;
            if entry_size < SYMBOL_SIZE
                || symbols.entry_size == 0
                || symbols.size % symbols.entry_size != 0
            {
                return Err("symbol table has an invalid entry size".into());
            }
            let strings = self.section(
                usize::try_from(symbols.link)
                    .map_err(|_| "symbol string table index is too large".to_string())?,
            )?;
            if strings.kind != SHT_STRTAB {
                return Err("symbol table does not link to a string table".into());
            }
            let symbol_bytes = self.section_bytes(&symbols, "symbol table")?;
            let string_bytes = self.section_bytes(&strings, "symbol string table")?;
            for symbol in symbol_bytes.chunks_exact(entry_size) {
                let name_offset = usize::try_from(read_u32(symbol, 0)?)
                    .map_err(|_| "symbol name offset is too large".to_string())?;
                let Some(name_bytes) = string_bytes.get(name_offset..) else {
                    return Err("symbol name lies outside its string table".into());
                };
                let name_end = name_bytes
                    .iter()
                    .position(|&byte| byte == 0)
                    .ok_or_else(|| "symbol name is not terminated".to_string())?;
                if &name_bytes[..name_end] == wanted.as_bytes() {
                    return Ok(Some(read_u64(symbol, 8)?));
                }
            }
        }
        Ok(None)
    }
}

fn checked_file_range(
    file_size: usize,
    offset: usize,
    size: usize,
    name: &str,
) -> Result<core::ops::Range<usize>, String> {
    let end = offset
        .checked_add(size)
        .ok_or_else(|| format!("{name} range overflows"))?;
    if end > file_size {
        return Err(format!("{name} is truncated"));
    }
    Ok(offset..end)
}

fn read_array<const N: usize>(bytes: &[u8], offset: usize) -> Result<[u8; N], String> {
    let range = checked_file_range(bytes.len(), offset, N, "ELF field")?;
    bytes[range]
        .try_into()
        .map_err(|_| "ELF field has the wrong size".into())
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, String> {
    Ok(u16::from_le_bytes(read_array(bytes, offset)?))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, String> {
    Ok(u32::from_le_bytes(read_array(bytes, offset)?))
}

fn read_u64(bytes: &[u8], offset: usize) -> Result<u64, String> {
    Ok(u64::from_le_bytes(read_array(bytes, offset)?))
}

#[derive(Clone, Copy)]
struct TestImage {
    entry: u64,
    tohost: u64,
    signature: Option<(u64, u64)>,
}

/// Load PT_LOAD segments into physical RAM and resolve the test result slot.
fn load_test(elf: &[u8], ram: &mut [u8]) -> Result<TestImage, String> {
    let elf = Elf64::parse(elf)?;
    elf.load_segments(ram)?;
    let tohost = elf
        .find_symbol("tohost")?
        .ok_or_else(|| "ELF does not define tohost".to_string())?;
    if checked_ram_range(ram.len(), RAM_BASE, elf.entry, 1).is_none() {
        return Err("entry point lies outside guest RAM".into());
    }
    if checked_ram_range(ram.len(), RAM_BASE, tohost, 8).is_none() {
        return Err("tohost lies outside guest RAM".into());
    }
    let signature = match (
        elf.find_symbol("begin_signature")?,
        elf.find_symbol("end_signature")?,
    ) {
        (Some(begin), Some(end)) => Some((begin, end)),
        _ => None,
    };
    Ok(TestImage {
        entry: elf.entry,
        tohost,
        signature,
    })
}

/// If the instruction at `pc` architecturally writes a non-zero x-register,
/// return that register (best effort: physical read, satp=0 assumption —
/// fine for the bare-metal p-variant tests lockstep runs on).
fn insn_x_dest(m: &IsaMachine, pc: u64) -> Option<usize> {
    let off = usize::try_from(pc.checked_sub(RAM_BASE)?).ok()?;
    let bytes = m.ram.get(off..off.checked_add(4)?)?;
    let lo = u16::from_le_bytes(bytes[..2].try_into().unwrap()) as u32;
    let insn = if lo & 3 == 3 {
        lo | ((u16::from_le_bytes(bytes[2..].try_into().unwrap()) as u32) << 16)
    } else {
        rv64_core::compressed::expand(lo as u16)?
    };
    use rv64_core::decode::{funct3, funct7, opcode, rd};
    let writes = match opcode(insn) {
        0x37 | 0x17 | 0x6f | 0x67 | 0x03 | 0x13 | 0x33 | 0x1b | 0x3b | 0x2f => true,
        0x73 => funct3(insn) != 0 && funct3(insn) != 4, // CSR ops
        // OP-FP forms whose destination is an x-register:
        // fcmp (0x50/0x51), fcvt.int.fmt (0x60/0x61), fmv.x/fclass (0x70/0x71)
        0x53 => matches!(funct7(insn), 0x50 | 0x51 | 0x60 | 0x61 | 0x70 | 0x71),
        _ => false,
    };
    (writes && rd(insn) != 0).then(|| rd(insn))
}

/// Minimal machine for privileged ISA conformance programs.
///
/// These programs need a hart, flat physical RAM, and the conventional
/// `tohost` result word. They do not need Linux devices or a product board.
struct IsaMachine {
    cpu: Cpu,
    ram: Vec<u8>,
    tohost: u64,
    result: Option<u64>,
}

impl IsaMachine {
    fn new(ram: Vec<u8>, image: TestImage) -> Self {
        let mut cpu = Cpu::new();
        cpu.enable_system(0);
        cpu.pc = image.entry;
        Self {
            cpu,
            ram,
            tohost: image.tohost,
            result: None,
        }
    }

    fn run_slice(&mut self, budget: u64) {
        let mut bus = FlatMemory::new(RAM_BASE, &mut self.ram);
        self.cpu.run(&mut bus, budget);
        self.capture_result();
    }

    fn capture_result(&mut self) {
        let range = checked_ram_range(self.ram.len(), RAM_BASE, self.tohost, 8)
            .expect("validated tohost address");
        let bytes = &self.ram[range];
        let value = u64::from_le_bytes(bytes.try_into().expect("eight-byte tohost word"));
        if value & 1 != 0 {
            self.result = Some(value >> 1);
        }
    }
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // --signature FILE : dump begin_signature..end_signature after the run
    //                    (RISCOF DUT protocol; single test only)
    // --trace FILE     : per-instruction commit log of x-register writes,
    //                    normalizable against `spike --log-commits`
    let mut sig_path = None;
    let mut trace_path = None;
    while args.len() >= 2 {
        match args[0].as_str() {
            "--signature" => {
                sig_path = Some(args[1].clone());
                args.drain(..2);
            }
            "--trace" => {
                trace_path = Some(args[1].clone());
                args.drain(..2);
            }
            _ => break,
        }
    }
    if args.is_empty() {
        eprintln!("usage: rv64-isa-test [--signature FILE] [--trace FILE] <test-elf>...");
        std::process::exit(2);
    }

    let (mut passed, mut failed) = (0u32, 0u32);
    for path in &args {
        let elf = std::fs::read(path).expect("read test");
        let name = path.rsplit('/').next().unwrap();

        let mut ram = vec![0; 64 << 20];
        let image = match load_test(&elf, &mut ram) {
            Ok(image) => image,
            Err(e) => {
                println!("ERROR {name}: {e}");
                failed += 1;
                continue;
            }
        };
        let signature = image.signature;
        let mut m = IsaMachine::new(ram, image);

        let mut result = None;
        if let Some(tp) = &trace_path {
            // Lockstep trace: single-step; for every instruction that
            // architecturally writes an x-register, emit "pc reg value" —
            // the same event stream `spike --log-commits` produces.
            use std::io::Write;
            let mut w = std::io::BufWriter::new(std::fs::File::create(tp).expect("trace file"));
            for _ in 0..60_000_000u64 {
                let pc = m.cpu.pc;
                let dest = insn_x_dest(&m, pc);
                let excs_before: u64 = m.cpu.exc_counts.iter().sum();
                m.run_slice(1);
                let trapped = m.cpu.exc_counts.iter().sum::<u64>() != excs_before;
                if let Some(rd) = dest {
                    if !trapped {
                        writeln!(w, "{pc:#x} x{rd} {:#x}", m.cpu.x[rd]).unwrap();
                    }
                }
                if m.result.is_some() {
                    result = m.result;
                    break;
                }
            }
        } else {
            for _ in 0..200 {
                m.run_slice(1_000_000);
                if m.result.is_some() {
                    result = m.result;
                    break;
                }
            }
        }

        // RISCOF signature dump (4 bytes per line, lowercase hex).
        if let Some(sp) = &sig_path {
            use std::io::Write;
            if let Some((begin, end)) = signature {
                let mut w =
                    std::io::BufWriter::new(std::fs::File::create(sp).expect("signature file"));
                let Some(range) = end
                    .checked_sub(begin)
                    .and_then(|size| usize::try_from(size).ok())
                    .and_then(|size| checked_ram_range(m.ram.len(), RAM_BASE, begin, size))
                    .filter(|range| range.len().is_multiple_of(4))
                else {
                    eprintln!("ERROR {name}: signature range is invalid");
                    failed += 1;
                    continue;
                };
                for word in m.ram[range].chunks_exact(4) {
                    let word = u32::from_le_bytes(word.try_into().unwrap());
                    writeln!(w, "{word:08x}").unwrap();
                }
            } else {
                eprintln!("warning: no begin/end_signature symbols in {name}");
            }
        }
        match result {
            Some(0) => {
                passed += 1;
                println!("PASS {name}");
            }
            Some(n) => {
                failed += 1;
                println!("FAIL {name} (test case {n}) pc={:#x}", m.cpu.pc);
            }
            None => {
                failed += 1;
                println!(
                    "TIMEOUT {name} pc={:#x} insns={}",
                    m.cpu.pc, m.cpu.insn_count
                );
            }
        }
    }
    println!("--- {passed} passed, {failed} failed");
    std::process::exit(if failed == 0 { 0 } else { 1 });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_u16(bytes: &mut [u8], offset: usize, value: u16) {
        bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
        bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn write_u64(bytes: &mut [u8], offset: usize, value: u64) {
        bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn test_elf() -> Vec<u8> {
        const PROGRAMS: usize = 0x40;
        const SECTIONS: usize = 0x100;
        const SEGMENT: usize = 0x300;
        const SYMBOLS: usize = 0x320;
        const STRINGS: usize = 0x350;

        let mut elf = vec![0; 0x380];
        elf[..6].copy_from_slice(b"\x7fELF\x02\x01");
        write_u64(&mut elf, 24, RAM_BASE);
        write_u64(&mut elf, 32, PROGRAMS as u64);
        write_u64(&mut elf, 40, SECTIONS as u64);
        write_u16(&mut elf, 54, PROGRAM_HEADER_SIZE as u16);
        write_u16(&mut elf, 56, 1);
        write_u16(&mut elf, 58, SECTION_HEADER_SIZE as u16);
        write_u16(&mut elf, 60, 3);

        write_u32(&mut elf, PROGRAMS, PT_LOAD);
        write_u64(&mut elf, PROGRAMS + 8, SEGMENT as u64);
        write_u64(&mut elf, PROGRAMS + 24, RAM_BASE);
        write_u64(&mut elf, PROGRAMS + 32, 4);
        write_u64(&mut elf, PROGRAMS + 40, 16);
        elf[SEGMENT..SEGMENT + 4].copy_from_slice(&[1, 2, 3, 4]);

        let symtab = SECTIONS + SECTION_HEADER_SIZE;
        write_u32(&mut elf, symtab + 4, SHT_SYMTAB);
        write_u64(&mut elf, symtab + 24, SYMBOLS as u64);
        write_u64(&mut elf, symtab + 32, (2 * SYMBOL_SIZE) as u64);
        write_u32(&mut elf, symtab + 40, 2);
        write_u64(&mut elf, symtab + 56, SYMBOL_SIZE as u64);

        let strtab = SECTIONS + 2 * SECTION_HEADER_SIZE;
        write_u32(&mut elf, strtab + 4, SHT_STRTAB);
        write_u64(&mut elf, strtab + 24, STRINGS as u64);
        write_u64(&mut elf, strtab + 32, 8);
        elf[STRINGS..STRINGS + 8].copy_from_slice(b"\0tohost\0");

        let tohost = SYMBOLS + SYMBOL_SIZE;
        write_u32(&mut elf, tohost, 1);
        write_u64(&mut elf, tohost + 8, RAM_BASE + 8);
        elf
    }

    #[test]
    fn loader_uses_symbol_table_link_and_zero_fills_segment_tail() {
        let elf = test_elf();
        let mut ram = vec![0xff; 64];
        let image = load_test(&elf, &mut ram).expect("valid test ELF");

        assert_eq!(image.entry, RAM_BASE);
        assert_eq!(image.tohost, RAM_BASE + 8);
        assert_eq!(&ram[..4], &[1, 2, 3, 4]);
        assert_eq!(&ram[4..16], &[0; 12]);
    }

    #[test]
    fn loader_rejects_truncated_input_without_panicking() {
        let mut ram = vec![0; 64];
        assert!(load_test(&[], &mut ram).is_err());

        let mut elf = test_elf();
        elf.truncate(0x302);
        assert!(load_test(&elf, &mut ram).is_err());
    }
}
