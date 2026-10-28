use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Base {
    Frame,
    Absolute,
    Value(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Address {
    Known { base: Base, offset: u32, span: u32 },
    Unknown,
}

impl Address {
    pub const fn exact(base: Base, offset: u32) -> Self {
        Self::Known {
            base,
            offset,
            span: 0,
        }
    }

    pub fn offset(self, delta: u32) -> Self {
        match self {
            Self::Known { base, offset, span } => Self::Known {
                base,
                offset: offset.wrapping_add(delta),
                span,
            },
            Self::Unknown => Self::Unknown,
        }
    }

    pub fn add(self, other: Self) -> Self {
        match (self, other) {
            (
                Self::Known { base, offset, span },
                Self::Known {
                    base: Base::Absolute,
                    offset: delta,
                    span: width,
                },
            )
            | (
                Self::Known {
                    base: Base::Absolute,
                    offset: delta,
                    span: width,
                },
                Self::Known { base, offset, span },
            ) => {
                match span
                    .checked_add(width)
                    .filter(|&span| span < i32::MAX as u32)
                {
                    Some(span) => Self::Known {
                        base,
                        offset: offset.wrapping_add(delta),
                        span,
                    },
                    None => Self::Unknown,
                }
            }
            _ => Self::Unknown,
        }
    }

    pub fn sub(self, other: Self) -> Self {
        match other {
            Self::Known {
                base: Base::Absolute,
                offset,
                span: 0,
            } => self.offset(offset.wrapping_neg()),
            Self::Known {
                base,
                offset,
                span: 0,
            } => match self {
                Self::Known {
                    base: own,
                    offset: at,
                    span: 0,
                } if own == base => Self::exact(Base::Absolute, at.wrapping_sub(offset)),
                _ => Self::Unknown,
            },
            _ => Self::Unknown,
        }
    }

    pub fn bounded(self, before: u32, after: u32) -> Self {
        match self {
            Self::Known { base, offset, span } => match span
                .checked_add(before)
                .and_then(|span| span.checked_add(after))
                .filter(|&span| span < i32::MAX as u32)
            {
                Some(span) => Self::Known {
                    base,
                    offset: offset.wrapping_sub(before),
                    span,
                },
                None => Self::Unknown,
            },
            Self::Unknown => Self::Unknown,
        }
    }

    pub fn is_exact(self) -> bool {
        matches!(self, Self::Known { span: 0, .. })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Range {
    pub address: Address,
    pub size: u32,
}

impl Range {
    pub fn new(address: Address, size: u32) -> Self {
        Self { address, size }
    }

    pub fn disjoint(self, other: Self) -> bool {
        let (
            Address::Known {
                base: a,
                offset: low_a,
                span: span_a,
            },
            Address::Known {
                base: b,
                offset: low_b,
                span: span_b,
            },
        ) = (self.address, other.address)
        else {
            return false;
        };
        if a != b || self.size == 0 || other.size == 0 {
            return false;
        }
        let low = low_a.wrapping_sub(low_b.wrapping_add(span_b));
        let high = low_a.wrapping_add(span_a).wrapping_sub(low_b);
        other.size <= low && low <= high && u64::from(high) + u64::from(self.size) <= 0x1_0000_0000
    }

    pub fn in_frame(self, frame: Address) -> Self {
        Self {
            address: match self.address {
                Address::Known {
                    base: Base::Frame,
                    offset,
                    span,
                } => frame.offset(offset).bounded(0, span),
                Address::Known {
                    base: Base::Absolute,
                    ..
                } => self.address,
                _ => Address::Unknown,
            },
            size: self.size,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Footprint {
    pub ranges: BTreeSet<Range>,
    pub opaque: bool,
}

impl Footprint {
    pub fn unknown() -> Self {
        Self {
            opaque: true,
            ..Self::default()
        }
    }

    pub fn insert(&mut self, range: Range) {
        if self.opaque {
            return;
        }
        if matches!(
            range.address,
            Address::Known {
                base: Base::Frame | Base::Absolute,
                ..
            }
        ) && range.size != 0
        {
            self.ranges.insert(range);
            if self.ranges.len() > 512 {
                self.ranges.clear();
                self.opaque = true;
            }
        } else {
            self.ranges.clear();
            self.opaque = true;
        }
    }

    pub fn join(&mut self, other: &Self) {
        if other.opaque {
            self.opaque = true;
            self.ranges.clear();
        }
        for &range in &other.ranges {
            self.insert(range);
        }
    }

    pub fn in_frame(&self, frame: Address) -> Self {
        let mut result = Self {
            opaque: self.opaque,
            ..Self::default()
        };
        for &range in &self.ranges {
            result.insert(range.in_frame(frame));
        }
        result
    }

    pub fn disjoint(&self, range: Range) -> bool {
        !self.opaque && self.ranges.iter().all(|&effect| effect.disjoint(range))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Effects {
    pub reads: Footprint,
    pub writes: Footprint,
}

impl Effects {
    pub fn unknown() -> Self {
        Self {
            reads: Footprint::unknown(),
            writes: Footprint::unknown(),
        }
    }

    pub fn join(&mut self, other: &Self) {
        self.reads.join(&other.reads);
        self.writes.join(&other.writes);
    }

    pub fn in_frame(&self, frame: Address) -> Self {
        Self {
            reads: self.reads.in_frame(frame),
            writes: self.writes.in_frame(frame),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disjoint_ranges_never_overlap_modulo_32_bits() {
        for a in -8i32..8 {
            for b in -8i32..8 {
                for span_a in 0..4 {
                    for span_b in 0..4 {
                        for size_a in 1..5 {
                            for size_b in 1..5 {
                                let left = Range::new(
                                    Address::exact(Base::Frame, a as u32).bounded(0, span_a),
                                    size_a,
                                );
                                let right = Range::new(
                                    Address::exact(Base::Frame, b as u32).bounded(0, span_b),
                                    size_b,
                                );
                                if !left.disjoint(right) {
                                    continue;
                                }
                                for offset in 0..span_a + size_a {
                                    assert!(
                                        (a as u32).wrapping_add(offset).wrapping_sub(b as u32)
                                            >= span_b + size_b,
                                        "{left:?} {right:?}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
        let frame = Range::new(Address::exact(Base::Frame, 0), 4);
        assert!(!frame.disjoint(Range::new(Address::exact(Base::Absolute, 0x500000), 4)));
        assert!(!frame.disjoint(Range::new(Address::Unknown, 4)));
    }

    #[test]
    fn call_frames_translate_without_inventing_provenance() {
        let range = Range::new(Address::exact(Base::Frame, 8), 4);
        assert_eq!(
            range
                .in_frame(Address::exact(Base::Frame, (-20i32) as u32))
                .address,
            Address::exact(Base::Frame, (-12i32) as u32)
        );
        assert_eq!(range.in_frame(Address::Unknown).address, Address::Unknown);
        let static_range = Range::new(Address::exact(Base::Absolute, 0x500000), 4);
        assert_eq!(static_range.in_frame(Address::Unknown), static_range);
    }

    #[test]
    fn summary_budget_widens_to_an_absorbing_unknown() {
        let mut footprint = Footprint::default();
        for offset in 0..600 {
            footprint.insert(Range::new(Address::exact(Base::Frame, offset * 4), 4));
        }
        assert_eq!(footprint, Footprint::unknown());
        footprint.join(&Footprint::default());
        footprint.insert(Range::new(Address::exact(Base::Absolute, 0x500000), 4));
        assert_eq!(footprint, Footprint::unknown());
    }
}
