use std::rc::Rc;

use super::contract::{AnalysisData, ValidJumpAddress};
use crate::{opcode, spec_opcode_gas, Spec, KECCAK_EMPTY};
use bytes::Bytes;
use primitive_types::H256;
use sha3::{Digest, Keccak256};

#[derive(Clone, Debug)]
pub enum BytecodeState {
    Raw,
    Checked {
        len: usize,
    },
    Analysed {
        len: usize,
        jumptable: ValidJumpAddress,
    },
}

#[derive(Clone, Debug)]
pub struct Bytecode {
    bytecode: Bytes,
    state: BytecodeState,
}

impl Default for Bytecode {
    fn default() -> Self {
        Bytecode::new()
    }
}

impl Bytecode {
    pub fn new() -> Self {
        // bytecode with one STOP opcode
        Bytecode {
            bytecode: vec![0].into(),
            state: BytecodeState::Analysed {
                len: 0,
                jumptable: ValidJumpAddress::new(Rc::new(Vec::new()), 0),
            },
        }
    }

    pub fn new_raw(bytecode: Bytes) -> Self {
        Self {
            bytecode,
            state: BytecodeState::Raw,
        }
    }

    /// Safety: Bytecode need to end with STOP (0x00) opcode as checked bytecode assumes that
    /// that it is safe to iterate over bytecode without checking lengths
    pub unsafe fn new_checked(bytecode: Bytes, len: usize) -> Self {
        Self {
            bytecode,
            state: BytecodeState::Checked { len },
        }
    }

    /// Safety: Same as new_checked, bytecode needs to end with STOP (0x00) opcode as checked bytecode assumes
    /// that it is safe to iterate over bytecode without checking length
    pub unsafe fn new_analysed(bytecode: Bytes, len: usize, jumptable: ValidJumpAddress) -> Self {
        Self {
            bytecode,
            state: BytecodeState::Analysed { len, jumptable },
        }
    }

    pub fn bytes(&self) -> &Bytes {
        &self.bytecode
    }

    pub fn hash(&self) -> H256 {
        let to_hash = match self.state {
            BytecodeState::Raw => self.bytecode.as_ref(),
            BytecodeState::Checked { len } => &self.bytecode[..len],
            BytecodeState::Analysed { len, .. } => &self.bytecode[..len],
        };
        if to_hash.is_empty() {
            KECCAK_EMPTY
        } else {
            H256::from_slice(Keccak256::digest(&to_hash).as_slice())
        }
    }

    pub fn is_empty(&self) -> bool {
        match self.state {
            BytecodeState::Raw => self.bytecode.is_empty(),
            BytecodeState::Checked { len } => len == 0,
            BytecodeState::Analysed { len, .. } => len == 0,
        }
    }

    pub fn len(&self) -> usize {
        match self.state {
            BytecodeState::Raw => self.bytecode.len(),
            BytecodeState::Checked { len, .. } => len,
            BytecodeState::Analysed { len, .. } => len,
        }
    }

    pub fn to_checked(self) -> Self {
        match self.state {
            BytecodeState::Raw => {
                let len = self.bytecode.len();
                let mut bytecode: Vec<u8> = Vec::from(self.bytecode.as_ref());
                bytecode.resize(len + 33, 0);
                Self {
                    bytecode: bytecode.into(),
                    state: BytecodeState::Checked { len },
                }
            }
            _ => self,
        }
    }

    pub fn to_analysed<SPEC: Spec>(self) -> Self {
        let (bytecode, len) = match self.state {
            BytecodeState::Raw => {
                let len = self.bytecode.len();
                let checked = self.to_checked();
                (checked.bytecode.into(), len)
            }
            BytecodeState::Checked { len } => (self.bytecode, len),
            _ => return self,
        };
        let jumptable = Self::analyze::<SPEC>(bytecode.as_ref());

        Self {
            bytecode: bytecode.into(),
            state: BytecodeState::Analysed { len, jumptable },
        }
    }

    pub fn lock<SPEC: Spec>(self) -> BytecodeLocked {
        let Bytecode { bytecode, state } = self.to_analysed::<SPEC>();
        if let BytecodeState::Analysed { len, jumptable } = state {
            BytecodeLocked {
                bytecode,
                len,
                jumptable,
            }
        } else {
            unreachable!("to_analysed transforms state to analysed");
        }
    }

    /// Analyze bytecode to get jumptable and gas blocks.
    fn analyze<SPEC: Spec>(code: &[u8]) -> ValidJumpAddress {
        let opcode_gas = spec_opcode_gas(SPEC::SPEC_ID);

        let mut analysis = ValidJumpAddress {
            first_gas_block: 0,
            analysis: Rc::new(vec![AnalysisData::none(); code.len()]),
        };
        let jumps = Rc::get_mut(&mut analysis.analysis).unwrap();

        let mut index = 0;
        let mut gas_in_block: u32 = 0;
        let mut block_start: usize = 0;

        // first gas block
        while index < code.len() {
            let opcode = unsafe { *code.get_unchecked(index) };
            let info = unsafe { opcode_gas.get_unchecked(opcode as usize) };
            analysis.first_gas_block += info.get_gas();

            index += if info.is_push() {
                ((opcode - opcode::PUSH1) + 2) as usize
            } else {
                1
            };

            if info.is_gas_block_end() {
                block_start = index - 1;
                if info.is_jump() {
                    unsafe {
                        jumps.get_unchecked_mut(block_start).set_is_jump();
                    }
                }
                break;
            }
        }

        while index < code.len() {
            let opcode = unsafe { *code.get_unchecked(index) };
            let info = unsafe { opcode_gas.get_unchecked(opcode as usize) };
            gas_in_block += info.get_gas();

            if info.is_gas_block_end() {
                if info.is_jump() {
                    unsafe {
                        jumps.get_unchecked_mut(index).set_is_jump();
                    }
                }
                unsafe {
                    jumps.get_unchecked_mut(block_start).set_gas_block(gas_in_block);
                }
                block_start = index;
                gas_in_block = 0;
                index += 1;
            } else {
                index += if info.is_push() {
                    ((opcode - opcode::PUSH1) + 2) as usize
                } else {
                    1
                };
            }
        }
        if gas_in_block != 0 {
            unsafe {
                jumps.get_unchecked_mut(block_start).set_gas_block(gas_in_block);
            }
        }
        analysis
    }
}

pub struct BytecodeLocked {
    bytecode: Bytes,
    len: usize,
    jumptable: ValidJumpAddress,
}

impl BytecodeLocked {
    pub fn as_ptr(&self) -> *const u8 {
        self.bytecode.as_ptr()
    }
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn unlock(self) -> Bytecode {
        Bytecode {
            bytecode: self.bytecode,
            state: BytecodeState::Analysed {
                len: self.len,
                jumptable: self.jumptable,
            },
        }
    }
    pub fn bytecode(&self) -> &[u8] {
        self.bytecode.as_ref()
    }

    pub fn original_bytecode_slice(&self) -> &[u8] {
        &self.bytecode.as_ref()[..self.len]
    }

    pub fn jumptable(&self) -> &ValidJumpAddress {
        &self.jumptable
    }
}
