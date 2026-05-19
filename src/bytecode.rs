/// Bytecode instruction set and code objects.
use crate::object::Value;

/// Opcode constants (upper 8 bits of a u32 instruction).
pub mod op {
    // Load/Store
    pub const LOAD_CONST: u8 = 0;
    pub const LOAD_FAST: u8 = 1;
    pub const STORE_FAST: u8 = 2;
    pub const LOAD_GLOBAL: u8 = 3;
    pub const STORE_GLOBAL: u8 = 4;
    pub const LOAD_DEREF: u8 = 5;
    pub const STORE_DEREF: u8 = 6;
    pub const LOAD_CLOSURE: u8 = 7;

    // Arithmetic
    pub const ADD: u8 = 10;
    pub const SUB: u8 = 11;
    pub const MUL: u8 = 12;
    pub const DIV: u8 = 13;
    pub const FLOOR_DIV: u8 = 14;
    pub const MOD: u8 = 15;
    pub const POW: u8 = 16;

    // Unary
    pub const UNARY_NEG: u8 = 20;
    pub const UNARY_NOT: u8 = 21;
    pub const UNARY_POS: u8 = 22;
    pub const UNARY_INVERT: u8 = 23;

    // Comparison
    pub const COMPARE_EQ: u8 = 30;
    pub const COMPARE_NE: u8 = 31;
    pub const COMPARE_LT: u8 = 32;
    pub const COMPARE_LE: u8 = 33;
    pub const COMPARE_GT: u8 = 34;
    pub const COMPARE_GE: u8 = 35;
    pub const COMPARE_IS: u8 = 36;
    pub const COMPARE_IS_NOT: u8 = 37;
    pub const CONTAINS_OP: u8 = 38;       // 'in' operator (operand: 0=in, 1=not in)

    // Jumps
    pub const JUMP: u8 = 40;
    pub const JUMP_IF_FALSE: u8 = 41;
    pub const JUMP_IF_TRUE: u8 = 42;

    // Functions
    pub const CALL_FUNCTION: u8 = 50;
    pub const RETURN_VALUE: u8 = 51;
    pub const MAKE_FUNCTION: u8 = 52;
    pub const MAKE_CLOSURE: u8 = 53;

    // Iteration
    pub const GET_ITER: u8 = 60;
    pub const FOR_ITER: u8 = 61;

    // Stack
    pub const POP_TOP: u8 = 70;
    pub const DUP_TOP: u8 = 71;
    pub const ROT_TWO: u8 = 72;
    pub const ROT_THREE: u8 = 73;

    // Collections
    pub const BUILD_LIST: u8 = 80;
    pub const LIST_APPEND: u8 = 81;
    pub const SUBSCRIPT: u8 = 82;
    pub const BUILD_TUPLE: u8 = 83;
    pub const BUILD_DICT: u8 = 84;
    pub const BUILD_SET: u8 = 85;

    // Attributes
    pub const LOAD_ATTR: u8 = 90;
    pub const STORE_ATTR: u8 = 91;
    pub const STORE_SUBSCRIPT: u8 = 92;
    pub const DELETE_SUBSCRIPT: u8 = 93;

    // Bitwise
    pub const BIT_AND: u8 = 100;
    pub const BIT_OR: u8 = 101;
    pub const BIT_XOR: u8 = 102;
    pub const LSHIFT: u8 = 103;
    pub const RSHIFT: u8 = 104;

    // Exceptions
    pub const SETUP_EXCEPT: u8 = 110;
    pub const POP_EXCEPT: u8 = 111;
    pub const RAISE: u8 = 112;
    pub const SETUP_FINALLY: u8 = 113;
    pub const END_FINALLY: u8 = 114;
    pub const LOAD_EXCEPTION: u8 = 115;

    // Classes
    pub const BUILD_CLASS: u8 = 120;

    // Generators
    pub const YIELD_VALUE: u8 = 130;

    // Unpacking
    pub const UNPACK_SEQUENCE: u8 = 140;

    pub const HALT: u8 = 255;
}

/// Encode an instruction: upper 8 bits opcode, lower 24 bits operand.
pub fn encode(opcode: u8, operand: u32) -> u32 {
    ((opcode as u32) << 24) | (operand & 0x00FF_FFFF)
}

/// Decode opcode from instruction.
pub fn decode_op(instr: u32) -> u8 {
    (instr >> 24) as u8
}

/// Decode operand from instruction.
pub fn decode_operand(instr: u32) -> u32 {
    instr & 0x00FF_FFFF
}

/// A compiled code object (one per function + one for module-level).
#[derive(Debug, Clone)]
pub struct CodeObject {
    pub name: String,
    pub instructions: Vec<u32>,
    pub constants: Vec<Value>,
    pub names: Vec<String>,
    pub local_names: Vec<String>,
    pub num_locals: usize,
    pub num_params: usize,
    pub line_table: Vec<u32>,
    /// Names of free variables (captured from enclosing scope).
    pub free_var_names: Vec<String>,
    /// Names of cell variables (captured by inner functions).
    pub cell_var_names: Vec<String>,
    /// Whether this function contains yield (is a generator).
    pub is_generator: bool,
}

impl CodeObject {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            instructions: Vec::new(),
            constants: Vec::new(),
            names: Vec::new(),
            local_names: Vec::new(),
            num_locals: 0,
            num_params: 0,
            line_table: Vec::new(),
            free_var_names: Vec::new(),
            cell_var_names: Vec::new(),
            is_generator: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode() {
        let instr = encode(op::LOAD_CONST, 42);
        assert_eq!(decode_op(instr), op::LOAD_CONST);
        assert_eq!(decode_operand(instr), 42);
    }

    #[test]
    fn max_operand() {
        let instr = encode(op::JUMP, 0x00FF_FFFF);
        assert_eq!(decode_operand(instr), 0x00FF_FFFF);
    }

    #[test]
    fn code_object_new() {
        let co = CodeObject::new("<module>");
        assert_eq!(co.name, "<module>");
        assert!(co.instructions.is_empty());
        assert!(!co.is_generator);
    }
}
