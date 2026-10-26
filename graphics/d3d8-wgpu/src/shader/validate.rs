use crate::RenderError;

const OP_PHASE: u32 = 0xfffd;
const OP_COMMENT: u32 = 0xfffe;
const OP_END: u32 = 0xffff;

pub const VS_OPCODES: &[(u32, u16, u16, u16)] = &[
    (0x0000, 0, 0, 0),
    (0x0001, 2, 0, 0),
    (0x002e, 2, 512, 65535),
    (0x0002, 3, 0, 0),
    (0x0003, 3, 0, 0),
    (0x0004, 4, 0, 0),
    (0x0005, 3, 0, 0),
    (0x0006, 2, 0, 0),
    (0x0007, 2, 0, 0),
    (0x0008, 3, 0, 0),
    (0x0009, 3, 0, 0),
    (0x000a, 3, 0, 0),
    (0x000b, 3, 0, 0),
    (0x000c, 3, 0, 0),
    (0x000d, 3, 0, 0),
    (0x0023, 2, 0, 0),
    (0x000e, 2, 0, 0),
    (0x000f, 2, 0, 0),
    (0x004e, 2, 0, 0),
    (0x004f, 2, 0, 0),
    (0x0010, 2, 0, 0),
    (0x0011, 3, 0, 0),
    (0x0012, 4, 0, 0),
    (0x0013, 2, 0, 0),
    (0x0020, 3, 0, 0),
    (0x0021, 3, 0, 0),
    (0x0022, 4, 512, 513),
    (0x0022, 2, 768, 65535),
    (0x0024, 2, 0, 0),
    (0x0025, 4, 512, 513),
    (0x0025, 2, 768, 65535),
    (0x0014, 3, 0, 0),
    (0x0015, 3, 0, 0),
    (0x0016, 3, 0, 0),
    (0x0017, 3, 0, 0),
    (0x0018, 3, 0, 0),
    (0x001f, 2, 0, 0),
    (0x0051, 5, 0, 0),
    (0x002f, 2, 0, 0),
    (0x0030, 5, 0, 0),
    (0x0026, 1, 512, 65535),
    (0x0027, 0, 512, 65535),
    (0x0028, 1, 512, 65535),
    (0x0029, 2, 513, 65535),
    (0x002a, 0, 512, 65535),
    (0x002b, 0, 512, 65535),
    (0x002c, 0, 513, 65535),
    (0x002d, 2, 513, 65535),
    (0x0060, 1, 0, 0),
    (0x0019, 1, 512, 65535),
    (0x001a, 2, 512, 65535),
    (0x001b, 2, 512, 65535),
    (0x001c, 0, 512, 65535),
    (0x001d, 0, 512, 65535),
    (0x001e, 1, 512, 65535),
    (0x005e, 3, 0, 0),
    (0x005f, 3, 768, 65535),
];

pub const PS_OPCODES: &[(u32, u16, u16, u16)] = &[
    (0x0000, 0, 0, 0),
    (0x0001, 2, 0, 0),
    (0x0002, 3, 0, 0),
    (0x0003, 3, 0, 0),
    (0x0004, 4, 0, 0),
    (0x0005, 3, 0, 0),
    (0x0006, 2, 0, 0),
    (0x0007, 2, 0, 0),
    (0x0008, 3, 0, 0),
    (0x0009, 3, 0, 0),
    (0x000a, 3, 0, 0),
    (0x000b, 3, 0, 0),
    (0x000c, 3, 0, 0),
    (0x000d, 3, 0, 0),
    (0x0023, 2, 0, 0),
    (0x000e, 2, 0, 0),
    (0x000f, 2, 0, 0),
    (0x004e, 2, 0, 0),
    (0x004f, 2, 0, 0),
    (0x0011, 3, 0, 0),
    (0x0012, 4, 0, 0),
    (0x0013, 2, 0, 0),
    (0x0050, 4, 256, 260),
    (0x0058, 4, 258, 768),
    (0x0020, 3, 0, 0),
    (0x0021, 3, 0, 0),
    (0x0024, 2, 0, 0),
    (0x0025, 4, 512, 513),
    (0x0025, 2, 768, 65535),
    (0x005a, 4, 512, 65535),
    (0x0014, 3, 0, 0),
    (0x0015, 3, 0, 0),
    (0x0016, 3, 0, 0),
    (0x0017, 3, 0, 0),
    (0x0018, 3, 0, 0),
    (0x001f, 2, 0, 0),
    (0x0026, 1, 513, 65535),
    (0x0027, 0, 513, 65535),
    (0x0028, 1, 513, 65535),
    (0x0029, 2, 513, 65535),
    (0x002a, 0, 513, 65535),
    (0x002b, 0, 513, 65535),
    (0x002c, 0, 513, 65535),
    (0x002d, 2, 513, 65535),
    (0x0060, 1, 0, 0),
    (0x0019, 1, 513, 65535),
    (0x001a, 2, 513, 65535),
    (0x001b, 2, 768, 65535),
    (0x001c, 0, 513, 65535),
    (0x001d, 0, 768, 65535),
    (0x001e, 1, 513, 65535),
    (0x0051, 5, 0, 0),
    (0x002f, 2, 0, 0),
    (0x0030, 5, 0, 0),
    (0x0040, 1, 0, 259),
    (0x0040, 2, 260, 260),
    (0x0041, 1, 256, 768),
    (0x0042, 1, 0, 259),
    (0x0042, 2, 260, 260),
    (0x0042, 3, 512, 65535),
    (0x0043, 2, 0, 259),
    (0x0044, 2, 256, 259),
    (0x0045, 2, 256, 259),
    (0x0046, 2, 256, 259),
    (0x0052, 2, 258, 259),
    (0x0047, 2, 256, 259),
    (0x0048, 2, 256, 259),
    (0x0049, 2, 256, 259),
    (0x004b, 2, 0, 0),
    (0x004c, 3, 256, 259),
    (0x004d, 2, 256, 259),
    (0x004a, 2, 256, 259),
    (0x0053, 2, 258, 259),
    (0x0054, 2, 259, 259),
    (0x0055, 2, 258, 259),
    (0x0056, 2, 258, 259),
    (0x0057, 1, 260, 260),
    (0x0059, 3, 260, 260),
    (0x005b, 2, 513, 65535),
    (0x005c, 2, 513, 65535),
    (0x005d, 5, 513, 65535),
    (0x005e, 3, 0, 0),
    (0x005f, 3, 768, 65535),
    (0xfffd, 0, 0, 0),
];

fn invalid(op: &'static str, why: impl Into<String>) -> RenderError {
    RenderError::invalid(op, why)
}

fn unsupported(op: &'static str, why: impl Into<String>) -> RenderError {
    RenderError::new(op, why)
}

fn opcode_params(op: u32, pixel: bool, version: u32) -> Option<u16> {
    let table = if pixel { PS_OPCODES } else { VS_OPCODES };
    for &(candidate, params, min, max) in table {
        if candidate == op && version >= u32::from(min) && (max == 0 || version <= u32::from(max)) {
            return Some(params);
        }
    }
    None
}

fn register_operand_count(op: u32, params: u16) -> u16 {
    match op {
        0x1f => 0,
        0x51 | 0x30 | 0x2f => 1,
        _ => params,
    }
}

fn version_supported(pixel: bool, major: u32, minor: u32) -> bool {
    if pixel {
        (major == 1 && (1..=4).contains(&minor))
            || (major == 2 && minor <= 1)
            || (major == 3 && minor == 0)
    } else {
        (major == 1 && minor == 1) || (major == 2 && minor <= 1) || (major == 3 && minor == 0)
    }
}

pub fn shader_word_count(words: &[u32], pixel: bool) -> Result<usize, RenderError> {
    if words.is_empty() {
        return Err(invalid("CreateShader", "shader bytecode is empty"));
    }
    let version_token = words[0];
    let high = version_token >> 16;
    if high != 0xffff && high != 0xfffe {
        return Err(invalid(
            "CreateShader",
            "shader bytecode does not start with a version token",
        ));
    }
    if (high == 0xffff) != pixel {
        return Err(invalid(
            "CreateShader",
            "shader version token does not match the shader stage",
        ));
    }
    let major = (version_token >> 8) & 0xff;
    let minor = version_token & 0xff;
    if !version_supported(pixel, major, minor) {
        let stage = if pixel { "ps" } else { "vs" };
        return Err(unsupported(
            "CreateShader",
            format!(
                "shader model {stage}_{major}_{minor} (raw token {version_token:#010x}) is not implemented"
            ),
        ));
    }
    let version = major * 256 + minor;
    let mut index = 1usize;
    let mut phase_seen = false;
    while index < words.len() {
        let token = words[index];
        let op = token & 0xffff;
        if op == OP_END {
            if token != OP_END {
                return Err(invalid(
                    "CreateShader",
                    "END token is not the canonical 0x0000FFFF",
                ));
            }
            return Ok(index + 1);
        }
        if op == OP_COMMENT {
            let extra = ((token >> 16) & 0x7fff) as usize;
            index += 1 + extra;
        } else if op == OP_PHASE {
            if token != OP_PHASE {
                return Err(invalid(
                    "CreateShader",
                    "PHASE token carries unexpected bits",
                ));
            }
            if !pixel || major != 1 || minor != 4 {
                return Err(unsupported(
                    "CreateShader",
                    "PHASE is only legal in pixel shader model 1.4",
                ));
            }
            if phase_seen {
                return Err(invalid(
                    "CreateShader",
                    "pixel shader 1.x contains more than one PHASE marker",
                ));
            }
            if index + 1 >= words.len() {
                return Err(invalid(
                    "CreateShader",
                    "PHASE marker is not followed by a second-phase instruction",
                ));
            }
            phase_seen = true;
            index += 1;
        } else {
            let Some(params) = opcode_params(op, pixel, version) else {
                return Err(unsupported(
                    "CreateShader",
                    format!(
                        "opcode {op} is not valid for {}{major}.{minor}",
                        if pixel { "ps_" } else { "vs_" }
                    ),
                ));
            };
            if major >= 2 {
                let declared = ((token >> 24) & 0xf) as usize;
                let registers = register_operand_count(op, params);
                let mut expected = 0usize;
                let mut at = index + 1;
                for operand in 0..params {
                    if at >= words.len() {
                        return Err(invalid(
                            "CreateShader",
                            "instruction operands extend past the staged bytecode",
                        ));
                    }
                    expected += 1;
                    if operand < registers && (words[at] & 0x2000) != 0 {
                        expected += 1;
                        at += 1;
                    }
                    at += 1;
                }
                if expected != declared {
                    return Err(invalid(
                        "CreateShader",
                        format!(
                            "instruction length {declared} does not match its operand count {expected}"
                        ),
                    ));
                }
                index += 1 + declared;
            } else {
                index += 1 + usize::from(params);
            }
        }
        if index > words.len() {
            return Err(invalid(
                "CreateShader",
                "shader instruction extends past the end of the staged bytecode",
            ));
        }
    }
    Err(invalid(
        "CreateShader",
        "shader bytecode has no END token within the staged window",
    ))
}
