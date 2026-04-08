//! Bytecode instruction set for the Nix evaluator.
//!
//! A stack-based instruction set: operands are pushed/popped from the
//! value stack, and inline operands (constant indices, jump offsets,
//! counts) are encoded as 16-bit values following the opcode byte.

/// Bytecode instructions for the Nix VM.
///
/// Each variant occupies exactly one byte (`#[repr(u8)]`). Inline
/// operands (constant pool index, jump offset, element count) follow
/// the opcode in the bytecode stream as 16-bit little-endian values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum OpCode {
    // ── Constants ───────────────────────────────────────────────
    /// Push a constant from the constant pool.
    /// Operand: u16 constant index.
    Constant = 0,
    /// Push `null`.
    Null = 1,
    /// Push `true`.
    True = 2,
    /// Push `false`.
    False = 3,

    // ── Arithmetic ─────────────────────────────────────────────
    /// Pop two values, push their sum (int+int, float+float, int+float).
    Add = 10,
    /// Pop two values, push their difference.
    Sub = 11,
    /// Pop two values, push their product.
    Mul = 12,
    /// Pop two values, push their quotient. Errors on division by zero.
    Div = 13,
    /// Pop one value, push its arithmetic negation.
    Negate = 14,

    // ── Logical ────────────────────────────────────────────────
    /// Pop one bool, push its logical negation.
    Not = 20,
    /// Pop two bools, push logical AND (short-circuit handled at compile time).
    And = 21,
    /// Pop two bools, push logical OR (short-circuit handled at compile time).
    Or = 22,
    /// Pop two values, push `a -> b` (logical implication: `!a || b`).
    Implication = 23,

    // ── Comparison ─────────────────────────────────────────────
    /// Pop two values, push `true` if equal.
    Equal = 30,
    /// Pop two values, push `true` if not equal.
    NotEqual = 31,
    /// Pop two values, push `true` if left < right.
    Less = 32,
    /// Pop two values, push `true` if left > right.
    Greater = 33,
    /// Pop two values, push `true` if left <= right.
    LessEqual = 34,
    /// Pop two values, push `true` if left >= right.
    GreaterEqual = 35,

    // ── Strings ────────────────────────────────────────────────
    /// Pop N string parts, concatenate into one string.
    /// Operand: u16 part count.
    Interpolate = 40,

    // ── Variables ──────────────────────────────────────────────
    /// Push a local variable by stack slot index.
    /// Operand: u16 slot index (relative to current frame's stack base).
    GetLocal = 50,
    /// Set a local variable by stack slot index.
    /// Operand: u16 slot index.
    SetLocal = 51,

    // ── Attribute sets ─────────────────────────────────────────
    /// Pop N key-value pairs (key on top, value below), construct attrset.
    /// Operand: u16 pair count.
    MakeAttrs = 60,
    /// Pop attrset and key (string constant index), push `attrset.key`.
    /// Operand: u16 constant index for the key name.
    GetAttr = 61,
    /// Pop attrset and key (string constant index), push bool.
    /// Operand: u16 constant index for the key name.
    HasAttr = 62,
    /// Pop two attrsets, push merged result (right overrides left, `//`).
    UpdateAttrs = 63,
    /// Pop attrset, key constant, and default value, push value or default.
    /// Stack order (top to bottom): default, attrset.
    /// Operand: u16 constant index for the key name.
    SelectOrDefault = 64,

    // ── Lists ──────────────────────────────────────────────────
    /// Pop N values, construct a list.
    /// Operand: u16 element count.
    MakeList = 70,
    /// Pop two lists, push concatenated result (`++`).
    Concat = 71,

    // ── Functions ──────────────────────────────────────────────
    /// Create a closure from a sub-chunk.
    /// Operand: u16 constant index pointing to the function's `Chunk`.
    /// Followed by u16 upvalue count, then for each upvalue:
    ///   u8 (1 = local, 0 = upvalue of enclosing), u16 index.
    MakeClosure = 80,
    /// Pop function and argument, call the function.
    Call = 81,
    /// Return from the current call frame.
    Return = 82,

    // ── Control flow ───────────────────────────────────────────
    /// Unconditional jump.
    /// Operand: u16 absolute target offset.
    Jump = 90,
    /// Pop condition; if false, jump to target.
    /// Operand: u16 absolute target offset.
    JumpIfFalse = 91,
    /// Pop condition; if true, jump to target.
    /// Operand: u16 absolute target offset.
    JumpIfTrue = 92,

    // ── Assertions ─────────────────────────────────────────────
    /// Pop condition; if false, raise `AssertionFailed`.
    Assert = 100,

    // ── Pop ────────────────────────────────────────────────────
    /// Discard the top of the stack.
    Pop = 110,

    // ── Superinstructions ─────────────────────────────────────
    // Fused opcodes for common instruction sequences. Each saves
    // one dispatch cycle (opcode fetch + decode + branch).

    /// Fused `GetLocal` + `GetAttr`: push `stack[base+slot].key`.
    /// Operands: u16 local slot, u16 key constant index.
    /// Equivalent to: `GetLocal slot; GetAttr key_idx`.
    GetLocalAttr = 120,
    /// Fused `GetLocal` + `Call`: call `stack[base+slot]` with TOS as arg.
    /// Operand: u16 local slot.
    /// Equivalent to: `GetLocal slot; <arg already on stack>; Call`.
    /// Note: the argument must be on the stack before this instruction.
    GetLocalCall = 121,
}

impl OpCode {
    /// Convert a raw byte to an opcode.
    pub fn from_byte(byte: u8) -> Option<OpCode> {
        // Safety: we validate the byte matches a known variant.
        match byte {
            0 => Some(OpCode::Constant),
            1 => Some(OpCode::Null),
            2 => Some(OpCode::True),
            3 => Some(OpCode::False),
            10 => Some(OpCode::Add),
            11 => Some(OpCode::Sub),
            12 => Some(OpCode::Mul),
            13 => Some(OpCode::Div),
            14 => Some(OpCode::Negate),
            20 => Some(OpCode::Not),
            21 => Some(OpCode::And),
            22 => Some(OpCode::Or),
            23 => Some(OpCode::Implication),
            30 => Some(OpCode::Equal),
            31 => Some(OpCode::NotEqual),
            32 => Some(OpCode::Less),
            33 => Some(OpCode::Greater),
            34 => Some(OpCode::LessEqual),
            35 => Some(OpCode::GreaterEqual),
            40 => Some(OpCode::Interpolate),
            50 => Some(OpCode::GetLocal),
            51 => Some(OpCode::SetLocal),
            60 => Some(OpCode::MakeAttrs),
            61 => Some(OpCode::GetAttr),
            62 => Some(OpCode::HasAttr),
            63 => Some(OpCode::UpdateAttrs),
            64 => Some(OpCode::SelectOrDefault),
            70 => Some(OpCode::MakeList),
            71 => Some(OpCode::Concat),
            80 => Some(OpCode::MakeClosure),
            81 => Some(OpCode::Call),
            82 => Some(OpCode::Return),
            90 => Some(OpCode::Jump),
            91 => Some(OpCode::JumpIfFalse),
            92 => Some(OpCode::JumpIfTrue),
            100 => Some(OpCode::Assert),
            110 => Some(OpCode::Pop),
            120 => Some(OpCode::GetLocalAttr),
            121 => Some(OpCode::GetLocalCall),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_all_opcodes() {
        let opcodes = [
            OpCode::Constant,
            OpCode::Null,
            OpCode::True,
            OpCode::False,
            OpCode::Add,
            OpCode::Sub,
            OpCode::Mul,
            OpCode::Div,
            OpCode::Negate,
            OpCode::Not,
            OpCode::And,
            OpCode::Or,
            OpCode::Implication,
            OpCode::Equal,
            OpCode::NotEqual,
            OpCode::Less,
            OpCode::Greater,
            OpCode::LessEqual,
            OpCode::GreaterEqual,
            OpCode::Interpolate,
            OpCode::GetLocal,
            OpCode::SetLocal,
            OpCode::MakeAttrs,
            OpCode::GetAttr,
            OpCode::HasAttr,
            OpCode::UpdateAttrs,
            OpCode::SelectOrDefault,
            OpCode::MakeList,
            OpCode::Concat,
            OpCode::MakeClosure,
            OpCode::Call,
            OpCode::Return,
            OpCode::Jump,
            OpCode::JumpIfFalse,
            OpCode::JumpIfTrue,
            OpCode::Assert,
            OpCode::Pop,
            OpCode::GetLocalAttr,
            OpCode::GetLocalCall,
        ];
        for op in opcodes {
            let byte = op as u8;
            let decoded = OpCode::from_byte(byte)
                .unwrap_or_else(|| panic!("failed to decode opcode byte {byte} for {op:?}"));
            assert_eq!(decoded, op, "roundtrip failed for {op:?}");
        }
    }

    #[test]
    fn invalid_byte_returns_none() {
        assert!(OpCode::from_byte(255).is_none());
        assert!(OpCode::from_byte(200).is_none());
        assert!(OpCode::from_byte(5).is_none());
    }
}
