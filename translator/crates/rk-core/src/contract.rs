use serde::{Deserialize, Serialize};

macro_rules! fields {
    ($($variant:ident => $name:literal),* $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[repr(u8)]
        pub enum Field {
            $($variant,)*
        }

        impl Field {
            pub const ALL: [Field; [$(stringify!($variant)),*].len()] = [$(Field::$variant,)*];

            pub fn bit(self) -> u16 {
                1 << self as u8
            }

            pub fn name(self) -> &'static str {
                [$($name,)*][self as usize]
            }

            pub fn from_name(name: &str) -> Option<Field> {
                Field::ALL
                    .into_iter()
                    .find(|field| field.name().eq_ignore_ascii_case(name))
            }
        }
    };
}

fields! {
    Eax => "EAX",
    Ecx => "ECX",
    Edx => "EDX",
    Ebx => "EBX",
    Esp => "ESP",
    Ebp => "EBP",
    Esi => "ESI",
    Edi => "EDI",
    Cf => "CF",
    Pf => "PF",
    Af => "AF",
    Zf => "ZF",
    Sf => "SF",
    Of => "OF",
}

const fn fields_bits(fields: &[Field]) -> Fields {
    let mut bits = 0u16;
    let mut index = 0;
    while index < fields.len() {
        bits |= 1 << fields[index] as u8;
        index += 1;
    }
    Fields(bits)
}

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct Fields(pub u16);

impl Fields {
    pub const ALL: Fields = Fields((1u16 << Field::ALL.len()) - 1);
    pub const GPRS: Fields = Fields((1u16 << Field::Cf as u8) - 1);
    pub const FLAGS: Fields = Fields(Fields::ALL.0 & !Fields::GPRS.0);
    pub const NONE: Fields = Fields(0);

    pub fn contains(self, field: Field) -> bool {
        self.0 & field.bit() != 0
    }

    pub fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub fn intersection(self, other: Fields) -> Fields {
        Fields(self.0 & other.0)
    }

    pub fn difference(self, other: Fields) -> Fields {
        Fields(self.0 & !other.0 & Fields::ALL.0)
    }

    pub fn contains_all(self) -> bool {
        self.0 == Fields::ALL.0
    }

    pub fn insert(&mut self, field: Field) {
        self.0 |= field.bit();
    }

    pub fn iter(self) -> impl Iterator<Item = Field> {
        Field::ALL.into_iter().filter(move |f| self.contains(*f))
    }
}

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
)]
pub struct PrivateArguments {
    pub registers: Fields,
    pub st0: bool,
}

impl PrivateArguments {
    pub fn is_empty(self) -> bool {
        self.registers.is_empty() && !self.st0
    }
}

impl FromIterator<Field> for Fields {
    fn from_iter<I: IntoIterator<Item = Field>>(iter: I) -> Self {
        iter.into_iter()
            .fold(Fields::NONE, |fields, field| fields | Fields(field.bit()))
    }
}

impl std::ops::BitOr for Fields {
    type Output = Fields;
    fn bitor(self, rhs: Fields) -> Fields {
        Fields(self.0 | rhs.0)
    }
}

impl std::ops::BitAnd for Fields {
    type Output = Fields;
    fn bitand(self, rhs: Fields) -> Fields {
        Fields(self.0 & rhs.0)
    }
}

impl std::ops::BitOrAssign for Fields {
    fn bitor_assign(&mut self, rhs: Fields) {
        self.0 |= rhs.0;
    }
}

impl std::ops::Not for Fields {
    type Output = Fields;
    fn not(self) -> Fields {
        Fields(!self.0 & Fields::ALL.0)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Contract {
    pub reads: Fields,
    pub kills: Fields,
    pub writes: Fields,
    pub preserves: Fields,
}

impl Contract {
    pub const CONSERVATIVE: Contract = Contract {
        reads: Fields::ALL,
        kills: Fields::NONE,
        writes: Fields::ALL,
        preserves: Fields::NONE,
    };

    pub const ABI: Contract = Contract {
        reads: Fields::ALL,
        kills: Fields::NONE,
        writes: fields_bits(&[
            Field::Eax,
            Field::Ecx,
            Field::Edx,
            Field::Esp,
            Field::Cf,
            Field::Pf,
            Field::Af,
            Field::Zf,
            Field::Sf,
            Field::Of,
        ]),
        preserves: fields_bits(&[Field::Ebx, Field::Ebp, Field::Esi, Field::Edi]),
    };
}
