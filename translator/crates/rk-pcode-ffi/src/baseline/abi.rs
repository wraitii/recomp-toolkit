pub use ffi::*;

#[cxx::bridge(namespace = "rp::baseline")]
mod ffi {
    #[derive(Default)]
    struct Frame {
        index: u32,
        target: u64,
    }

    #[repr(i32)]
    #[derive(Debug, PartialOrd, Ord)]
    enum Status {
        Fallthrough,
        Branch,
        Call,
        Return,
        HelperFailed,
        WideAddress,
        NullAccess,
        OutsideArena,
        UndefinedUnique,
        BudgetExhausted,
        DivideByZero,
        DivideOverflow,
    }

    #[repr(u32)]
    #[derive(Debug)]
    enum FloatOp {
        Add,
        Sub,
        Mul,
        Div,
        Min,
        Max,
        Sqrt,
        Rcp,
        Rsqrt,
        Equal,
        NotEqual,
        Less,
        LessEqual,
        Nan,
    }

    #[repr(i32)]
    enum CompileResult {
        Error = -1,
        Rejected = 0,
        Compiled = 1,
    }

    #[repr(u8)]
    #[derive(Debug)]
    enum StringFamily {
        Movs,
        Stos,
        Lods,
        Cmps,
        Scas,
    }

    #[repr(u8)]
    #[derive(Debug)]
    enum Repeat {
        Once,
        WhileEqual,
        WhileNotEqual,
    }

    #[repr(u8)]
    #[derive(Debug)]
    enum X87Kind {
        Push = 1,
        PushInteger,
        Store,
        Arithmetic,
        Compare,
        CompareFlags,
        Exchange,
        Free,
        Status,
        Init,
        Rotate,
        Nop,
        Unary,
        Constant,
        StoreStatus,
        Examine,
        Remainder,
        Yl2x,
        Scale,
        Tangent,
    }

    #[repr(u8)]
    enum X87Operand {
        Memory = 8,
    }

    #[derive(Clone, Copy, Debug)]
    struct X87Call {
        kind: X87Kind,
        operand: u8,
        bits: u8,
        flags: u8,
        extra: u8,
    }

    #[derive(Clone, Copy)]
    struct CpuLayout {
        gpr: u32,
        eip: u32,
        eflags_misc: u32,
        fs_base: u32,
        fpu_cw: u32,
        fpu_sw: u32,
        fpu_tag: u32,
        fpu_top: u32,
        mm: u32,
        xmm: u32,
        split_flags: [u32; 7],
    }

    struct Callbacks {
        translated_entry: usize,
        x87_select: usize,
        write_memory: usize,
        string: usize,
        rdtsc: usize,
        cpuid: usize,
        x87: usize,
        float_ops: [usize; 14],
        clock_ns: usize,
    }

    struct Config {
        cpu: CpuLayout,
        guest_base: usize,
        guest_size: u32,
        null_limit: u32,
        store_hooks: bool,
        callbacks: Callbacks,
        pid: u32,
        tid: u32,
    }

    struct Block {
        code: usize,
        count: u32,
        byte_count: u32,
        lengths: [u8; 16],
        bytes: [u8; 240],
    }

    struct Rejection {
        length: u32,
        mnemonic: [u8; 48],
        detail: [u8; 160],
    }

    extern "Rust" {
        unsafe fn compile(
            pc: u32,
            data: *const u8,
            len: usize,
            config: &Config,
            out: &mut Block,
            rejection: &mut Rejection,
        ) -> CompileResult;
    }
}

use super::compile;

pub type X87Specialized = unsafe extern "C" fn(*mut Frame, u64, u32) -> i32;

impl Callbacks {
    pub fn validate(&self) -> Result<(), &'static str> {
        let required = [
            self.translated_entry,
            self.x87_select,
            self.write_memory,
            self.string,
            self.rdtsc,
            self.cpuid,
            self.x87,
            self.clock_ns,
        ];
        if required.iter().chain(&self.float_ops).any(|&f| f == 0) {
            return Err("baseline configuration is missing a callback");
        }
        Ok(())
    }

    pub fn translated_entry(&self, target: u32) -> bool {
        let function: unsafe extern "C" fn(u32) -> bool =
            unsafe { std::mem::transmute(self.translated_entry) };
        unsafe { function(target) }
    }

    pub fn x87_select(&self, call: &X87Call) -> Option<X87Specialized> {
        let function: unsafe extern "C" fn(*const X87Call) -> Option<X87Specialized> =
            unsafe { std::mem::transmute(self.x87_select) };
        unsafe { function(call) }
    }

    pub fn clock_ns(&self) -> u64 {
        let function: unsafe extern "C" fn() -> u64 = unsafe { std::mem::transmute(self.clock_ns) };
        unsafe { function() }
    }

    pub fn float(&self, op: FloatOp) -> usize {
        self.float_ops[op.repr as usize]
    }
}

impl Config {
    pub fn validate(&self) -> Result<(), &'static str> {
        self.callbacks.validate()?;
        if self.guest_base == 0 || self.guest_size < 8 || self.null_limit > self.guest_size {
            return Err("invalid baseline memory configuration");
        }
        Ok(())
    }
}

impl Rejection {
    pub fn fill(&mut self, length: usize, mnemonic: &str, detail: &str) {
        self.length = length as u32;
        copy_text(&mut self.mnemonic, mnemonic);
        copy_text(&mut self.detail, detail);
    }
}

fn copy_text(dest: &mut [u8], text: &str) {
    let n = text.len().min(dest.len() - 1);
    dest[..n].copy_from_slice(&text.as_bytes()[..n]);
    dest[n] = 0;
}
