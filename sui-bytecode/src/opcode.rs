//! Bytecode instruction set for the Nix evaluator.
//!
//! A stack-based instruction set: operands are pushed/popped from the
//! value stack, and inline operands (constant indices, jump offsets,
//! counts) are encoded as 16-bit values following the opcode byte.
//!
//! ## One authored table — the `opcodes!` macro
//!
//! The entire instruction set is declared **once** in the `opcodes! { … }`
//! table below. From that single table the macro generates, by
//! construction:
//!
//! * the `#[repr(u8)]` `OpCode` enum (each variant pinned to its wire byte),
//! * [`OpCode::from_byte`] — the `u8 -> Option<OpCode>` decoder,
//! * [`OpCode::to_byte`] — the inverse `OpCode -> u8`,
//! * [`OpCode::ALL`] — the exhaustive variant list, and
//! * [`OpCode::disasm_operands`] — the disassembler's u16-operand arity
//!   (0/1/2) for each opcode.
//!
//! This kills the drift class the hand-transcribed instruction set carried:
//! previously the enum, a hand-written `from_byte` match, and a hand-written
//! roundtrip-test array each restated the byte↔variant map, and the test
//! array **silently passed** when a new variant was omitted (it only checked
//! what was listed). Now `OpCode::ALL` and the `disasm_operands` match are
//! generated from the same table as the enum — a new opcode is a single new
//! row, and the exhaustive roundtrip test (driven off `ALL`, not a parallel
//! array) cannot skip it. `disasm_operands` is a `match self { … }` over
//! every variant, so a missing operand-arity column is a **compile error**.

/// Declare the bytecode instruction set from one table.
///
/// Each row is `Variant = <byte>, operands = <n>;`, optionally preceded by
/// doc comments / attributes (which pass through onto the enum variant).
/// `<byte>` is the wire byte (`#[repr(u8)]` discriminant); `<n>` is the
/// number of inline **u16** operands the disassembler prints for that
/// opcode (0, 1, or 2) — a per-opcode property that was previously implicit
/// in a hand-written classification match in `chunk.rs`.
macro_rules! opcodes {
    (
        $(
            $(#[$vmeta:meta])*
            $name:ident = $byte:literal , operands = $operands:literal ;
        )*
    ) => {
        /// Bytecode instructions for the Nix VM.
        ///
        /// Each variant occupies exactly one byte (`#[repr(u8)]`). Inline
        /// operands (constant pool index, jump offset, element count) follow
        /// the opcode in the bytecode stream as 16-bit little-endian values.
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        #[repr(u8)]
        pub enum OpCode {
            $(
                $(#[$vmeta])*
                $name = $byte,
            )*
        }

        impl OpCode {
            /// Every opcode variant, in declaration order. Generated from the
            /// same table as the enum, so it can never omit a variant — the
            /// exhaustive roundtrip test drives off this list.
            pub const ALL: &'static [OpCode] = &[ $( OpCode::$name ),* ];

            /// Convert a raw byte to an opcode.
            pub fn from_byte(byte: u8) -> Option<OpCode> {
                match byte {
                    $( $byte => Some(OpCode::$name), )*
                    _ => None,
                }
            }

            /// The wire byte for this opcode (inverse of [`Self::from_byte`]).
            ///
            /// Equivalent to `self as u8`; provided as the named paired
            /// inverse so the round-trip is spelled out at call sites.
            #[must_use]
            pub fn to_byte(self) -> u8 {
                self as u8
            }

            /// Number of inline **u16** operands the disassembler prints for
            /// this opcode (0, 1, or 2). This is the arity the `Chunk` Debug
            /// formatter uses to advance past inline operands; it is an
            /// exhaustive `match self`, so adding an opcode without an
            /// operand-arity column is a compile error.
            #[must_use]
            pub fn disasm_operands(self) -> u8 {
                match self {
                    $( OpCode::$name => $operands, )*
                }
            }
        }
    };
}

opcodes! {
    // ── Constants ───────────────────────────────────────────────
    /// Push a constant from the constant pool.
    /// Operand: u16 constant index.
    Constant = 0, operands = 1;
    /// Push `null`.
    Null = 1, operands = 0;
    /// Push `true`.
    True = 2, operands = 0;
    /// Push `false`.
    False = 3, operands = 0;

    // ── Arithmetic ─────────────────────────────────────────────
    /// Pop two values, push their sum (int+int, float+float, int+float).
    Add = 10, operands = 0;
    /// Pop two values, push their difference.
    Sub = 11, operands = 0;
    /// Pop two values, push their product.
    Mul = 12, operands = 0;
    /// Pop two values, push their quotient. Errors on division by zero.
    Div = 13, operands = 0;
    /// Pop one value, push its arithmetic negation.
    Negate = 14, operands = 0;

    // ── Logical ────────────────────────────────────────────────
    /// Pop one bool, push its logical negation.
    Not = 20, operands = 0;
    /// Pop two bools, push logical AND (short-circuit handled at compile time).
    And = 21, operands = 0;
    /// Pop two bools, push logical OR (short-circuit handled at compile time).
    Or = 22, operands = 0;
    /// Pop two values, push `a -> b` (logical implication: `!a || b`).
    Implication = 23, operands = 0;

    // ── Comparison ─────────────────────────────────────────────
    /// Pop two values, push `true` if equal.
    Equal = 30, operands = 0;
    /// Pop two values, push `true` if not equal.
    NotEqual = 31, operands = 0;
    /// Pop two values, push `true` if left < right.
    Less = 32, operands = 0;
    /// Pop two values, push `true` if left > right.
    Greater = 33, operands = 0;
    /// Pop two values, push `true` if left <= right.
    LessEqual = 34, operands = 0;
    /// Pop two values, push `true` if left >= right.
    GreaterEqual = 35, operands = 0;

    // ── Strings ────────────────────────────────────────────────
    /// Pop N string parts, concatenate into one string.
    /// Operand: u16 part count.
    Interpolate = 40, operands = 1;

    // ── Variables ──────────────────────────────────────────────
    /// Push a local variable by stack slot index.
    /// Operand: u16 slot index (relative to current frame's stack base).
    GetLocal = 50, operands = 1;
    /// Set a local variable by stack slot index.
    /// Operand: u16 slot index.
    SetLocal = 51, operands = 1;
    /// Push an upvalue from the current closure's upvalue array.
    /// Operand: u8 upvalue index.
    GetUpvalue = 52, operands = 1;
    /// Set an upvalue in the current closure's upvalue array.
    /// Operand: u8 upvalue index.
    SetUpvalue = 53, operands = 1;

    // ── With scopes ────────────────────────────────────────────
    /// Push the TOS value onto the with-scope stack.
    PushWith = 54, operands = 0;
    /// Pop from the with-scope stack.
    PopWith = 55, operands = 0;
    /// Look up a name in the with-scope stack (innermost first).
    /// Operand: u16 constant index for the variable name string.
    LookupWith = 56, operands = 1;

    // ── Attribute sets ─────────────────────────────────────────
    /// Pop N key-value pairs (key on top, value below), construct attrset.
    /// Operand: u16 pair count.
    MakeAttrs = 60, operands = 1;
    /// Pop attrset and key (string constant index), push `attrset.key`.
    /// Operand: u16 constant index for the key name.
    GetAttr = 61, operands = 1;
    /// Pop attrset and key (string constant index), push bool.
    /// Operand: u16 constant index for the key name.
    HasAttr = 62, operands = 1;
    /// Pop two attrsets, push merged result (right overrides left, `//`).
    UpdateAttrs = 63, operands = 0;
    /// Pop attrset, key constant, and default value, push value or default.
    /// Stack order (top to bottom): default, attrset.
    /// Operand: u16 constant index for the key name.
    SelectOrDefault = 64, operands = 1;
    /// Dynamic attribute access: pop string key, pop attrset, push `attrset.key`.
    /// No inline operand — key comes from the stack at runtime.
    DynGetAttr = 65, operands = 0;
    /// Dynamic hasattr: pop string key, pop attrset, push bool.
    /// No inline operand — key comes from the stack at runtime.
    DynHasAttr = 66, operands = 0;
    /// Dynamic select-or-default: pop default, key, attrset; push value or default.
    /// Stack order (top to bottom): default, key (string), attrset.
    /// No inline operand — key comes from the stack at runtime.
    DynSelectOrDefault = 67, operands = 0;

    // ── Lists ──────────────────────────────────────────────────
    /// Pop N values, construct a list.
    /// Operand: u16 element count.
    MakeList = 70, operands = 1;
    /// Pop two lists, push concatenated result (`++`).
    Concat = 71, operands = 0;

    // ── Functions ──────────────────────────────────────────────
    /// Create a closure from a sub-chunk.
    /// Operand: u16 constant index pointing to the function's `Chunk`.
    /// Followed by u16 upvalue count, then for each upvalue:
    ///   u8 (1 = local, 0 = upvalue of enclosing), u16 index.
    MakeClosure = 80, operands = 1;
    /// Pop function and argument, call the function.
    Call = 81, operands = 0;
    /// Return from the current call frame.
    Return = 82, operands = 0;
    /// Pop function and argument, tail-call: reuse the current frame.
    /// Semantically identical to Call but does not grow the call stack.
    TailCall = 83, operands = 0;

    // ── Control flow ───────────────────────────────────────────
    /// Unconditional jump.
    /// Operand: u16 absolute target offset.
    Jump = 90, operands = 1;
    /// Pop condition; if false, jump to target.
    /// Operand: u16 absolute target offset.
    JumpIfFalse = 91, operands = 1;
    /// Pop condition; if true, jump to target.
    /// Operand: u16 absolute target offset.
    JumpIfTrue = 92, operands = 1;

    // ── Assertions & Throw ──────────────────────────────────────
    /// Pop condition; if false, raise `AssertionFailed`.
    Assert = 100, operands = 0;
    /// Pop a string from stack and raise `Throw(msg)`.
    /// Used for deferred search path errors caught by tryEval.
    Throw = 101, operands = 0;

    // ── Stack manipulation ─────────────────────────────────────
    /// Discard the top of the stack.
    Pop = 110, operands = 0;
    /// Duplicate the top of the stack.
    Dup = 111, operands = 0;

    // ── Superinstructions ─────────────────────────────────────
    /// Fused `GetLocal` + `GetAttr`: push `stack[base+slot].key`.
    /// Operands: u16 local slot, u16 key constant index.
    GetLocalAttr = 120, operands = 2;
    /// Fused `GetLocal` + `Call`: call `stack[base+slot]` with TOS as arg.
    /// Operand: u16 local slot.
    GetLocalCall = 121, operands = 1;

    // ── Builtins ─────────────────────────────────────────────────
    /// Push the `builtins` attribute set onto the stack.
    PushBuiltins = 130, operands = 1;
    /// Call a builtin function by index.
    /// Operand: u16 builtin index, u16 arg count.
    CallBuiltin = 131, operands = 2;

    // ── Thunks (lazy evaluation) ─────────────────────────────────
    /// Create a thunk wrapping a sub-chunk (lazy value).
    /// Operand: u16 constant index, u16 upvalue count,
    /// then for each upvalue: u8 is_local, u16 index.
    MakeThunk = 140, operands = 1;
    /// Force the top of stack: if it is a thunk, evaluate and replace.
    Force = 141, operands = 1;
    /// Patch upvalues of a thunk in a local slot.
    /// Operand: u16 slot, u16 upvalue count,
    /// then for each: u8 is_local, u16 index.
    PatchThunkUpvalues = 142, operands = 0;
    /// Create a lazy thunk from a source span (deferred compilation).
    /// Operand: u16 source_constant_idx, u32 offset, u32 length,
    ///          u16 base_dir_constant_idx, u16 upvalue count,
    ///          then for each upvalue: u8 is_local, u16 index.
    MakeLazyThunk = 143, operands = 0;

    // ── Import ───────────────────────────────────────────────────
    /// Pop a path from the stack, import the file, push the result.
    Import = 150, operands = 1;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The byte value pinned to every opcode variant, as of the pre-macro
    /// hand-written `#[repr(u8)]` enum. This table is the byte-identical
    /// oracle: the `opcodes!` table must reproduce every one of these bytes,
    /// or the VM's bytecode would silently change wire format. Do **not**
    /// "fix" a mismatch by editing this array — a diff here means a byte
    /// changed in the instruction set, which breaks every compiled program.
    const PINNED_BYTES: &[(OpCode, u8)] = &[
        (OpCode::Constant, 0),
        (OpCode::Null, 1),
        (OpCode::True, 2),
        (OpCode::False, 3),
        (OpCode::Add, 10),
        (OpCode::Sub, 11),
        (OpCode::Mul, 12),
        (OpCode::Div, 13),
        (OpCode::Negate, 14),
        (OpCode::Not, 20),
        (OpCode::And, 21),
        (OpCode::Or, 22),
        (OpCode::Implication, 23),
        (OpCode::Equal, 30),
        (OpCode::NotEqual, 31),
        (OpCode::Less, 32),
        (OpCode::Greater, 33),
        (OpCode::LessEqual, 34),
        (OpCode::GreaterEqual, 35),
        (OpCode::Interpolate, 40),
        (OpCode::GetLocal, 50),
        (OpCode::SetLocal, 51),
        (OpCode::GetUpvalue, 52),
        (OpCode::SetUpvalue, 53),
        (OpCode::PushWith, 54),
        (OpCode::PopWith, 55),
        (OpCode::LookupWith, 56),
        (OpCode::MakeAttrs, 60),
        (OpCode::GetAttr, 61),
        (OpCode::HasAttr, 62),
        (OpCode::UpdateAttrs, 63),
        (OpCode::SelectOrDefault, 64),
        (OpCode::DynGetAttr, 65),
        (OpCode::DynHasAttr, 66),
        (OpCode::DynSelectOrDefault, 67),
        (OpCode::MakeList, 70),
        (OpCode::Concat, 71),
        (OpCode::MakeClosure, 80),
        (OpCode::Call, 81),
        (OpCode::Return, 82),
        (OpCode::TailCall, 83),
        (OpCode::Jump, 90),
        (OpCode::JumpIfFalse, 91),
        (OpCode::JumpIfTrue, 92),
        (OpCode::Assert, 100),
        (OpCode::Throw, 101),
        (OpCode::Pop, 110),
        (OpCode::Dup, 111),
        (OpCode::GetLocalAttr, 120),
        (OpCode::GetLocalCall, 121),
        (OpCode::PushBuiltins, 130),
        (OpCode::CallBuiltin, 131),
        (OpCode::MakeThunk, 140),
        (OpCode::Force, 141),
        (OpCode::PatchThunkUpvalues, 142),
        (OpCode::MakeLazyThunk, 143),
        (OpCode::Import, 150),
    ];

    /// The disassembler u16-operand arity pinned to every opcode, as of the
    /// pre-macro hand-written classification match in `chunk.rs`. The
    /// `opcodes!` `operands = N` column must reproduce these exactly, or the
    /// `Chunk` Debug output changes.
    const PINNED_OPERANDS: &[(OpCode, u8)] = &[
        // one printed u16 (advance +2 in the disassembler)
        (OpCode::Constant, 1),
        (OpCode::GetLocal, 1),
        (OpCode::SetLocal, 1),
        (OpCode::GetUpvalue, 1),
        (OpCode::SetUpvalue, 1),
        (OpCode::LookupWith, 1),
        (OpCode::GetAttr, 1),
        (OpCode::HasAttr, 1),
        (OpCode::SelectOrDefault, 1),
        (OpCode::MakeAttrs, 1),
        (OpCode::MakeList, 1),
        (OpCode::Interpolate, 1),
        (OpCode::MakeClosure, 1),
        (OpCode::Jump, 1),
        (OpCode::JumpIfFalse, 1),
        (OpCode::JumpIfTrue, 1),
        (OpCode::GetLocalCall, 1),
        (OpCode::PushBuiltins, 1),
        (OpCode::MakeThunk, 1),
        (OpCode::Force, 1),
        (OpCode::Import, 1),
        // two printed u16 (advance +4)
        (OpCode::GetLocalAttr, 2),
        (OpCode::CallBuiltin, 2),
        // everything else: zero printed operands (the `_ => {}` arm)
        (OpCode::Null, 0),
        (OpCode::True, 0),
        (OpCode::False, 0),
        (OpCode::Add, 0),
        (OpCode::Sub, 0),
        (OpCode::Mul, 0),
        (OpCode::Div, 0),
        (OpCode::Negate, 0),
        (OpCode::Not, 0),
        (OpCode::And, 0),
        (OpCode::Or, 0),
        (OpCode::Implication, 0),
        (OpCode::Equal, 0),
        (OpCode::NotEqual, 0),
        (OpCode::Less, 0),
        (OpCode::Greater, 0),
        (OpCode::LessEqual, 0),
        (OpCode::GreaterEqual, 0),
        (OpCode::PushWith, 0),
        (OpCode::PopWith, 0),
        (OpCode::UpdateAttrs, 0),
        (OpCode::DynGetAttr, 0),
        (OpCode::DynHasAttr, 0),
        (OpCode::DynSelectOrDefault, 0),
        (OpCode::Concat, 0),
        (OpCode::Call, 0),
        (OpCode::Return, 0),
        (OpCode::TailCall, 0),
        (OpCode::Assert, 0),
        (OpCode::Throw, 0),
        (OpCode::Pop, 0),
        (OpCode::Dup, 0),
        (OpCode::PatchThunkUpvalues, 0),
        (OpCode::MakeLazyThunk, 0),
    ];

    /// The generated `OpCode::ALL` covers exactly 57 opcodes — the full
    /// instruction set. A count check turns "the macro dropped a row" into a
    /// failing test rather than a silent gap.
    #[test]
    fn all_covers_full_instruction_set() {
        assert_eq!(OpCode::ALL.len(), 57, "OpCode::ALL must list every opcode");
    }

    /// Every opcode's byte is byte-identical to the pre-macro hand table.
    #[test]
    fn opcode_bytes_are_byte_identical() {
        assert_eq!(
            PINNED_BYTES.len(),
            OpCode::ALL.len(),
            "PINNED_BYTES must pin every opcode"
        );
        for &(op, byte) in PINNED_BYTES {
            assert_eq!(op.to_byte(), byte, "byte for {op:?} changed");
            assert_eq!(op as u8, byte, "`as u8` for {op:?} changed");
            assert_eq!(
                OpCode::from_byte(byte),
                Some(op),
                "from_byte({byte}) must decode to {op:?}"
            );
        }
    }

    /// Exhaustive round-trip: every opcode in the generated `ALL` list
    /// decodes back to itself. Because `ALL` is generated from the same
    /// table as the enum (not a parallel hand-kept array), a new variant
    /// cannot be omitted here — closing the silent-pass hole the old
    /// `roundtrip_all_opcodes` array had.
    #[test]
    fn roundtrip_all_opcodes() {
        for &op in OpCode::ALL {
            let byte = op.to_byte();
            let decoded = OpCode::from_byte(byte)
                .unwrap_or_else(|| panic!("failed to decode opcode byte {byte} for {op:?}"));
            assert_eq!(decoded, op, "roundtrip failed for {op:?}");
        }
    }

    /// The disassembler operand-arity column is byte-identical to the
    /// pre-macro `chunk.rs` classification (0/1/2 printed u16 operands).
    #[test]
    fn disasm_operands_are_byte_identical() {
        assert_eq!(
            PINNED_OPERANDS.len(),
            OpCode::ALL.len(),
            "PINNED_OPERANDS must pin every opcode"
        );
        for &(op, ar) in PINNED_OPERANDS {
            assert_eq!(op.disasm_operands(), ar, "operand arity for {op:?} changed");
        }
    }

    #[test]
    fn invalid_byte_returns_none() {
        assert!(OpCode::from_byte(255).is_none());
        assert!(OpCode::from_byte(200).is_none());
        assert!(OpCode::from_byte(5).is_none());
    }
}
