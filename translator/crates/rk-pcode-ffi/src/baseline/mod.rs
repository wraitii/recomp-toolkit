mod abi;
mod emit;
mod helper;
mod ir;
mod jitdump;
mod state;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::slice;

use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{default_libcall_names, Linkage, Module};

use crate::lift::lift;
use abi::{Block, CompileResult, Config, Rejection};
use ir::{decode, Instruction, Rejected};

const MAX_FUNCTIONS: u32 = 65536;
const MAX_INSTRUCTION_BYTES: usize = 15;

struct Compiler {
    module: JITModule,
    count: u32,
}

thread_local! {
    static COMPILER: RefCell<Option<Compiler>> = const { RefCell::new(None) };
}

impl Compiler {
    fn new() -> Result<Self, String> {
        let verify = std::env::var("RECOMP_BASELINE_VERIFY").as_deref() == Ok("1");
        let builder = JITBuilder::with_flags(
            &[
                ("enable_verifier", if verify { "true" } else { "false" }),
                ("opt_level", "speed"),
            ],
            default_libcall_names(),
        )
        .map_err(|e| e.to_string())?;
        Ok(Self {
            module: JITModule::new(builder),
            count: 0,
        })
    }

    fn compile(
        &mut self,
        config: &Config,
        pc: u32,
        instructions: &[Instruction],
    ) -> Result<*const u8, String> {
        let profiling = jitdump::enabled();
        let target = self.module.target_config();
        let (ptr, call_conv) = (target.pointer_type(), target.default_call_conv);
        let signature = emit::signature(call_conv, ptr);
        let mut ctx = self.module.make_context();
        ctx.func.signature = signature.clone();
        let mut builder_ctx = FunctionBuilderContext::new();
        {
            let mut b = FunctionBuilder::new(&mut ctx.func, &mut builder_ctx);
            emit::build(&mut b, config, call_conv, ptr, pc, instructions, profiling);
            b.seal_all_blocks();
            b.finalize();
        }
        let name = jitdump::name(pc);
        let id = self
            .module
            .declare_function(&name, Linkage::Local, &signature)
            .map_err(|e| e.to_string())?;
        self.module
            .define_function(id, &mut ctx)
            .map_err(|e| e.to_string())?;
        let compiled = ctx.compiled_code().expect("defined function");
        let size = compiled.code_buffer().len();
        let mut locations = BTreeMap::new();
        if profiling {
            locations.insert(0, 0);
            for loc in compiled.buffer.get_srclocs_sorted() {
                let instruction = if loc.loc.is_default() {
                    0
                } else {
                    loc.loc.bits()
                };
                locations.insert(loc.start, instruction);
                locations.insert(loc.end, 0);
            }
            locations.remove(&(size as u32));
        }
        let locations: Vec<_> = locations.into_iter().collect();
        self.module
            .finalize_definitions()
            .map_err(|e| e.to_string())?;
        self.count += 1;
        let code = self.module.get_finalized_function(id);
        if profiling {
            let function = jitdump::Function {
                name: &name,
                pc,
                instructions,
                locations: &locations,
                code: unsafe { slice::from_raw_parts(code, size) },
            };
            jitdump::record(&function, config);
        }
        Ok(code)
    }
}

fn collect(
    pc: u32,
    bytes: &[u8],
    config: &Config,
    max_instructions: usize,
    rejection: &mut Rejection,
) -> (Vec<Instruction>, usize) {
    let mut instructions = Vec::new();
    let mut offset = 0usize;
    while instructions.len() < max_instructions && offset < bytes.len() {
        let Some(addr) = pc.checked_add(offset as u32) else {
            break;
        };
        if offset != 0 && config.callbacks.translated_entry(addr) {
            break;
        }
        let window = &bytes[offset..bytes.len().min(offset + MAX_INSTRUCTION_BYTES)];
        let decoded = match lift(addr, window) {
            Err(error) => Err((window.len(), String::new(), error)),
            Ok(lifted) => {
                let (length, mnemonic) = (usize::from(lifted.length), lifted.mnemonic.clone());
                decode(lifted).map_err(|Rejected(detail)| (length, mnemonic, detail))
            }
        };
        let instruction = match decoded {
            Ok(instruction) => instruction,
            Err((length, mnemonic, detail)) => {
                if offset == 0 {
                    rejection.fill(length, &mnemonic, &detail);
                }
                break;
            }
        };
        offset += usize::from(instruction.length);
        let terminal = instruction.terminal();
        instructions.push(instruction);
        if terminal {
            break;
        }
    }
    (instructions, offset)
}

fn failed(rejection: &mut Rejection, error: &str) -> CompileResult {
    rejection.fill(0, "", error);
    CompileResult::Error
}

unsafe fn compile(
    pc: u32,
    data: *const u8,
    len: usize,
    config: &Config,
    out: &mut Block,
    rejection: &mut Rejection,
) -> CompileResult {
    if data.is_null() {
        return failed(rejection, "baseline compile received no guest bytes");
    }
    if let Err(error) = config.validate() {
        return failed(rejection, error);
    }
    out.code = 0;
    out.count = 0;
    out.byte_count = 0;
    let bytes = slice::from_raw_parts(data, len.min(out.bytes.len()));
    let (instructions, byte_count) = collect(pc, bytes, config, out.lengths.len(), rejection);
    let Some(first) = instructions.first() else {
        return CompileResult::Rejected;
    };
    out.count = instructions.len() as u32;
    out.byte_count = byte_count as u32;
    for (length, instruction) in out.lengths.iter_mut().zip(&instructions) {
        *length = instruction.length;
    }
    out.bytes[..byte_count].copy_from_slice(&bytes[..byte_count]);
    COMPILER.with(|cell| {
        let mut cell = cell.borrow_mut();
        let compiler = match cell.as_mut() {
            Some(compiler) => compiler,
            None => match Compiler::new() {
                Ok(compiler) => cell.insert(compiler),
                Err(error) => return failed(rejection, &error),
            },
        };
        if compiler.count >= MAX_FUNCTIONS {
            rejection.fill(
                usize::from(first.length),
                &first.mnemonic,
                "native block allocation limit reached",
            );
            return CompileResult::Rejected;
        }
        match compiler.compile(config, pc, &instructions) {
            Ok(code) => {
                out.code = code as usize;
                CompileResult::Compiled
            }
            Err(error) => failed(rejection, &error),
        }
    })
}
