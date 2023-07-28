pub mod cpu;
pub mod debug_info;
pub mod elf;
pub mod exec;
pub mod lifter;
pub mod utils;

mod config;
mod exit;
mod regs;
mod trace;

use std::any::Any;

use hashbrown::{HashMap, HashSet};

use crate::debug_info::{DebugInfo, SourceLocation};

pub use crate::{
    config::Config,
    cpu::{Arch, Cpu, CpuSnapshot, Exception, RegHandler, ShadowStack, ShadowStackEntry},
    exit::VmExit,
    lifter::BlockGroup,
    regs::{RegValue, Regs, ValueSource, VarSource},
    trace::{HookHandler, HookTrampoline, InstHook, StoreRef, TraceStore},
};
pub use icicle_mem as mem;
pub use icicle_mem::Mmu;
pub use lifter::DecodeError;

#[derive(Copy, Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct BlockKey {
    pub vaddr: u64,
    pub isa_mode: u64,
}

/// Keeps track of all the code in the program that the emulator has discovered.
#[derive(Default)]
pub struct BlockTable {
    pub map: HashMap<BlockKey, BlockGroup>,
    pub blocks: Vec<lifter::Block>,
    pub disasm: HashMap<u64, String>,
    pub breakpoints: HashSet<u64>,
    pub modified: HashSet<usize>,
}

impl BlockTable {
    pub fn flush_code(&mut self) {
        self.map.clear();
        self.blocks.clear();
        self.disasm.clear();
        self.modified.clear();
    }

    pub fn get_info(&self, key: BlockKey) -> Option<BlockInfoRef> {
        let group = *self.map.get(&key)?;
        Some(BlockInfoRef { group, code: self })
    }

    pub fn address_of(&self, id: u64, offset: u64) -> u64 {
        let block = &self.blocks[id as usize];
        block
            .pcode
            .instructions
            .iter()
            .take(offset as usize)
            .filter(|inst| matches!(inst.op, pcode::Op::InstructionMarker))
            .map(|x| x.inputs.first().as_u64())
            .last()
            .unwrap_or(0)
    }
}

pub struct BlockInfoRef<'a> {
    group: BlockGroup,
    code: &'a BlockTable,
}

impl<'a> BlockInfoRef<'a> {
    /// Return an iterator over all (instruction start, instruction len) pairs in the group.
    pub fn instructions(&self) -> impl Iterator<Item = (u64, u64)> + 'a {
        self.code.blocks[self.group.range()].iter().flat_map(|block| block.instructions())
    }

    /// Return the entry block
    pub fn entry_block(&self) -> &'a lifter::Block {
        &self.code.blocks[self.group.blocks.0]
    }

    /// Return an iterator over all blocks in the group
    pub fn blocks(&self) -> impl Iterator<Item = (usize, &'a lifter::Block)> + 'a {
        self.group.range().map(|i| (i, &self.code.blocks[i]))
    }
}

pub trait Environment {
    /// Loads the target into the enviroment.
    fn load(&mut self, cpu: &mut Cpu, path: &[u8]) -> Result<(), String>;

    /// Called whenever an exception is generated by the CPU.
    fn handle_exception(&mut self, cpu: &mut Cpu) -> Option<VmExit>;

    /// Returns the next time the environment wants to interrupt the CPU.
    fn next_timer(&self) -> u64 {
        u64::MAX
    }

    /// Get a direct reference to the debug info loaded for the current environment
    fn debug_info(&self) -> Option<&DebugInfo> {
        None
    }

    /// Obtains debug information about the target address.
    fn symbolize_addr(&mut self, _cpu: &mut Cpu, _addr: u64) -> Option<SourceLocation> {
        None
    }

    /// Looks up symbol in the environment
    fn lookup_symbol(&mut self, _symbol: &str) -> Option<u64> {
        None
    }

    /// Gets the address of the program entrypoint.
    // @note: currently only used for debugging, currently `load` is expected to configure the cpu
    // with the entrypoint.
    fn entry_point(&mut self) -> u64 {
        0
    }

    /// Creates a snapshot of the current state of the environment which can be restored later.
    fn snapshot(&mut self) -> Box<dyn Any>;

    /// Restores the environment to the state of the snapshot.
    fn restore(&mut self, snapshot: &Box<dyn Any>);
}

pub trait EnvironmentAny: Environment {
    fn as_any(&self) -> &dyn Any;
    fn as_mut_any(&mut self) -> &mut dyn Any;
}

impl<E: Environment + 'static> EnvironmentAny for E {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_mut_any(&mut self) -> &mut dyn Any {
        self
    }
}

impl Environment for () {
    fn load(&mut self, _: &mut Cpu, _: &[u8]) -> Result<(), String> {
        Err("No environment loaded".into())
    }
    fn handle_exception(&mut self, _: &mut Cpu) -> Option<VmExit> {
        None
    }
    fn snapshot(&mut self) -> Box<dyn Any> {
        Box::new(())
    }
    fn restore(&mut self, _: &Box<dyn Any>) {}
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[repr(u32)]
pub enum ExceptionCode {
    None = 0x0000,

    InstructionLimit = 0x0001,
    Halt = 0x0002,
    Sleep = 0x0003,

    Syscall = 0x0101,
    CpuStateChanged = 0x0102,
    DivideByZero = 0x0103,

    ReadUnmapped = 0x0201,
    ReadPerm = 0x0202,
    ReadUnaligned = 0x0203,
    ReadWatch = 0x0204,
    ReadUninitialized = 0x0205,

    WriteUnmapped = 0x0301,
    WritePerm = 0x0302,
    WriteWatch = 0x0303,
    WriteUnaligned = 0x0304,

    ExecViolation = 0x0401,
    SelfModifyingCode = 0x0402,
    ExecUnaligned = 0x0404,
    OutOfMemory = 0x0501,
    AddressOverflow = 0x0502,

    InvalidInstruction = 0x1001,
    UnknownInterrupt = 0x1002,
    UnknownCpuID = 0x1003,
    InvalidOpSize = 0x1004,
    InvalidFloatSize = 0x1005,
    CodeNotTranslated = 0x1006,
    ShadowStackOverflow = 0x1007,
    ShadowStackInvalid = 0x1008,
    InvalidTarget = 0x1009,
    UnimplementedOp = 0x100a,

    ExternalAddr = 0x2001,
    Environment = 0x2002,

    JitError = 0x3001,
    InternalError = 0x3002,

    UnknownError,
}

impl ExceptionCode {
    #[inline]
    pub fn from_u32(code: u32) -> Self {
        match code {
            0x0000 => Self::None,
            0x0001 => Self::InstructionLimit,
            0x0002 => Self::Halt,
            0x0003 => Self::Sleep,

            0x0101 => Self::Syscall,
            0x0102 => Self::CpuStateChanged,
            0x0103 => Self::DivideByZero,

            0x0201 => Self::ReadUnmapped,
            0x0202 => Self::ReadPerm,
            0x0203 => Self::ReadUnaligned,
            0x0204 => Self::ReadWatch,
            0x0205 => Self::ReadUninitialized,

            0x0301 => Self::WriteUnmapped,
            0x0302 => Self::WritePerm,
            0x0303 => Self::WriteWatch,
            0x0304 => Self::WriteUnaligned,

            0x0401 => Self::ExecViolation,
            0x0402 => Self::SelfModifyingCode,
            0x0501 => Self::OutOfMemory,
            0x0502 => Self::AddressOverflow,

            0x1001 => Self::InvalidInstruction,
            0x1002 => Self::UnknownInterrupt,
            0x1003 => Self::UnknownCpuID,
            0x1004 => Self::InvalidOpSize,
            0x1005 => Self::InvalidFloatSize,
            0x1006 => Self::CodeNotTranslated,
            0x1007 => Self::ShadowStackOverflow,
            0x1008 => Self::ShadowStackInvalid,
            0x1009 => Self::InvalidTarget,
            0x100a => Self::UnimplementedOp,

            0x2001 => Self::ExternalAddr,
            0x2002 => Self::Environment,

            0x3001 => Self::JitError,
            0x3002 => Self::InternalError,

            _ => {
                if cfg!(debug_assertions) {
                    panic!("Unknown exception code: {:#0x}", code);
                }
                Self::UnknownError
            }
        }
    }

    pub fn is_running(&self) -> bool {
        matches!(self, Self::None | Self::InstructionLimit)
    }

    pub fn is_memory_error(&self) -> bool {
        matches!(
            self,
            Self::ReadUnmapped
                | Self::ReadPerm
                | Self::ReadUnaligned
                | Self::ReadWatch
                | Self::ReadUninitialized
                | Self::WriteUnmapped
                | Self::WritePerm
                | Self::WriteWatch
                | Self::WriteUnaligned
                | Self::SelfModifyingCode
        )
    }

    pub fn from_load_error(err: icicle_mem::MemError) -> Self {
        use icicle_mem::MemError;
        match err {
            MemError::Unmapped => Self::ReadUnmapped,
            MemError::Uninitalized => Self::ReadUninitialized,
            MemError::ReadViolation => Self::ReadPerm,
            MemError::Unaligned => Self::ReadUnaligned,
            MemError::ReadWatch => Self::ReadWatch,
            _ => Self::from(err),
        }
    }

    pub fn from_store_error(err: icicle_mem::MemError) -> Self {
        use icicle_mem::MemError;
        match err {
            MemError::Unmapped => Self::WriteUnmapped,
            MemError::WriteViolation => Self::WritePerm,
            MemError::Unaligned => Self::WriteUnaligned,
            MemError::WriteWatch => Self::WriteWatch,
            _ => Self::from(err),
        }
    }
}

impl From<icicle_mem::MemError> for ExceptionCode {
    fn from(err: icicle_mem::MemError) -> Self {
        use icicle_mem::MemError;
        match err {
            MemError::Unmapped => Self::ReadUnmapped,
            MemError::Uninitalized => Self::ReadUninitialized,
            MemError::ReadViolation => Self::ReadPerm,
            MemError::WriteViolation => Self::WritePerm,
            MemError::ExecViolation => Self::ExecViolation,
            MemError::ReadWatch => Self::ReadWatch,
            MemError::WriteWatch => Self::WriteWatch,
            MemError::Unaligned => Self::ReadUnaligned,
            MemError::OutOfMemory => Self::OutOfMemory,
            MemError::SelfModifyingCode => Self::SelfModifyingCode,
            MemError::AddressOverflow => Self::AddressOverflow,

            // These are errors that should be handled by the memory subsystem.
            MemError::Unallocated | MemError::Unknown => Self::UnknownError,
        }
    }
}

impl From<DecodeError> for ExceptionCode {
    fn from(err: DecodeError) -> Self {
        match err {
            DecodeError::InvalidInstruction => ExceptionCode::InvalidInstruction,
            DecodeError::NonExecutableMemory => ExceptionCode::ExecViolation,
            DecodeError::BadAlignment => ExceptionCode::ExecUnaligned,
            DecodeError::DisassemblyChanged => ExceptionCode::SelfModifyingCode,
            DecodeError::OptimizationError => ExceptionCode::UnknownError,
        }
    }
}

#[derive(Copy, Clone)]
#[repr(u64)]
pub enum InternalError {
    CorruptedBlockMap,
    UnsupportedIsaMode,
}

pub fn read_value_zxt(cpu: &mut Cpu, value: pcode::Value) -> u64 {
    cpu.read_dynamic(value).zxt()
}

pub fn read_value_sxt(cpu: &mut Cpu, value: pcode::Value) -> i64 {
    cpu.read_dynamic(value).sxt()
}
