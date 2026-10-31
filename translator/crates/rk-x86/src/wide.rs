use rk_core::ir::OpCode;
use rk_ir::SimdOp;

pub fn deferred(mnemonic: &str) -> bool {
    if mnemonic.starts_with("CMP") && mnemonic.ends_with("PS") {
        return true;
    }
    matches!(
        mnemonic,
        "MOVAPS"
            | "MOVUPS"
            | "MOVNTPS"
            | "MOVDQA"
            | "MOVDQU"
            | "MOVAPD"
            | "XORPS"
            | "ANDPS"
            | "ORPS"
            | "ANDNPS"
            | "ADDPS"
            | "SUBPS"
            | "MULPS"
            | "UNPCKLPS"
            | "UNPCKHPS"
            | "MOVLHPS"
            | "MOVHLPS"
            | "MOVLPS"
            | "MOVHPS"
            | "PUNPCKLQDQ"
            | "PUNPCKHQDQ"
            | "MINPS"
            | "MAXPS"
            | "DIVPS"
            | "SQRTPS"
            | "RCPPS"
            | "RSQRTPS"
            | "MOVMSKPS"
            | "PSRLDQ"
            | "SHUFPS"
            | "PSHUFD"
    )
}

pub fn semantic(mnemonic: &str) -> Option<SimdOp> {
    let opcode = match mnemonic {
        "MINPS" => OpCode::FloatMin,
        "MAXPS" => OpCode::FloatMax,
        "DIVPS" => OpCode::FloatDiv,
        "SQRTPS" => OpCode::FloatSqrt,
        "RCPPS" => OpCode::FloatRcp,
        "RSQRTPS" => OpCode::FloatRsqrt,
        "MOVMSKPS" => return Some(SimdOp::SignMask { lane_bytes: 4 }),
        _ => return None,
    };
    Some(SimdOp::Float {
        opcode,
        lane_bytes: 4,
    })
}
