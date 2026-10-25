use rk_ir::x87::{ArithmeticOp, X87Condition, X87Unary};
use rk_ssa::ValueId;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Type {
    F64,
    F32,
    U8,
    U16,
    U32,
    U64,
    I16,
    I32,
    I64,
    Cw,
    Sw,
    Tag,
    Bool,
    Void,
}

impl Type {
    pub fn c_name(self) -> &'static str {
        match self {
            Type::F64 => "double",
            Type::F32 => "float",
            Type::U8 => "uint8_t",
            Type::U16 => "uint16_t",
            Type::U32 => "uint32_t",
            Type::U64 => "uint64_t",
            Type::I16 => "int16_t",
            Type::I32 => "int32_t",
            Type::I64 => "int64_t",
            Type::Cw | Type::Sw => "uint16_t",
            Type::Tag => "unsigned",
            Type::Bool => "int",
            Type::Void => "void",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Expr {
    Literal(Type, u64),
    Constant(&'static str),
    Local(u32, Type),
    Carry(i32, &'static str),
    Environment(&'static str),
    Resident(&'static str),
    Cw(u16),
    External(ValueId),
    Cpu,
    Ln2,
    FlagCondition(X87Condition),
    Eax,
    AxMerge(Box<Expr>),
    St(u8),
    Slot(&'static str, Box<Expr>),
    Physical(Box<Expr>, u32),
    Rotate(Box<Expr>, i8),
    Binary(&'static str, Box<Expr>, Box<Expr>),
    Group(Box<Expr>),
    Cast(Type, Box<Expr>),
    Negate(Box<Expr>),
    ClearMask(u16),
    NanFold(Box<Expr>),
    NanTest(Box<Expr>),
    FloatStore(Box<Expr>, Box<Expr>),
    PcRound(Box<Expr>, Box<Expr>),
    FloatArithmetic(
        &'static str,
        Box<Expr>,
        Box<Expr>,
        Box<Expr>,
        Option<Box<Expr>>,
    ),
    StatusWord(Box<Expr>, Box<Expr>),
    Load {
        bits: u16,
        integer: bool,
        address: Box<Expr>,
    },
    LoadWord(Box<Expr>),
    IntegerConvert {
        bits: u16,
        args: Vec<Expr>,
    },
    Helper(&'static str, Vec<Expr>),
}

impl Expr {
    pub fn ty(&self) -> Type {
        match self {
            Expr::Literal(ty, _) | Expr::Cast(ty, _) => *ty,
            Expr::Constant(_) => Type::F64,
            Expr::Local(_, ty) => *ty,
            Expr::Carry(_, part) => part_type(part),
            Expr::Environment(name) | Expr::Resident(name) => match *name {
                "cw" => Type::Cw,
                "sw" => Type::Sw,
                "top" => Type::U32,
                _ => Type::Tag,
            },
            Expr::Cw(_) => Type::Cw,
            Expr::External(_) | Expr::Eax | Expr::AxMerge(_) => Type::U32,
            Expr::Cpu | Expr::Ln2 => Type::F64,
            Expr::FlagCondition(_) => Type::Bool,
            Expr::St(_) => Type::F64,
            Expr::Slot(part, _) => part_type(part),
            Expr::Physical(_, _) | Expr::Rotate(_, _) => Type::U32,
            Expr::Binary(_, lhs, _) => lhs.ty(),
            Expr::Group(value) | Expr::Negate(value) => value.ty(),
            Expr::ClearMask(_) => Type::U16,
            Expr::NanFold(value) | Expr::PcRound(_, value) => value.ty(),
            Expr::NanTest(_) => Type::U16,
            Expr::FloatStore(..) => Type::F32,
            Expr::FloatArithmetic(..) => Type::F64,
            Expr::StatusWord(_, _) | Expr::LoadWord(_) => Type::U16,
            Expr::Load { bits, integer, .. } => match (*bits, *integer) {
                (16, true) => Type::I16,
                (32, true) => Type::I32,
                (64, true) => Type::I64,
                _ => Type::F64,
            },
            Expr::IntegerConvert { bits, .. } => match bits {
                16 => Type::I16,
                32 => Type::I32,
                _ => Type::I64,
            },
            Expr::Helper(name, _) => helper_type(name),
        }
    }
}

pub fn part_type(part: &str) -> Type {
    match part {
        "value" => Type::F64,
        "bits" => Type::U64,
        "exact" => Type::U8,
        "tag" => Type::Tag,
        _ => Type::U32,
    }
}

fn helper_type(name: &str) -> Type {
    match name {
        "ftag_classify" => Type::Tag,
        "fstsw" => Type::U16,
        "fto_float" => Type::F32,
        "fist" => Type::I64,
        "fcomi_flags" => Type::U16,
        "fabs"
        | "sqrt"
        | "sin"
        | "cos"
        | "tan"
        | "expm1"
        | "log2"
        | "log1p"
        | "fx87"
        | "fx87_exact"
        | "fround"
        | "fdivz"
        | "fprem"
        | "fscale"
        | "atan2"
        | "fxtract_exponent"
        | "fxtract_significand" => Type::F64,
        _ => Type::Void,
    }
}

pub fn literal(value: u64, ty: Type) -> Expr {
    Expr::Literal(ty, value)
}

pub fn zero() -> Expr {
    literal(0, Type::U8)
}

pub fn one() -> Expr {
    literal(1, Type::U8)
}

pub fn empty() -> Expr {
    literal(3, Type::Tag)
}

pub fn slot(part: &'static str, physical: Expr) -> Expr {
    Expr::Slot(part, Box::new(physical))
}

pub fn env(part: &'static str) -> Expr {
    Expr::Environment(part)
}

pub fn resident(part: &'static str) -> Expr {
    Expr::Resident(part)
}

pub fn external(value: ValueId) -> Expr {
    Expr::External(value)
}

pub fn group(value: Expr) -> Expr {
    Expr::Group(Box::new(value))
}

pub fn cast(ty: Type, value: Expr) -> Expr {
    Expr::Cast(ty, Box::new(value))
}

pub fn binary(op: &'static str, lhs: Expr, rhs: Expr) -> Expr {
    Expr::Binary(op, Box::new(lhs), Box::new(rhs))
}

pub fn helper(name: &'static str, args: Vec<Expr>) -> Expr {
    Expr::Helper(name, args)
}

pub fn constant_float(name: &'static str) -> Expr {
    Expr::Constant(name)
}

pub fn carry(position: i32, part: &'static str) -> Expr {
    Expr::Carry(position, part)
}

pub fn st(index: u8) -> Expr {
    Expr::St(index)
}

pub fn load(bits: u16, address: Expr, integer: bool) -> Expr {
    Expr::Load {
        bits,
        integer,
        address: Box::new(address),
    }
}

pub fn arithmetic(op: ArithmeticOp, lhs: Expr, rhs: Expr, sw: Expr) -> Expr {
    match op {
        ArithmeticOp::Div => helper("fdivz", vec![sw, lhs, rhs]),
        ArithmeticOp::Add => binary("+", lhs, rhs),
        ArithmeticOp::Sub => binary("-", lhs, rhs),
        ArithmeticOp::Mul => binary("*", lhs, rhs),
    }
}

pub fn unary(op: X87Unary, value: Expr, sw: Expr, cw: Expr) -> Expr {
    match op {
        X87Unary::Abs => helper("fabs", vec![value]),
        X87Unary::Negate => Expr::Negate(Box::new(value)),
        X87Unary::Sqrt => helper("fx87", vec![sw, cw, helper("sqrt", vec![value])]),
        X87Unary::Round => helper("fx87_exact", vec![sw, helper("fround", vec![cw, value])]),
        X87Unary::Sin => helper("fx87_exact", vec![sw, helper("sin", vec![value])]),
        X87Unary::Cos => helper("fx87_exact", vec![sw, helper("cos", vec![value])]),
        X87Unary::ExpM1 => helper(
            "fx87_exact",
            vec![sw, helper("expm1", vec![binary("*", value, Expr::Ln2)])],
        ),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assign {
    pub target: Expr,
    pub value: Expr,
    pub op: &'static str,
}

impl Assign {
    pub fn new(target: Expr, value: Expr) -> Self {
        Self {
            target,
            value,
            op: "=",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Activation {
    pub index: u32,
    pub clone: Option<&'static str>,
    pub choice: Choice,
    pub known_mode: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    Dynamic,
    Fast,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    Assign(Assign),
    Evaluate(Expr),
    Store {
        bits: u16,
        address: Expr,
        value: Expr,
        floating: bool,
        bcd: bool,
    },
    Bitcast {
        bits: u16,
        target: Expr,
        value: Expr,
        floating: bool,
    },
    PublishTags(Vec<(Expr, Expr)>),
    Activation(Activation),
    Reset,
    Declare(Expr),
    Conditional(Expr, Vec<Event>),
    EmptyTags(bool),
}
