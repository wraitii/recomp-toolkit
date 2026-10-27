//! Builds one `FunctionIr` per mapped function. It decodes and lifts each span, builds instruction
//! successors and external exits, and recovers jump tables; an unresolved indirect jump
//! conservatively reaches every instruction. It also proves `PUSH; RET` continuations and
//! popped-return jumps, marks noreturn and dead code, and classifies SEH frames, helpers and import
//! tails.
//! Functions are analyzed in parallel, and failures stay per-function diagnostics in
//! `Analysis::ir`.

use std::collections::{BTreeMap, BTreeSet};

use rayon::prelude::*;
use rk_core::ir::FunctionIr;
use rk_core::program::Program;
use rk_core::{Addr, Result};
use rk_decode::Decoder;
use rk_sleigh::Lifter;

mod cfg;
mod dump;
mod seh;
mod tables;

use cfg::{build_raw, finalize, RawFunction};
use seh::seh_helpers;

pub use dump::dump;
pub use seh::{msvc_chkstk, msvc_seh_epilog, msvc_seh_prolog};

pub(crate) const R_EAX: u8 = 0;
pub(crate) const R_ECX: u8 = 1;
pub(crate) const R_EDX: u8 = 2;
pub(crate) const R_ESP: u8 = 4;
pub(crate) const R_EBP: u8 = 5;

pub struct Analysis {
    pub ir: BTreeMap<Addr, Result<FunctionIr>>,
    pub seh_helpers: BTreeSet<Addr>,
}

thread_local! {
    static LIFTERS: std::cell::RefCell<Option<(Lifter, Decoder)>> = const { std::cell::RefCell::new(None) };
}

fn with_lifters<R>(f: impl FnOnce(&mut Lifter, &Decoder) -> R) -> R {
    LIFTERS.with(|slot| {
        let mut slot = slot.borrow_mut();
        let (lifter, decoder) = slot.get_or_insert_with(|| {
            (
                Lifter::new().expect("SLEIGH initialization"),
                Decoder::new().expect("capstone initialization"),
            )
        });
        f(lifter, decoder)
    })
}

pub fn analyze(program: &Program) -> Analysis {
    let addresses: Vec<Addr> = program.functions.keys().copied().collect();
    let relocated: BTreeMap<Addr, Addr> = program
        .image
        .relocations
        .iter()
        .map(|relocation| (relocation.site, relocation.target))
        .collect();
    let mut raw: Vec<(Addr, Result<RawFunction>)> = addresses
        .par_iter()
        .map(|&addr| {
            let result = with_lifters(|lifter, decoder| {
                build_raw(program, addr, lifter, decoder, &relocated)
            });
            (addr, result)
        })
        .collect();
    raw.sort_by_key(|(addr, _)| *addr);
    let helpers = seh_helpers(program, &raw);
    let ir: BTreeMap<Addr, Result<FunctionIr>> = raw
        .into_par_iter()
        .map(|(addr, raw)| {
            let result = raw.and_then(|raw| finalize(program, addr, raw, &helpers));
            (addr, result)
        })
        .collect();
    Analysis {
        ir,
        seh_helpers: helpers,
    }
}
