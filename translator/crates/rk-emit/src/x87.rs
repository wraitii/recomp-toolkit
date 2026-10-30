use rk_core::Result;
use rk_ir::x87::X87Condition;
use rk_ssa::{BlockId, ValueId};
use rk_x87::{Event, Expr, Plan, Type};

pub struct St0Text {
    pub value: String,
    pub bits: String,
    pub exact: String,
    pub tag: String,
    pub canonical: bool,
}

pub trait External {
    fn value(&self, id: ValueId) -> Result<String>;
    fn call_hooks(&self) -> bool;
}

fn carry_name(position: i32, part: &str) -> String {
    let tag = if position < 0 {
        format!("m{}", -position)
    } else {
        position.to_string()
    };
    format!("x87c{tag}_{part}")
}

fn constant(name: &str) -> &'static str {
    match name {
        "FLD1" => "1.0",
        "FLDZ" => "0.0",
        "FLDPI" => "3.14159265358979323846",
        "FLDLN2" => "0.69314718055994530942",
        "FLDL2E" => "1.44269504088896340736",
        "FLDLG2" => "0.30102999566398119521",
        "FLDL2T" => "3.32192809488736234787",
        _ => "0.0",
    }
}

fn condition(condition: X87Condition) -> &'static str {
    match condition {
        X87Condition::B => "c->eflags_cf",
        X87Condition::E => "c->eflags_zf",
        X87Condition::Be => "(c->eflags_cf || c->eflags_zf)",
        X87Condition::U => "c->eflags_pf",
        X87Condition::Nb => "!c->eflags_cf",
        X87Condition::Ne => "!c->eflags_zf",
        X87Condition::Nbe => "(!c->eflags_cf && !c->eflags_zf)",
        X87Condition::Nu => "!c->eflags_pf",
    }
}

pub struct Printer<'a> {
    pub external: &'a dyn External,
    pub lazy_environment: bool,
}

impl Printer<'_> {
    fn resident(&self, part: &str) -> String {
        if self.lazy_environment && matches!(part, "top" | "sw" | "tag") {
            format!("x87_cached_{part}_")
        } else {
            format!("c->fpu_{part}")
        }
    }

    fn synchronize(&self, reload: bool) -> Vec<String> {
        if !self.lazy_environment {
            return Vec::new();
        }
        ["top", "sw", "tag"]
            .into_iter()
            .map(|part| {
                let cached = self.resident(part);
                if reload {
                    format!("{cached} = c->fpu_{part};")
                } else {
                    format!("c->fpu_{part} = {cached};")
                }
            })
            .collect()
    }

    pub fn expr(&self, value: &Expr) -> Result<String> {
        Ok(match value {
            Expr::Literal(ty, data) => {
                if *ty == Type::Tag && *data == 3 {
                    "FTAG_EMPTY".into()
                } else if *ty == Type::F64 {
                    format!("{data}.0")
                } else {
                    data.to_string()
                }
            }
            Expr::Constant(name) => constant(name).into(),
            Expr::Local(index, _) => format!("x87s{index}"),
            Expr::Carry(position, part) => carry_name(*position, part),
            Expr::St0Argument => "st0_".into(),
            Expr::Environment(part) => format!("x87_{part}_"),
            Expr::Resident(part) => self.resident(part),
            Expr::Cw(value) => format!("0x{value:x}u"),
            Expr::External(id) => self.external.value(*id)?,
            Expr::Cpu => "c".into(),
            Expr::Ln2 => "M_LN2".into(),
            Expr::FlagCondition(value) => condition(*value).into(),
            Expr::Eax => "c->r[0]".into(),
            Expr::AxMerge(value) => format!("(c->r[0] & 0xffff0000u) | {}", self.expr(value)?),
            Expr::St(index) => format!("ST(c, {index})"),
            Expr::Slot(part, physical) => {
                let physical = self.expr(physical)?;
                match *part {
                    "tag" if self.lazy_environment => {
                        format!("((x87_cached_tag_ >> (2u * ({physical}))) & 3u)")
                    }
                    "tag" => format!("ftag_of(c, {physical})"),
                    "value" => format!("c->st[{physical}]"),
                    "bits" => format!("c->st_bits[{physical}]"),
                    "exact" => format!("c->st_exact[{physical}]"),
                    _ => "0".into(),
                }
            }
            Expr::Physical(top, offset) => format!("({} + {offset}u) & 7u", self.expr(top)?),
            Expr::Rotate(top, delta) => {
                format!(
                    "({} {} 1u) & 7u",
                    self.expr(top)?,
                    if *delta < 0 { "-" } else { "+" }
                )
            }
            Expr::Binary(op, lhs, rhs) => format!("{} {op} {}", self.expr(lhs)?, self.expr(rhs)?),
            Expr::Group(value) => format!("({})", self.expr(value)?),
            Expr::Cast(ty, value) => format!("({}){}", ty.c_name(), self.expr(value)?),
            Expr::Negate(value) => format!("-({})", self.expr(value)?),
            Expr::ClearMask(mask) => format!("(uint16_t)~0x{mask:04x}u"),
            Expr::NanFold(value) => {
                let value = self.expr(value)?;
                format!("(RECOMP_UNLIKELY({value} != {value}) ? x87_indefinite() : {value})")
            }
            Expr::NanTest(value) => {
                let value = self.expr(value)?;
                format!("(uint16_t)({value} != {value})")
            }
            Expr::FloatStore(cw, value) => {
                let cw = self.expr(cw)?;
                let value = self.expr(value)?;
                format!(
                    "(RECOMP_UNLIKELY(({cw} & 0x300u) != 0u || {value} != {value}) ? fto_float_cw({cw}, {value}) : (float)({value}))"
                )
            }
            Expr::PcRound(cw, value) => {
                let cw = self.expr(cw)?;
                let value = self.expr(value)?;
                format!("(({cw} & 0x300u) == 0u ? (double)(float)({value}) : ({value}))")
            }
            Expr::FloatArithmetic(op, cw, lhs, rhs, sw) => {
                let cw = self.expr(cw)?;
                let lhs = self.expr(lhs)?;
                let rhs = self.expr(rhs)?;
                let raw = format!("{lhs} {op} {rhs}");
                let narrow = format!("(double)((float)({lhs}) {op} (float)({rhs}))");
                let (narrow, general) = match sw {
                    Some(sw) => {
                        let sw = self.expr(sw)?;
                        (
                            format!("fx87_exact_sw(&{sw}, {narrow})"),
                            format!("fx87_sw(&{sw}, {cw}, {raw})"),
                        )
                    }
                    None => (narrow, raw),
                };
                format!("(({cw} & 0x300u) == 0u ? {narrow} : {general})")
            }
            Expr::StatusWord(sw, top) => format!(
                "(uint16_t)(({} & (uint16_t)~0x3800u) | (({}) << 11))",
                self.expr(sw)?,
                self.expr(top)?
            ),
            Expr::Load {
                bits,
                integer,
                address,
            } => {
                let address = self.expr(address)?;
                if *bits <= 64 {
                    let call = format!(
                        "recomp_rd{}{bits}_(arena_, (uint32_t){address})",
                        if *integer { "" } else { "f" }
                    );
                    if *integer {
                        format!("(int{bits}_t){call}")
                    } else {
                        call
                    }
                } else if *integer {
                    format!("(int{bits}_t)rd{bits}({address})")
                } else {
                    format!("rdf{bits}({address})")
                }
            }
            Expr::LoadWord(address) => {
                format!("recomp_rd16_(arena_, (uint32_t){})", self.expr(address)?)
            }
            Expr::IntegerConvert { bits, args } => {
                let mut args = args.iter().map(|arg| self.expr(arg));
                let status = args.next().transpose()?.unwrap_or_default();
                let rest = args.collect::<Result<Vec<_>>>()?.join(", ");
                format!("fist_i{bits}_sw(&{status}, {rest})")
            }
            Expr::Helper(name, args) => {
                let args = args
                    .iter()
                    .map(|arg| self.expr(arg))
                    .collect::<Result<Vec<_>>>()?;
                let joined = args.join(", ");
                let status = args.first().map_or(String::new(), |arg| format!("&{arg}"));
                let rest = args.iter().skip(1).cloned().collect::<Vec<_>>().join(", ");
                match *name {
                    "fcom" | "fucom" => {
                        format!(
                            "recomp_fcom_({status}, {rest}, {}u)",
                            (*name == "fucom") as u8
                        )
                    }
                    "fcomi_flags" => format!("fcomi_flags_sw({status}, {rest})"),
                    "fdivz" => format!("fdivz_sw({status}, {rest})"),
                    "fprem" => format!("fprem_sw({status}, {rest})"),
                    "fx87_exact" => format!("fx87_exact_sw({status}, {rest})"),
                    "fx87" => format!("fx87_sw({status}, {rest})"),
                    "fround" => format!("fround_rc({joined})"),
                    "fto_float" => format!("fto_float_cw({joined})"),
                    "status_word" => format!("({joined})"),
                    _ => format!("{name}({joined})"),
                }
            }
        })
    }

    pub fn declaration(&self, value: &Expr) -> Result<String> {
        Ok(format!("{} {};", value.ty().c_name(), self.expr(value)?))
    }

    pub fn events(&self, events: &[Event]) -> Result<Vec<String>> {
        let mut lines: Vec<String> = Vec::new();
        for event in events {
            let observes_cpu = match event {
                Event::Evaluate(value) => uses_cpu(value),
                Event::Assign(assign) => uses_cpu(&assign.value) || uses_cpu(&assign.target),
                Event::Store { value, address, .. } => uses_cpu(value) || uses_cpu(address),
                Event::Bitcast { value, target, .. } => uses_cpu(value) || uses_cpu(target),
                Event::EmptyTags(_) => true,
                _ => false,
            };
            if observes_cpu && self.lazy_environment {
                lines.extend(self.synchronize(false));
                let eager = Printer {
                    external: self.external,
                    lazy_environment: false,
                };
                lines.extend(eager.events(std::slice::from_ref(event))?);
                lines.extend(self.synchronize(true));
                continue;
            }
            match event {
                Event::CommitEnvironment => lines.extend(self.synchronize(false)),
                Event::ReloadEnvironment => lines.extend(self.synchronize(true)),
                Event::Reset => {}
                Event::Assign(assign) => {
                    let target = self.expr(&assign.target)?;
                    let value = self.expr(&assign.value)?;
                    lines.push(format!("{target} {} {value};", assign.op));
                }
                Event::Evaluate(value) => {
                    let text = self.expr(value)?;
                    if !text.is_empty() {
                        lines.push(format!("{text};"));
                    }
                }
                Event::Bitcast {
                    bits,
                    target,
                    value,
                    floating,
                } => {
                    let source_type = match (*floating, *bits) {
                        (true, 32) => "uint32_t",
                        (true, 64) => "uint64_t",
                        (false, 32) => "float",
                        (false, 64) => "double",
                        _ => unreachable!(),
                    };
                    lines.push(format!("{{ {source_type} x87_bits_ = ({}); RECOMP_BITCOPY(&{}, &x87_bits_, {}); }}",
                        self.expr(value)?, self.expr(target)?, bits / 8));
                }
                Event::Store {
                    bits,
                    address,
                    value,
                    floating,
                    bcd,
                } => {
                    let address = self.expr(address)?;
                    let value = self.expr(value)?;
                    if *bits <= 64 && !*bcd {
                        let call = format!("recomp_wr{}{bits}_", if *floating { "f" } else { "" });
                        lines.push(format!("{call}(arena_, (uint32_t){address}, {value});"));
                    } else {
                        let call = if *bcd {
                            format!("wrbcd{bits}")
                        } else if *floating {
                            format!("wrf{bits}")
                        } else {
                            format!("wr{bits}")
                        };
                        lines.push(format!("{call}({address}, {value});"));
                    }
                }
                Event::PublishTags(tags) => {
                    let masks = tags
                        .iter()
                        .map(|(physical, _)| {
                            Ok(format!("(3u << (2u * ({})))", self.expr(physical)?))
                        })
                        .collect::<Result<Vec<_>>>()?
                        .join(" | ");
                    let values = tags
                        .iter()
                        .map(|(physical, tag)| {
                            Ok(format!(
                                "(({}) << (2u * ({})))",
                                self.expr(tag)?,
                                self.expr(physical)?
                            ))
                        })
                        .collect::<Result<Vec<_>>>()?
                        .join(" | ");
                    lines.push(format!(
                        "{tag} = (uint16_t)(({tag} & ~({masks})) | {values});",
                        tag = self.resident("tag")
                    ));
                }
                Event::Activation(activation) => {
                    lines.push(format!("x87_top_ = {};", self.resident("top")));
                    if activation.clone == Some("fast") && !activation.known_mode {
                        let hooks = if self.external.call_hooks() {
                            "recomp_hooks_ever || "
                        } else {
                            ""
                        };
                        lines.push(format!(
                            "if (RECOMP_UNLIKELY({hooks}(c->fpu_cw & 0x0f00u) != 0u)) goto X87CW{};",
                            activation.index
                        ));
                    } else if activation.clone == Some("general") {
                        lines.push(format!("X87CW{}:;", activation.index));
                    }
                    lines.push("x87_cw_ = c->fpu_cw;".into());
                    lines.push(format!("x87_sw_ = {};", self.resident("sw")));
                }
                Event::Declare(_) => {}
                Event::Conditional(condition, body) => {
                    let body = self.events(body)?.join(" ");
                    lines.push(format!("if ({}) {{ {body} }}", self.expr(condition)?));
                }
                Event::EmptyTags(reset_top) => {
                    if *reset_top {
                        lines.push("c->fpu_top = 0u;".into());
                    }
                    lines.push("c->fpu_tag = 0xffffu;".into());
                }
            }
        }
        Ok(lines)
    }
}

pub trait PlanLines {
    fn block_lines(
        &self,
        clone: usize,
        block: BlockId,
        external: &dyn External,
    ) -> Result<Vec<String>>;
    fn event_lines(
        &self,
        clone: usize,
        value: ValueId,
        external: &dyn External,
    ) -> Result<Vec<String>>;
    fn end_lines(
        &self,
        clone: usize,
        block: BlockId,
        external: &dyn External,
    ) -> Result<Vec<String>>;
    fn default_lines(
        &self,
        clone: usize,
        value: ValueId,
        external: &dyn External,
    ) -> Result<Vec<String>>;
    fn after_lines(
        &self,
        clone: usize,
        value: ValueId,
        external: &dyn External,
    ) -> Result<Vec<String>>;
    fn declaration_lines(&self, external: &dyn External) -> Result<Vec<String>>;
    fn st0_handover(
        &self,
        clone: usize,
        value: ValueId,
        external: &dyn External,
    ) -> Result<Option<St0Text>>;
}

impl PlanLines for Plan {
    fn block_lines(
        &self,
        clone: usize,
        block: BlockId,
        external: &dyn External,
    ) -> Result<Vec<String>> {
        let printer = Printer {
            external,
            lazy_environment: self.lazy_environment,
        };
        printer.events(
            self.programs[clone]
                .blocks
                .get(&block)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
        )
    }

    fn event_lines(
        &self,
        clone: usize,
        value: ValueId,
        external: &dyn External,
    ) -> Result<Vec<String>> {
        let printer = Printer {
            external,
            lazy_environment: self.lazy_environment,
        };
        printer.events(
            self.programs[clone]
                .events
                .get(&value)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
        )
    }

    fn end_lines(
        &self,
        clone: usize,
        block: BlockId,
        external: &dyn External,
    ) -> Result<Vec<String>> {
        let printer = Printer {
            external,
            lazy_environment: self.lazy_environment,
        };
        printer.events(
            self.programs[clone]
                .ends
                .get(&block)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
        )
    }

    fn default_lines(
        &self,
        clone: usize,
        value: ValueId,
        external: &dyn External,
    ) -> Result<Vec<String>> {
        let printer = Printer {
            external,
            lazy_environment: self.lazy_environment,
        };
        printer.events(
            self.programs[clone]
                .defaults
                .get(&value)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
        )
    }

    fn st0_handover(
        &self,
        clone: usize,
        value: ValueId,
        external: &dyn External,
    ) -> Result<Option<St0Text>> {
        let printer = Printer {
            external,
            lazy_environment: self.lazy_environment,
        };
        let Some(handover) = self.programs[clone].arguments.get(&value) else {
            return Ok(None);
        };
        Ok(Some(St0Text {
            value: printer.expr(&handover.value)?,
            bits: printer.expr(&handover.bits)?,
            exact: printer.expr(&handover.exact)?,
            tag: printer.expr(&handover.tag)?,
            canonical: handover.canonical,
        }))
    }

    fn after_lines(
        &self,
        clone: usize,
        value: ValueId,
        external: &dyn External,
    ) -> Result<Vec<String>> {
        let printer = Printer {
            external,
            lazy_environment: self.lazy_environment,
        };
        printer.events(
            self.programs[clone]
                .after
                .get(&value)
                .map(Vec::as_slice)
                .unwrap_or(&[]),
        )
    }

    fn declaration_lines(&self, external: &dyn External) -> Result<Vec<String>> {
        let printer = Printer {
            external,
            lazy_environment: self.lazy_environment,
        };
        let mut lines = self
            .declarations
            .iter()
            .map(|value| printer.declaration(value))
            .collect::<Result<Vec<_>>>()?;
        if self.lazy_environment {
            for (part, ty) in [("top", "uint32_t"), ("sw", "uint16_t"), ("tag", "uint16_t")] {
                lines.push(format!("{ty} x87_cached_{part}_ = c->fpu_{part};"));
            }
        }
        Ok(lines)
    }
}

fn uses_cpu(value: &Expr) -> bool {
    match value {
        Expr::Cpu | Expr::St(_) => true,
        Expr::AxMerge(v)
        | Expr::Slot(_, v)
        | Expr::Physical(v, _)
        | Expr::Rotate(v, _)
        | Expr::Group(v)
        | Expr::Cast(_, v)
        | Expr::Negate(v)
        | Expr::NanFold(v)
        | Expr::NanTest(v)
        | Expr::LoadWord(v) => uses_cpu(v),
        Expr::Binary(_, a, b)
        | Expr::PcRound(a, b)
        | Expr::FloatStore(a, b)
        | Expr::StatusWord(a, b) => uses_cpu(a) || uses_cpu(b),
        Expr::FloatArithmetic(_, cw, a, b, sw) => {
            uses_cpu(cw) || uses_cpu(a) || uses_cpu(b) || sw.as_ref().is_some_and(|v| uses_cpu(v))
        }
        Expr::Load { address, .. } => uses_cpu(address),
        Expr::IntegerConvert { args, .. } | Expr::Helper(_, args) => args.iter().any(uses_cpu),
        _ => false,
    }
}
