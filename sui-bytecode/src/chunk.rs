//! Bytecode container — holds instructions and a constant pool.
//!
//! A `Chunk` is the unit of compiled code. Each Nix expression compiles
//! to one top-level chunk; lambdas produce nested chunks stored in the
//! constant pool.

use crate::error::CompileError;
use crate::opcode::OpCode;
use crate::value::VMValue;

/// A compiled bytecode chunk.
///
/// Contains the instruction stream, a constant pool for literal values
/// and nested function chunks, and source line information for error
/// reporting.
#[derive(Clone, Default)]
pub struct Chunk {
    /// The raw bytecode instruction stream.
    pub code: Vec<u8>,
    /// Constant pool: literals, string keys, nested chunks.
    pub constants: Vec<VMValue>,
    /// Source line number for each byte in `code` (1:1 mapping).
    /// Used for error messages. Line 0 means "unknown".
    pub lines: Vec<u32>,
}

impl Chunk {
    /// Create an empty chunk.
    #[must_use]
    pub fn new() -> Self {
        Self {
            code: Vec::new(),
            constants: Vec::new(),
            lines: Vec::new(),
        }
    }

    /// Write a single byte to the instruction stream.
    pub fn write_byte(&mut self, byte: u8, line: u32) {
        self.code.push(byte);
        self.lines.push(line);
    }

    /// Write an opcode to the instruction stream.
    pub fn write_op(&mut self, op: OpCode, line: u32) {
        self.write_byte(op as u8, line);
    }

    /// Write a u16 operand as two little-endian bytes.
    pub fn write_u16(&mut self, value: u16, line: u32) {
        let bytes = value.to_le_bytes();
        self.write_byte(bytes[0], line);
        self.write_byte(bytes[1], line);
    }

    /// Add a constant to the pool and return its index.
    ///
    /// Returns an error if the pool exceeds `u16::MAX` entries.
    pub fn add_constant(&mut self, value: VMValue) -> Result<u16, CompileError> {
        if self.constants.len() >= u16::MAX as usize {
            return Err(CompileError::ConstantPoolOverflow);
        }
        let idx = self.constants.len() as u16;
        self.constants.push(value);
        Ok(idx)
    }

    /// Read a u16 operand from the bytecode at the given offset.
    ///
    /// Returns the value and the offset past the two bytes.
    #[must_use]
    pub fn read_u16(&self, offset: usize) -> u16 {
        u16::from_le_bytes([self.code[offset], self.code[offset + 1]])
    }

    /// Return the current length of the bytecode stream.
    #[must_use]
    pub fn len(&self) -> usize {
        self.code.len()
    }

    /// Whether the chunk contains no instructions.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.code.is_empty()
    }

    /// Patch a u16 value at the given offset in the bytecode stream.
    ///
    /// Used for back-patching jump targets after the target offset is known.
    pub fn patch_u16(&mut self, offset: usize, value: u16) {
        let bytes = value.to_le_bytes();
        self.code[offset] = bytes[0];
        self.code[offset + 1] = bytes[1];
    }
}

impl std::fmt::Debug for Chunk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Chunk ({} bytes, {} constants)", self.code.len(), self.constants.len())?;
        let mut offset = 0;
        while offset < self.code.len() {
            let byte = self.code[offset];
            let line = self.lines.get(offset).copied().unwrap_or(0);
            if let Some(op) = OpCode::from_byte(byte) {
                write!(f, "  {offset:04}  L{line:<4}  {op:?}")?;
                offset += 1;
                // Print inline operands for opcodes that have them.
                match op {
                    OpCode::Constant
                    | OpCode::GetLocal
                    | OpCode::SetLocal
                    | OpCode::GetAttr
                    | OpCode::HasAttr
                    | OpCode::SelectOrDefault
                    | OpCode::MakeAttrs
                    | OpCode::MakeList
                    | OpCode::Interpolate
                    | OpCode::MakeClosure
                    | OpCode::Jump
                    | OpCode::JumpIfFalse
                    | OpCode::JumpIfTrue
                    | OpCode::GetLocalCall => {
                        if offset + 1 < self.code.len() {
                            let operand = self.read_u16(offset);
                            write!(f, " {operand}")?;
                            offset += 2;
                        }
                    }
                    // Two u16 operands.
                    OpCode::GetLocalAttr => {
                        if offset + 3 < self.code.len() {
                            let slot = self.read_u16(offset);
                            let key = self.read_u16(offset + 2);
                            write!(f, " slot={slot} key={key}")?;
                            offset += 4;
                        }
                    }
                    _ => {}
                }
                writeln!(f)?;
            } else {
                writeln!(f, "  {offset:04}  L{line:<4}  <unknown {byte}>")?;
                offset += 1;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_and_read_constant() {
        let mut chunk = Chunk::new();
        let idx = chunk.add_constant(VMValue::Int(42)).unwrap();
        assert_eq!(idx, 0);
        assert_eq!(chunk.constants[0], VMValue::Int(42));
    }

    #[test]
    fn write_and_read_u16() {
        let mut chunk = Chunk::new();
        chunk.write_u16(0x1234, 1);
        assert_eq!(chunk.read_u16(0), 0x1234);
    }

    #[test]
    fn patch_u16() {
        let mut chunk = Chunk::new();
        // Write a placeholder.
        chunk.write_u16(0xFFFF, 1);
        // Patch it.
        chunk.patch_u16(0, 0x0042);
        assert_eq!(chunk.read_u16(0), 0x0042);
    }

    #[test]
    fn write_op_and_line_tracking() {
        let mut chunk = Chunk::new();
        chunk.write_op(OpCode::Null, 5);
        chunk.write_op(OpCode::Return, 5);
        assert_eq!(chunk.code.len(), 2);
        assert_eq!(chunk.lines.len(), 2);
        assert_eq!(chunk.lines[0], 5);
        assert_eq!(chunk.lines[1], 5);
    }

    #[test]
    fn debug_format() {
        let mut chunk = Chunk::new();
        chunk.write_op(OpCode::Null, 1);
        chunk.write_op(OpCode::Return, 1);
        let debug = format!("{chunk:?}");
        assert!(debug.contains("Null"));
        assert!(debug.contains("Return"));
    }
}
