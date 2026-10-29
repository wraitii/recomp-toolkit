use std::collections::BTreeSet;

use rk_core::contract::DirectArguments;
use rk_core::Addr;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Declaration {
    Raw(Addr),
    Direct(Addr),
    DirectVariant {
        addr: Addr,
        input_arguments: DirectArguments,
    },
}

#[derive(Clone, Debug, Default)]
pub struct Preludes {
    pub memory_helpers: String,
    pub return_helpers: String,
    pub table_return_helpers: String,
    pub float_helpers: String,
}

#[derive(Clone, Debug, Default)]
pub struct Requires {
    pub arena: bool,
    pub return_fast: bool,
    pub fcom: bool,
    pub seh_call: bool,
}

#[derive(Clone, Debug)]
pub struct Body {
    pub addr: Addr,
    pub insns: usize,
    pub first: Addr,
    pub last: Addr,
    pub text: String,
    pub requires: Requires,
    pub declarations: BTreeSet<Declaration>,
}
