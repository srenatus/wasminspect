use crate::address::{DataAddr, ElemAddr, FuncAddr, GlobalAddr, MemoryAddr, TableAddr};
use crate::config::Config;
use crate::func::*;
use crate::inst::{Instruction, InstructionKind};
use crate::interceptor::Interceptor;
use crate::memory::MemoryInstance;
use crate::module::*;
use crate::stack::{CallFrame, Label, ProgramCounter, Stack, StackValue};
use crate::store::*;
use crate::value::{Copysign, Nearest, RefType, RefVal, TruncSat, TruncTo};
use crate::value::{
    ExtendInto, FromLittleEndian, IntoLittleEndian, NativeValue, Value, F32, F64, I32, I64, U32,
    U64,
};
use crate::{data, elem, memory, stack, table, value};
use wasmparser::{FuncType, Type, TypeOrFuncType};

use std::convert::TryInto;
use std::{ops::*, usize};

#[derive(Debug)]
pub enum Trap {
    Unreachable,
    Memory(memory::Error),
    Stack(stack::Error),
    Table(table::Error),
    Value(value::Error),
    Element(elem::Error),
    Data(data::Error),
    IndirectCallTypeMismatch {
        callee_name: String,
        expected: FuncType,
        actual: FuncType,
    },
    DirectCallTypeMismatch {
        callee_name: String,
        expected: Vec<Type>,
        actual: Vec<Type>,
    },
    UnexpectedStackValueType {
        expected: Type,
        actual: Type,
    },
    UnexpectedNonRefValueType {
        actual: Type,
    },
    UndefinedFunc(usize),
    ElementTypeMismatch {
        expected: RefType,
        actual: RefVal,
    },
    NoMoreInstruction,
    HostFunctionError(Box<dyn std::error::Error + Send + Sync>),
    MemoryAddrOverflow {
        base: u32,
        offset: u64,
    },
}

impl std::error::Error for Trap {}

impl std::fmt::Display for Trap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Memory(e) => write!(f, "{}", e),
            Self::Value(e) => write!(f, "{}", e),
            Self::Table(e) => write!(f, "{}", e),
            Self::Stack(e) => write!(f, "{}", e),
            Self::Element(e) => write!(f, "{}", e),
            Self::Data(e) => write!(f, "{}", e),
            Self::IndirectCallTypeMismatch {
                callee_name,
                expected,
                actual,
            } => write!(
                f,
                "indirect call type mismatch for '{}':
 >> call_indirect instruction expected {:?}
 >> but actual implementation has      {:?}",
                callee_name, expected, actual
            ),
            Self::UndefinedFunc(addr) => write!(f, "uninitialized element {:?}", addr),
            Self::Unreachable => write!(f, "unreachable"),
            Self::MemoryAddrOverflow { base, offset } => write!(
                f,
                "out of bounds memory access: memory address overflow (base: {}, offset: {})",
                base, offset
            ),
            _ => write!(f, "{:?}", self),
        }
    }
}

impl From<table::Error> for Trap {
    fn from(e: table::Error) -> Self {
        Trap::Table(e)
    }
}

impl From<elem::Error> for Trap {
    fn from(e: elem::Error) -> Self {
        Trap::Element(e)
    }
}

impl From<memory::Error> for Trap {
    fn from(e: memory::Error) -> Self {
        Trap::Memory(e)
    }
}

impl From<data::Error> for Trap {
    fn from(e: data::Error) -> Self {
        Trap::Data(e)
    }
}

pub enum Signal {
    Next,
    Breakpoint,
    End,
}

pub type ExecResult<T> = std::result::Result<T, Trap>;

#[derive(Debug)]
pub enum ReturnValError {
    TypeMismatchReturnValue(Value, Type),
    Stack(stack::Error),
    NoValue(Type),
}

pub type ReturnValResult = Result<Vec<Value>, ReturnValError>;

impl std::error::Error for ReturnValError {}

impl std::fmt::Display for ReturnValError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

pub struct Executor {
    pub pc: ProgramCounter,
    pub stack: Stack,
}

impl Executor {
    pub fn new(initial_frame: CallFrame, initial_arity: usize, pc: ProgramCounter) -> Self {
        let mut stack = Stack::default();
        let _ = stack.set_frame(initial_frame);
        stack.push_label(Label::Return {
            arity: initial_arity,
        });
        Self { pc, stack }
    }

    pub fn pop_result(&mut self, return_ty: Vec<Type>) -> ReturnValResult {
        let mut results = vec![];
        for ty in return_ty.into_iter().rev() {
            let val = self.stack.pop_value().map_err(ReturnValError::Stack)?;
            results.push(val);
            if !val.isa(ty) {
                return Err(ReturnValError::TypeMismatchReturnValue(val, ty));
            }
        }
        Ok(results.into_iter().rev().collect())
    }

    pub fn current_func_insts<'a>(&self, store: &'a Store) -> ExecResult<&'a [Instruction]> {
        let func = store.func_global(self.pc.exec_addr());
        Ok(func.defined().unwrap().instructions())
    }

    pub fn execute_step<I: Interceptor>(
        &mut self,
        store: &Store,
        interceptor: &I,
        config: &Config,
    ) -> ExecResult<Signal> {
        let func = store.func_global(self.pc.exec_addr()).defined().unwrap();
        let module_index = func.module_index();
        let inst = match func.inst(self.pc.inst_index()) {
            Some(inst) => inst,
            None => return Err(Trap::NoMoreInstruction),
        };

        let signal = interceptor.execute_inst(inst)?;
        let result = self.execute_inst(inst, module_index, store, interceptor, config)?;
        Ok(match (signal, result) {
            (_, Signal::End) => Signal::End,
            (signal, Signal::Next) => signal,
            (_, other) => other,
        })
    }

    fn execute_inst<I: Interceptor>(
        &mut self,
        inst: &Instruction,
        module_index: ModuleIndex,
        store: &Store,
        interceptor: &I,
        config: &Config,
    ) -> ExecResult<Signal> {
        self.pc.inc_inst_index();
        let result = match &inst.kind {
            InstructionKind::Unreachable => Err(Trap::Unreachable),
            InstructionKind::Nop => Ok(Signal::Next),
            InstructionKind::Block { ty } => {
                let (params_size, results_size) = self.get_type_arity(ty, store)?;
                let params = self.stack.pop_values(params_size).map_err(Trap::Stack)?;
                self.stack.push_label(Label::Block {
                    arity: results_size,
                });
                self.stack.push_values(params.into_iter().rev());
                Ok(Signal::Next)
            }
            InstructionKind::Loop { ty } => {
                let start_loop = InstIndex(self.pc.inst_index().0 - 1);
                let (params_size, _) = self.get_type_arity(ty, store)?;
                let params = self.stack.pop_values(params_size).map_err(Trap::Stack)?;
                self.stack
                    .push_label(Label::new_loop(start_loop, params_size));
                self.stack.push_values(params.into_iter().rev());
                Ok(Signal::Next)
            }
            InstructionKind::If { ty } => {
                let val: i32 = self.pop_as()?;
                let (params_size, results_size) = self.get_type_arity(ty, store)?;
                let params = self.stack.pop_values(params_size).map_err(Trap::Stack)?;
                self.stack.push_label(Label::If {
                    arity: results_size,
                });
                self.stack.push_values(params.into_iter().rev());
                if val == 0 {
                    let mut depth = 1;
                    loop {
                        let index = self.pc.inst_index().0 as usize;
                        match self.current_func_insts(store)?[index].kind {
                            InstructionKind::End => depth -= 1,
                            InstructionKind::Block { ty: _ } => depth += 1,
                            InstructionKind::If { ty: _ } => depth += 1,
                            InstructionKind::Loop { ty: _ } => depth += 1,
                            InstructionKind::Else => {
                                if depth == 1 {
                                    self.pc.inc_inst_index();
                                    break;
                                }
                            }
                            _ => (),
                        }
                        if depth == 0 {
                            break;
                        }
                        self.pc.inc_inst_index();
                    }
                }
                Ok(Signal::Next)
            }
            InstructionKind::Else => self.branch(0, store),
            InstructionKind::End => {
                if self.stack.is_func_top_level().map_err(Trap::Stack)? {
                    // When the end of a function is reached without a jump
                    let ret_pc = self.stack.current_frame().map_err(Trap::Stack)?.ret_pc;
                    let func = store.func_global(self.pc.exec_addr());
                    let results = self
                        .stack
                        .pop_values(func.ty().returns.len())
                        .map_err(Trap::Stack)?;
                    self.stack.pop_label().map_err(Trap::Stack)?;
                    self.stack.pop_frame().map_err(Trap::Stack)?;
                    self.stack.push_values(results.into_iter().rev());
                    if let Some(ret_pc) = ret_pc {
                        self.pc = ret_pc;
                        Ok(Signal::Next)
                    } else {
                        Ok(Signal::End)
                    }
                } else {
                    // When the end of a block is reached without a jump
                    let results = self.stack.pop_while(|v| matches!(v, StackValue::Value(_)));
                    self.stack.pop_label().map_err(Trap::Stack)?;
                    let results = results
                        .into_iter()
                        .rev()
                        .map(|v| v.into_value().map_err(Trap::Stack))
                        .collect::<ExecResult<Vec<_>>>()?;
                    self.stack.push_values(results);
                    Ok(Signal::Next)
                }
            }
            InstructionKind::Br { relative_depth } => self.branch(*relative_depth, store),
            InstructionKind::BrIf { relative_depth } => {
                let val = self.stack.pop_value().map_err(Trap::Stack)?;
                if val != Value::I32(0) {
                    self.branch(*relative_depth, store)
                } else {
                    Ok(Signal::Next)
                }
            }
            InstructionKind::BrTable { table: payload } => {
                let val: i32 = self.pop_as()?;
                let val = val as usize;
                let depth = if val < payload.table.len() {
                    payload.table[val]
                } else {
                    payload.default
                };
                self.branch(depth, store)
            }
            InstructionKind::Return => self.do_return(store),
            InstructionKind::Call { function_index } => {
                let frame = self.stack.current_frame().map_err(Trap::Stack)?;
                let addr = FuncAddr::new_unsafe(frame.module_index(), *function_index as usize);
                self.invoke(addr, store, interceptor)
            }
            InstructionKind::CallIndirect { index, table_index } => {
                let frame = self.stack.current_frame().map_err(Trap::Stack)?;
                let addr = TableAddr::new_unsafe(frame.module_index(), *table_index as usize);
                let module = store.module(frame.module_index()).defined().unwrap();
                let ty = module.get_type(*index as usize);
                let buf_index: i32 = self.pop_as()?;
                let table = store.table(addr);
                let buf_index = buf_index as usize;
                let func_ref = table.borrow().get_at(buf_index).map_err(Trap::Table)?;

                let func_addr = match func_ref {
                    RefVal::NullRef(_) => Err(Trap::UndefinedFunc(buf_index)),
                    RefVal::FuncRef(addr) => Ok(addr),
                    other => Err(Trap::ElementTypeMismatch {
                        expected: RefType::FuncRef,
                        actual: other,
                    }),
                }?;
                let (func, _) = store
                    .func(func_addr)
                    .ok_or(Trap::UndefinedFunc(func_addr.1))?;
                if func.ty() == ty {
                    self.invoke(func_addr, store, interceptor)
                } else {
                    Err(Trap::IndirectCallTypeMismatch {
                        callee_name: func.name().clone(),
                        expected: ty.clone(),
                        actual: func.ty().clone(),
                    })
                }
            }
            InstructionKind::Drop => {
                self.stack.pop_value().map_err(Trap::Stack)?;
                Ok(Signal::Next)
            }
            InstructionKind::Select | InstructionKind::TypedSelect { .. } => {
                let cond: i32 = self.pop_as()?;
                let val2 = self.stack.pop_value().map_err(Trap::Stack)?;
                let val1 = self.stack.pop_value().map_err(Trap::Stack)?;
                if cond != 0 {
                    self.stack.push_value(val1);
                } else {
                    self.stack.push_value(val2);
                }
                Ok(Signal::Next)
            }
            InstructionKind::LocalGet { local_index } => {
                let value = self
                    .stack
                    .current_frame()
                    .map_err(Trap::Stack)?
                    .local(*local_index as usize);
                self.stack.push_value(value);
                Ok(Signal::Next)
            }
            InstructionKind::LocalSet { local_index } => self.set_local(*local_index as usize),
            InstructionKind::LocalTee { local_index } => {
                let val = self.stack.pop_value().map_err(Trap::Stack)?;
                self.stack.push_value(val);
                self.stack.push_value(val);
                self.set_local(*local_index as usize)
            }
            InstructionKind::GlobalGet { global_index } => {
                let addr = GlobalAddr::new_unsafe(module_index, *global_index as usize);
                let global = store.global(addr);
                self.stack.push_value(global.borrow().value());
                Ok(Signal::Next)
            }
            InstructionKind::GlobalSet { global_index } => {
                let addr = GlobalAddr::new_unsafe(module_index, *global_index as usize);
                let value = self.stack.pop_value().map_err(Trap::Stack)?;
                let global = store.global(addr);
                global.borrow_mut().set_value(value);
                Ok(Signal::Next)
            }
            InstructionKind::TableGet { table } => {
                let addr = TableAddr::new_unsafe(module_index, *table as usize);
                let table = store.table(addr);
                let index: i32 = self.pop_as()?;
                let val = table.borrow().get_at(index as usize)?;
                self.stack.push_value(Value::Ref(val));
                Ok(Signal::Next)
            }
            InstructionKind::TableSet { table } => {
                let addr = TableAddr::new_unsafe(module_index, *table as usize);
                let table = store.table(addr);
                let ref_val = self.pop_ref()?;
                let index: i32 = self.pop_as()?;
                table.borrow_mut().set_at(index as usize, ref_val)?;
                Ok(Signal::Next)
            }
            InstructionKind::TableSize { table } => {
                let addr = TableAddr::new_unsafe(module_index, *table as usize);
                let table = store.table(addr);
                let sz = table.borrow().buffer_len();
                self.stack.push_value(Value::I32(sz as i32));
                Ok(Signal::Next)
            }

            InstructionKind::TableGrow { table } => {
                let addr = TableAddr::new_unsafe(module_index, *table as usize);
                let table = store.table(addr);
                let sz = table.borrow().buffer_len();
                let n: i32 = self.pop_as()?;
                let ref_val = self.pop_ref()?;
                let ret_val = match table.borrow_mut().grow(n as usize, ref_val) {
                    Ok(_) => sz as i32,
                    Err(_) => -1,
                };
                self.stack.push_value(Value::I32(ret_val));
                Ok(Signal::Next)
            }

            InstructionKind::TableFill { table } => {
                let addr = TableAddr::new_unsafe(module_index, *table as usize);
                let table = store.table(addr);
                let n = self.pop_as::<i32>()? as usize;
                let ref_val = self.pop_ref()?;
                let index = self.pop_as::<i32>()? as usize;

                table.borrow().validate_region(index, n)?;

                for index in index..(index + n) {
                    table.borrow_mut().set_at(index, ref_val)?;
                }

                Ok(Signal::Next)
            }

            InstructionKind::TableCopy {
                dst_table,
                src_table,
            } => {
                let dst_addr = TableAddr::new_unsafe(module_index, *dst_table as usize);
                let dst_table = store.table(dst_addr);
                let src_addr = TableAddr::new_unsafe(module_index, *src_table as usize);
                let src_table = store.table(src_addr);
                let n = self.pop_as::<i32>()? as usize;
                let src_base = self.pop_as::<i32>()? as usize;
                let dst_base = self.pop_as::<i32>()? as usize;

                let values = (0..n)
                    .map(|offset| -> ExecResult<_> {
                        Ok(src_table.borrow().get_at(src_base + offset)?)
                    })
                    .collect::<ExecResult<Vec<_>>>()?;
                src_table.borrow().validate_region(src_base, n)?;
                dst_table.borrow().validate_region(dst_base, n)?;
                for (offset, val) in values.into_iter().enumerate().take(n) {
                    dst_table.borrow_mut().set_at(dst_base + offset, val)?;
                }

                Ok(Signal::Next)
            }

            InstructionKind::TableInit { segment, table } => {
                let table_addr = TableAddr::new_unsafe(module_index, *table as usize);
                let elem_addr = ElemAddr::new_unsafe(module_index, *segment as usize);
                let table = store.table(table_addr);
                let elem = store.elem(elem_addr);
                let n = self.pop_as::<i32>()? as usize;
                let src_base = self.pop_as::<i32>()? as usize;
                let dst_base = self.pop_as::<i32>()? as usize;

                table.borrow().validate_region(dst_base, n)?;
                elem.borrow().validate_region(src_base, n)?;

                for offset in 0..n {
                    let val = elem.borrow().get_at(src_base + offset)?;
                    table.borrow_mut().set_at(dst_base + offset, val)?;
                }
                Ok(Signal::Next)
            }
            InstructionKind::ElemDrop { segment } => {
                let elem_addr = ElemAddr::new_unsafe(module_index, *segment as usize);
                let elem = store.elem(elem_addr);
                elem.borrow_mut().drop_elem();
                Ok(Signal::Next)
            }

            InstructionKind::I32Load { memarg } => self.load::<i32>(memarg.offset, store, config),
            InstructionKind::I64Load { memarg } => self.load::<i64>(memarg.offset, store, config),
            InstructionKind::F32Load { memarg } => self.load::<F32>(memarg.offset, store, config),
            InstructionKind::F64Load { memarg } => self.load::<F64>(memarg.offset, store, config),

            InstructionKind::I32Load8S { memarg } => {
                self.load_extend::<i8, i32>(memarg.offset, store, config)
            }
            InstructionKind::I32Load8U { memarg } => {
                self.load_extend::<u8, i32>(memarg.offset, store, config)
            }
            InstructionKind::I32Load16S { memarg } => {
                self.load_extend::<i16, i32>(memarg.offset, store, config)
            }
            InstructionKind::I32Load16U { memarg } => {
                self.load_extend::<u16, i32>(memarg.offset, store, config)
            }

            InstructionKind::I64Load8S { memarg } => {
                self.load_extend::<i8, i64>(memarg.offset, store, config)
            }
            InstructionKind::I64Load8U { memarg } => {
                self.load_extend::<u8, i64>(memarg.offset, store, config)
            }
            InstructionKind::I64Load16S { memarg } => {
                self.load_extend::<i16, i64>(memarg.offset, store, config)
            }
            InstructionKind::I64Load16U { memarg } => {
                self.load_extend::<u16, i64>(memarg.offset, store, config)
            }
            InstructionKind::I64Load32S { memarg } => {
                self.load_extend::<i32, i64>(memarg.offset, store, config)
            }
            InstructionKind::I64Load32U { memarg } => {
                self.load_extend::<u32, i64>(memarg.offset, store, config)
            }

            InstructionKind::I32Store { memarg } => {
                self.store::<i32, _>(memarg.offset, store, interceptor, config)
            }
            InstructionKind::I64Store { memarg } => {
                self.store::<i64, _>(memarg.offset, store, interceptor, config)
            }
            InstructionKind::F32Store { memarg } => {
                self.store::<F32, _>(memarg.offset, store, interceptor, config)
            }
            InstructionKind::F64Store { memarg } => {
                self.store::<F64, _>(memarg.offset, store, interceptor, config)
            }

            InstructionKind::I32Store8 { memarg } => {
                self.store_with_width::<i32, _>(memarg.offset, 1, store, interceptor, config)
            }
            InstructionKind::I32Store16 { memarg } => {
                self.store_with_width::<i32, _>(memarg.offset, 2, store, interceptor, config)
            }
            InstructionKind::I64Store8 { memarg } => {
                self.store_with_width::<i64, _>(memarg.offset, 1, store, interceptor, config)
            }
            InstructionKind::I64Store16 { memarg } => {
                self.store_with_width::<i64, _>(memarg.offset, 2, store, interceptor, config)
            }
            InstructionKind::I64Store32 { memarg } => {
                self.store_with_width::<i64, _>(memarg.offset, 4, store, interceptor, config)
            }

            InstructionKind::MemorySize { .. } => {
                self.stack
                    .push_value(Value::I32(self.memory(store)?.borrow().page_count() as i32));
                Ok(Signal::Next)
            }
            InstructionKind::MemoryGrow { .. } => {
                let grow_page: i32 = self.pop_as()?;
                let mem = self.memory(store)?;
                let size = mem.borrow().page_count();
                match mem.borrow_mut().grow(grow_page as usize) {
                    Ok(_) => {
                        self.stack.push_value(Value::I32(size as i32));
                    }
                    Err(err) => {
                        println!("[Debug] Failed to grow memory {:?}", err);
                        self.stack.push_value(Value::I32(-1));
                    }
                }
                Ok(Signal::Next)
            }
            InstructionKind::MemoryCopy { src, dst } => {
                let dst_addr = MemoryAddr::new_unsafe(module_index, *dst as usize);
                let dst_mem = store.memory(dst_addr);
                let src_addr = MemoryAddr::new_unsafe(module_index, *src as usize);
                let src_mem = store.memory(src_addr);
                let n = self.pop_as::<i32>()? as usize;
                let src_base = self.pop_as::<i32>()? as usize;
                let dst_base = self.pop_as::<i32>()? as usize;

                src_mem.borrow().validate_region(src_base, n)?;

                let values = (0..n)
                    .map(|offset| -> ExecResult<_> {
                        Ok(src_mem.borrow().load_as::<u8>(src_base + offset)?)
                    })
                    .collect::<ExecResult<Vec<_>>>()?;

                dst_mem.borrow().validate_region(dst_base, n)?;
                dst_mem.borrow_mut().store(dst_base, &values)?;

                Ok(Signal::Next)
            }
            InstructionKind::MemoryFill { mem } => {
                let addr = MemoryAddr::new_unsafe(module_index, *mem as usize);
                let mem = store.memory(addr);
                let n = self.pop_as::<i32>()? as usize;
                let val = self.pop_as::<i32>()?;
                let val = val.to_le_bytes()[0];
                let offset = self.pop_as::<i32>()? as usize;

                mem.borrow().validate_region(offset, n)?;

                mem.borrow_mut()
                    .store(offset, &std::iter::repeat(val).take(n).collect::<Vec<_>>())?;

                Ok(Signal::Next)
            }
            InstructionKind::MemoryInit { segment, mem } => {
                let mem_addr = MemoryAddr::new_unsafe(module_index, *mem as usize);
                let seg_addr = DataAddr::new_unsafe(module_index, *segment as usize);
                let mem = store.memory(mem_addr);
                let data = store.data(seg_addr);
                let n = self.pop_as::<i32>()? as usize;
                let src_base = self.pop_as::<i32>()? as usize;
                let dst_base = self.pop_as::<i32>()? as usize;

                mem.borrow().validate_region(dst_base, n)?;
                data.borrow().validate_region(src_base, n)?;

                mem.borrow_mut()
                    .store(dst_base, &data.borrow().raw()[src_base..(src_base + n)])?;
                Ok(Signal::Next)
            }
            InstructionKind::DataDrop { segment } => {
                let data_addr = DataAddr::new_unsafe(module_index, *segment as usize);
                let data = store.data(data_addr);
                data.borrow_mut().drop_bytes();
                Ok(Signal::Next)
            }

            InstructionKind::RefNull { ty } => {
                let null_ref = Value::null_ref(*ty)
                    .expect("invalid null_ref type should be validated before execution");
                self.stack.push_value(null_ref);
                Ok(Signal::Next)
            }
            InstructionKind::RefIsNull => {
                let ref_val = self.pop_ref()?;
                let ret_val = match ref_val {
                    RefVal::NullRef(_) => Value::I32(1),
                    _ => Value::I32(0),
                };
                self.stack.push_value(ret_val);
                Ok(Signal::Next)
            }
            InstructionKind::RefFunc { function_index } => {
                let ref_val = Value::Ref(RefVal::FuncRef(FuncAddr::new_unsafe(
                    module_index,
                    *function_index as usize,
                )));
                self.stack.push_value(ref_val);
                Ok(Signal::Next)
            }
            InstructionKind::I32Const { value } => {
                self.stack.push_value(Value::I32(*value));
                Ok(Signal::Next)
            }
            InstructionKind::I64Const { value } => {
                self.stack.push_value(Value::I64(*value));
                Ok(Signal::Next)
            }
            InstructionKind::F32Const { value } => {
                self.stack.push_value(Value::F32(value.bits()));
                Ok(Signal::Next)
            }
            InstructionKind::F64Const { value } => {
                self.stack.push_value(Value::F64(value.bits()));
                Ok(Signal::Next)
            }

            InstructionKind::I32Eqz => self.testop::<i32, _>(|v| v == 0),
            InstructionKind::I32Eq => self.relop(|a: i32, b: i32| a == b),
            InstructionKind::I32Ne => self.relop(|a: i32, b: i32| a != b),
            InstructionKind::I32LtS => self.relop(|a: i32, b: i32| a < b),
            InstructionKind::I32LtU => self.relop::<u32, _>(|a, b| a < b),
            InstructionKind::I32GtS => self.relop(|a: i32, b: i32| a > b),
            InstructionKind::I32GtU => self.relop::<u32, _>(|a, b| a > b),
            InstructionKind::I32LeS => self.relop(|a: i32, b: i32| a <= b),
            InstructionKind::I32LeU => self.relop::<u32, _>(|a, b| a <= b),
            InstructionKind::I32GeS => self.relop(|a: i32, b: i32| a >= b),
            InstructionKind::I32GeU => self.relop::<u32, _>(|a, b| a >= b),

            InstructionKind::I64Eqz => self.testop::<i64, _>(|v| v == 0),
            InstructionKind::I64Eq => self.relop(|a: i64, b: i64| a == b),
            InstructionKind::I64Ne => self.relop(|a: i64, b: i64| a != b),
            InstructionKind::I64LtS => self.relop(|a: i64, b: i64| a < b),
            InstructionKind::I64LtU => self.relop::<u64, _>(|a, b| a < b),
            InstructionKind::I64GtS => self.relop(|a: i64, b: i64| a > b),
            InstructionKind::I64GtU => self.relop::<u64, _>(|a, b| a > b),
            InstructionKind::I64LeS => self.relop(|a: i64, b: i64| a <= b),
            InstructionKind::I64LeU => self.relop::<u64, _>(|a, b| a <= b),
            InstructionKind::I64GeS => self.relop(|a: i64, b: i64| a >= b),
            InstructionKind::I64GeU => self.relop::<u64, _>(|a, b| a >= b),

            // Safety: imprecision is expected behavior
            #[allow(clippy::float_cmp)]
            InstructionKind::F32Eq => self.relop::<F32, _>(|a, b| a.to_float() == b.to_float()),
            #[allow(clippy::float_cmp)]
            InstructionKind::F32Ne => self.relop::<F32, _>(|a, b| a.to_float() != b.to_float()),
            InstructionKind::F32Lt => self.relop::<F32, _>(|a, b| a.to_float() < b.to_float()),
            InstructionKind::F32Gt => self.relop::<F32, _>(|a, b| a.to_float() > b.to_float()),
            InstructionKind::F32Le => self.relop::<F32, _>(|a, b| a.to_float() <= b.to_float()),
            InstructionKind::F32Ge => self.relop::<F32, _>(|a, b| a.to_float() >= b.to_float()),

            // Safety: imprecision is expected behavior
            #[allow(clippy::float_cmp)]
            InstructionKind::F64Eq => self.relop(|a: F64, b: F64| a.to_float() == b.to_float()),
            #[allow(clippy::float_cmp)]
            InstructionKind::F64Ne => self.relop(|a: F64, b: F64| a.to_float() != b.to_float()),
            InstructionKind::F64Lt => self.relop(|a: F64, b: F64| a.to_float() < b.to_float()),
            InstructionKind::F64Gt => self.relop(|a: F64, b: F64| a.to_float() > b.to_float()),
            InstructionKind::F64Le => self.relop(|a: F64, b: F64| a.to_float() <= b.to_float()),
            InstructionKind::F64Ge => self.relop(|a: F64, b: F64| a.to_float() >= b.to_float()),

            InstructionKind::I32Clz => self.unop(|v: i32| v.leading_zeros() as i32),
            InstructionKind::I32Ctz => self.unop(|v: i32| v.trailing_zeros() as i32),
            InstructionKind::I32Popcnt => self.unop(|v: i32| v.count_ones() as i32),
            InstructionKind::I32Add => self.binop(|a: u32, b: u32| a.wrapping_add(b)),
            InstructionKind::I32Sub => self.binop(|a: i32, b: i32| a.wrapping_sub(b)),
            InstructionKind::I32Mul => self.binop(|a: i32, b: i32| a.wrapping_mul(b)),
            InstructionKind::I32DivS => self.try_binop(I32::try_wrapping_div),
            InstructionKind::I32DivU => self.try_binop(U32::try_wrapping_div),
            InstructionKind::I32RemS => self.try_binop(I32::try_wrapping_rem),
            InstructionKind::I32RemU => self.try_binop(U32::try_wrapping_rem),
            InstructionKind::I32And => self.binop(|a: i32, b: i32| a.bitand(b)),
            InstructionKind::I32Or => self.binop(|a: i32, b: i32| a.bitor(b)),
            InstructionKind::I32Xor => self.binop(|a: i32, b: i32| a.bitxor(b)),
            InstructionKind::I32Shl => self.binop(|a: u32, b: u32| a.wrapping_shl(b)),
            InstructionKind::I32ShrS => self.binop(|a: i32, b: i32| a.wrapping_shr(b as u32)),
            InstructionKind::I32ShrU => self.binop(|a: u32, b: u32| a.wrapping_shr(b)),
            InstructionKind::I32Rotl => self.binop(|a: i32, b: i32| a.rotate_left(b as u32)),
            InstructionKind::I32Rotr => self.binop(|a: i32, b: i32| a.rotate_right(b as u32)),

            InstructionKind::I64Clz => self.unop(|v: i64| v.leading_zeros() as i64),
            InstructionKind::I64Ctz => self.unop(|v: i64| v.trailing_zeros() as i64),
            InstructionKind::I64Popcnt => self.unop(|v: i64| v.count_ones() as i64),
            InstructionKind::I64Add => self.binop(|a: i64, b: i64| a.wrapping_add(b)),
            InstructionKind::I64Sub => self.binop(|a: i64, b: i64| a.wrapping_sub(b)),
            InstructionKind::I64Mul => self.binop(|a: i64, b: i64| a.wrapping_mul(b)),
            InstructionKind::I64DivS => self.try_binop(I64::try_wrapping_div),
            InstructionKind::I64DivU => self.try_binop(U64::try_wrapping_div),
            InstructionKind::I64RemS => self.try_binop(I64::try_wrapping_rem),
            InstructionKind::I64RemU => self.try_binop(U64::try_wrapping_rem),
            InstructionKind::I64And => self.binop(|a: i64, b: i64| a.bitand(b)),
            InstructionKind::I64Or => self.binop(|a: i64, b: i64| a.bitor(b)),
            InstructionKind::I64Xor => self.binop(|a: i64, b: i64| a.bitxor(b)),
            InstructionKind::I64Shl => self.binop(|a: u64, b: u64| a.wrapping_shl(b as u32)),
            InstructionKind::I64ShrS => self.binop(|a: i64, b: i64| a.wrapping_shr(b as u32)),
            InstructionKind::I64ShrU => self.binop(|a: u64, b: u64| a.wrapping_shr(b as u32)),
            InstructionKind::I64Rotl => self.binop(|a: i64, b: i64| a.rotate_left(b as u32)),
            InstructionKind::I64Rotr => self.binop(|a: i64, b: i64| a.rotate_right(b as u32)),

            InstructionKind::F32Abs => self.unop(|v: F32| v.to_float().abs()),
            InstructionKind::F32Neg => self.unop(|v: F32| -v.to_float()),
            InstructionKind::F32Ceil => self.unop(|v: F32| v.to_float().ceil()),
            InstructionKind::F32Floor => self.unop(|v: F32| v.to_float().floor()),
            InstructionKind::F32Trunc => self.unop(|v: F32| v.to_float().trunc()),
            InstructionKind::F32Nearest => self.unop(|v: F32| v.nearest()),
            InstructionKind::F32Sqrt => self.unop(|v: F32| v.to_float().sqrt()),
            InstructionKind::F32Add => self.binop(|a: F32, b: F32| a.to_float() + b.to_float()),
            InstructionKind::F32Sub => self.binop(|a: F32, b: F32| a.to_float() - b.to_float()),
            InstructionKind::F32Mul => self.binop(|a: F32, b: F32| a.to_float() * b.to_float()),
            InstructionKind::F32Div => self.binop(|a: F32, b: F32| a.to_float() / b.to_float()),
            InstructionKind::F32Min => self.binop(F32::min),
            InstructionKind::F32Max => self.binop(F32::max),
            InstructionKind::F32Copysign => self.binop(|a: F32, b: F32| a.copysign(b)),

            InstructionKind::F64Abs => self.unop(|v: F64| v.to_float().abs()),
            InstructionKind::F64Neg => self.unop(|v: F64| -v.to_float()),
            InstructionKind::F64Ceil => self.unop(|v: F64| v.to_float().ceil()),
            InstructionKind::F64Floor => self.unop(|v: F64| v.to_float().floor()),
            InstructionKind::F64Trunc => self.unop(|v: F64| v.to_float().trunc()),
            InstructionKind::F64Nearest => self.unop(|v: F64| v.nearest()),
            InstructionKind::F64Sqrt => self.unop(|v: F64| v.to_float().sqrt()),
            InstructionKind::F64Add => self.binop(|a: F64, b: F64| a.to_float() + b.to_float()),
            InstructionKind::F64Sub => self.binop(|a: F64, b: F64| a.to_float() - b.to_float()),
            InstructionKind::F64Mul => self.binop(|a: F64, b: F64| a.to_float() * b.to_float()),
            InstructionKind::F64Div => self.binop(|a: F64, b: F64| a.to_float() / b.to_float()),
            InstructionKind::F64Min => self.binop(F64::min),
            InstructionKind::F64Max => self.binop(F64::max),
            InstructionKind::F64Copysign => self.binop(|a: F64, b: F64| a.copysign(b)),

            InstructionKind::I32WrapI64 => self.unop(|v: i64| Value::I32(v as i32)),
            InstructionKind::I32TruncF32S => self.try_unop::<F32, _, _>(TruncTo::<i32>::trunc_to),
            InstructionKind::I32TruncF32U => self.try_unop::<F32, _, _>(TruncTo::<u32>::trunc_to),
            InstructionKind::I32TruncF64S => self.try_unop::<F64, _, _>(TruncTo::<i32>::trunc_to),
            InstructionKind::I32TruncF64U => self.try_unop::<F64, _, _>(TruncTo::<u32>::trunc_to),
            InstructionKind::I64ExtendI32S => self.unop(|v: i32| Value::from(v as u64)),
            InstructionKind::I64ExtendI32U => self.unop(|v: u32| Value::from(v as u64)),
            InstructionKind::I64TruncF32S => self.try_unop::<F32, _, _>(TruncTo::<i64>::trunc_to),
            InstructionKind::I64TruncF32U => self.try_unop::<F32, _, _>(TruncTo::<u64>::trunc_to),
            InstructionKind::I64TruncF64S => self.try_unop::<F64, _, _>(TruncTo::<i64>::trunc_to),
            InstructionKind::I64TruncF64U => self.try_unop::<F64, _, _>(TruncTo::<u64>::trunc_to),
            InstructionKind::F32ConvertI32S => self.unop(|x: u32| x as i32 as f32),
            InstructionKind::F32ConvertI32U => self.unop(|x: u32| x as f32),
            InstructionKind::F32ConvertI64S => self.unop(|x: u64| x as i64 as f32),
            InstructionKind::F32ConvertI64U => self.unop(|x: u64| x as f32),
            InstructionKind::F32DemoteF64 => self.unop(|x: F64| x.to_float() as f32),
            InstructionKind::F64ConvertI32S => self.unop(|x: u32| f64::from(x as i32)),
            InstructionKind::F64ConvertI32U => self.unop::<u32, _, _>(f64::from),
            InstructionKind::F64ConvertI64S => self.unop(|x: u64| x as i64 as f64),
            InstructionKind::F64ConvertI64U => self.unop(|x: u64| x as f64),
            InstructionKind::F64PromoteF32 => self.unop(|x: F32| f64::from(x.to_float())),

            InstructionKind::I32Extend8S => self.unop(|x: i32| I32::extend_with_width(x, 8)),
            InstructionKind::I32Extend16S => self.unop(|x: i32| I32::extend_with_width(x, 16)),
            InstructionKind::I64Extend8S => self.unop(|x: i64| I64::extend_with_width(x, 8)),
            InstructionKind::I64Extend16S => self.unop(|x: i64| I64::extend_with_width(x, 16)),
            InstructionKind::I64Extend32S => self.unop(|x: i64| I64::extend_with_width(x, 32)),

            InstructionKind::I32ReinterpretF32 => self.unop(|v: F32| v.to_bits() as i32),
            InstructionKind::I64ReinterpretF64 => self.unop(|v: F64| v.to_bits() as i64),
            InstructionKind::F32ReinterpretI32 => self.unop(f32::from_bits),
            InstructionKind::F64ReinterpretI64 => self.unop(f64::from_bits),
            InstructionKind::I32TruncSatF32S => self.unop::<F32, _, _>(TruncSat::<i32>::trunc_sat),
            InstructionKind::I32TruncSatF32U => self.unop::<F32, _, _>(TruncSat::<u32>::trunc_sat),
            InstructionKind::I32TruncSatF64S => self.unop::<F64, _, _>(TruncSat::<i32>::trunc_sat),
            InstructionKind::I32TruncSatF64U => self.unop::<F64, _, _>(TruncSat::<u32>::trunc_sat),
            InstructionKind::I64TruncSatF32S => self.unop::<F32, _, _>(TruncSat::<i64>::trunc_sat),
            InstructionKind::I64TruncSatF32U => self.unop::<F32, _, _>(TruncSat::<u64>::trunc_sat),
            InstructionKind::I64TruncSatF64S => self.unop::<F64, _, _>(TruncSat::<i64>::trunc_sat),
            InstructionKind::I64TruncSatF64U => self.unop::<F64, _, _>(TruncSat::<u64>::trunc_sat),
            other => unimplemented!("{:?}", other),
        };
        if self.stack.is_over_top_level() {
            Ok(Signal::End)
        } else {
            result
        }
    }

    fn pop_as<T: NativeValue>(&mut self) -> ExecResult<T> {
        let value = self.stack.pop_value().map_err(Trap::Stack)?;
        T::from_value(value).ok_or(Trap::UnexpectedStackValueType {
            expected: T::value_type(),
            actual: value.value_type(),
        })
    }
    fn pop_ref(&mut self) -> ExecResult<RefVal> {
        let ref_val: Value = self.stack.pop_value().map_err(Trap::Stack)?;
        let ref_val = match ref_val {
            Value::Ref(r) => r,
            _ => {
                return Err(Trap::UnexpectedNonRefValueType {
                    actual: ref_val.value_type(),
                })
            }
        };
        Ok(ref_val)
    }

    fn branch(&mut self, depth: u32, store: &Store) -> ExecResult<Signal> {
        let depth = depth as usize;
        let label = *self.stack.frame_label(depth).map_err(Trap::Stack)?;

        let arity = label.arity();

        let mut results = vec![];
        for _ in 0..arity {
            results.push(self.stack.pop_value().map_err(Trap::Stack)?);
        }

        for _ in 0..depth + 1 {
            self.stack.pop_while(|v| matches!(v, StackValue::Value(_)));
            self.stack.pop_label().map_err(Trap::Stack)?;
        }

        for _ in 0..arity {
            self.stack.push_value(results.pop().unwrap());
        }

        // Jump to the continuation
        match label {
            Label::Loop { label, .. } => self.pc.loop_jump(&label),
            Label::Return { .. } => {
                return self.do_return(store);
            }
            Label::If { .. } | Label::Block { .. } => {
                let mut depth = depth + 1;
                loop {
                    let index = self.pc.inst_index().0 as usize;
                    match self.current_func_insts(store)?[index].kind {
                        InstructionKind::End => depth -= 1,
                        InstructionKind::Block { ty: _ } => depth += 1,
                        InstructionKind::If { ty: _ } => depth += 1,
                        InstructionKind::Loop { ty: _ } => depth += 1,
                        _ => (),
                    }
                    self.pc.inc_inst_index();
                    if depth == 0 {
                        break;
                    }
                }
            }
        }
        Ok(Signal::Next)
    }

    fn testop<T: NativeValue, F: Fn(T) -> bool>(&mut self, f: F) -> ExecResult<Signal> {
        self.unop(|a| Value::I32(if f(a) { 1 } else { 0 }))
    }

    fn relop<T: NativeValue, F: Fn(T, T) -> bool>(&mut self, f: F) -> ExecResult<Signal> {
        self.binop(|a: T, b: T| Value::I32(if f(a, b) { 1 } else { 0 }))
    }

    fn try_binop<T: NativeValue, To: Into<Value>, F: Fn(T, T) -> Result<To, value::Error>>(
        &mut self,
        f: F,
    ) -> ExecResult<Signal> {
        let rhs = self.pop_as()?;
        let lhs = self.pop_as()?;
        self.stack
            .push_value(f(lhs, rhs).map(|v| v.into()).map_err(Trap::Value)?);
        Ok(Signal::Next)
    }

    fn binop<T: NativeValue, To: Into<Value>, F: Fn(T, T) -> To>(
        &mut self,
        f: F,
    ) -> ExecResult<Signal> {
        let rhs = self.pop_as()?;
        let lhs = self.pop_as()?;
        self.stack.push_value(f(lhs, rhs).into());
        Ok(Signal::Next)
    }

    fn try_unop<From: NativeValue, To: Into<Value>, F: Fn(From) -> Result<To, value::Error>>(
        &mut self,
        f: F,
    ) -> ExecResult<Signal> {
        let v: From = self.pop_as()?;
        self.stack
            .push_value(f(v).map(|v| v.into()).map_err(Trap::Value)?);
        Ok(Signal::Next)
    }

    fn unop<From: NativeValue, To: Into<Value>, F: Fn(From) -> To>(
        &mut self,
        f: F,
    ) -> ExecResult<Signal> {
        let v: From = self.pop_as()?;
        self.stack.push_value(f(v).into());
        Ok(Signal::Next)
    }

    fn invoke<I: Interceptor>(
        &mut self,
        addr: FuncAddr,
        store: &Store,
        interceptor: &I,
    ) -> ExecResult<Signal> {
        let (func, exec_addr) = store.func(addr).ok_or(Trap::UndefinedFunc(addr.1))?;

        let mut args = Vec::new();
        let mut found_mismatch = false;
        for _ in func.ty().params.iter() {
            match self.stack.pop_value() {
                Ok(val) => args.push(val),
                Err(_) => found_mismatch = true,
            }
        }

        if found_mismatch {
            return Err(Trap::DirectCallTypeMismatch {
                callee_name: func.name().to_string(),
                actual: args.iter().map(|v| v.value_type()).collect(),
                expected: func.ty().params.to_vec(),
            });
        }
        args.reverse();

        let arity = func.ty().returns.len();
        match func {
            FunctionInstance::Defined(func) => {
                let pc = ProgramCounter::new(func.module_index(), exec_addr, InstIndex::zero());
                let frame = CallFrame::new_from_func(exec_addr, func, args, Some(self.pc));
                self.stack.set_frame(frame).map_err(Trap::Stack)?;
                self.stack.push_label(Label::Return { arity });
                self.pc = pc;
                interceptor.invoke_func(func.name(), self, store)
            }
            FunctionInstance::Native(func) => {
                let mut result = Vec::new();
                func.code()
                    .call(&args, &mut result, store, addr.module_index())?;
                assert_eq!(result.len(), arity);
                for v in result {
                    self.stack.push_value(v);
                }
                Ok(Signal::Next)
            }
        }
    }
    fn do_return(&mut self, store: &Store) -> ExecResult<Signal> {
        let ret_pc = self.stack.current_frame().map_err(Trap::Stack)?.ret_pc;
        let func = store.func_global(self.pc.exec_addr());
        let arity = func.ty().returns.len();
        let results = self.stack.pop_values(arity).map_err(Trap::Stack)?;
        self.stack
            .pop_while(|v| !matches!(v, StackValue::Activation(_)));
        self.stack.pop_frame().map_err(Trap::Stack)?;
        self.stack.push_values(results.into_iter().rev());

        if let Some(ret_pc) = ret_pc {
            self.pc = ret_pc;
        }
        Ok(Signal::Next)
    }

    /// Returns a pair of arities for parameter and result
    fn get_type_arity(&self, ty: &TypeOrFuncType, store: &Store) -> ExecResult<(usize, usize)> {
        Ok(match ty {
            TypeOrFuncType::Type(Type::EmptyBlockType) => (0, 0),
            TypeOrFuncType::Type(_) => (0, 1),
            TypeOrFuncType::FuncType(type_id) => {
                let frame = self.stack.current_frame().map_err(Trap::Stack)?;
                let module = store.module(frame.module_index()).defined().unwrap();
                let ty = module.get_type(*type_id as usize);
                (ty.params.len(), ty.returns.len())
            }
        })
    }

    fn set_local(&mut self, index: usize) -> ExecResult<Signal> {
        let value = self.stack.pop_value().map_err(Trap::Stack)?;
        self.stack.set_local(index, value).map_err(Trap::Stack)?;

        Ok(Signal::Next)
    }

    fn memory(&self, store: &Store) -> ExecResult<std::rc::Rc<std::cell::RefCell<MemoryInstance>>> {
        let frame = self.stack.current_frame().map_err(Trap::Stack)?;
        let mem_addr = MemoryAddr::new_unsafe(frame.module_index(), 0);
        Ok(store.memory(mem_addr))
    }

    fn mem_addr(base: u32, offset: u64, memory64: bool) -> ExecResult<u64> {
        let addr = if memory64 {
            offset.checked_add(base as u64)
        } else {
            let offset: u32 = offset
                .try_into()
                .map_err(|_| Trap::MemoryAddrOverflow { base, offset })?;
            let addr = offset.checked_add(base as u32);
            addr.map(|v| v as u64)
        };
        if let Some(addr) = addr {
            Ok(addr)
        } else {
            Err(Trap::MemoryAddrOverflow { base, offset })
        }
    }

    fn store<T: NativeValue + IntoLittleEndian, I: Interceptor>(
        &mut self,
        offset: u64,
        store: &Store,
        interceptor: &I,
        config: &Config,
    ) -> ExecResult<Signal> {
        let val: T = self.pop_as()?;
        let base_addr: i32 = self.pop_as()?;
        let base_addr: u32 = u32::from_le_bytes(base_addr.to_le_bytes());
        let addr = Self::mem_addr(base_addr, offset, config.features.memory64)? as usize;
        let buf = val.into_le_bytes();
        self.memory(store)?
            .borrow_mut()
            .store(addr, &buf)
            .map_err(Trap::Memory)?;
        interceptor.after_store(addr, &buf)
    }

    fn store_with_width<T: NativeValue + IntoLittleEndian, I: Interceptor>(
        &mut self,
        offset: u64,
        width: usize,
        store: &Store,
        interceptor: &I,
        config: &Config,
    ) -> ExecResult<Signal> {
        let val: T = self.pop_as()?;
        let base_addr: i32 = self.pop_as()?;
        let base_addr: u32 = u32::from_le_bytes(base_addr.to_le_bytes());
        let addr = Self::mem_addr(base_addr, offset, config.features.memory64)? as usize;
        let buf = val.into_le_bytes();
        let buf: Vec<u8> = buf.into_iter().take(width).collect();
        self.memory(store)?
            .borrow_mut()
            .store(addr, &buf)
            .map_err(Trap::Memory)?;
        interceptor.after_store(addr, &buf)
    }

    fn load<T>(&mut self, offset: u64, store: &Store, config: &Config) -> ExecResult<Signal>
    where
        T: NativeValue + FromLittleEndian,
        T: Into<Value>,
    {
        let base_addr: i32 = self.pop_as()?;
        let base_addr: u32 = u32::from_le_bytes(base_addr.to_le_bytes());
        let addr = Self::mem_addr(base_addr, offset, config.features.memory64)? as usize;
        let result: T = self
            .memory(store)?
            .borrow_mut()
            .load_as(addr)
            .map_err(Trap::Memory)?;
        self.stack.push_value(result.into());
        Ok(Signal::Next)
    }

    fn load_extend<T: FromLittleEndian + ExtendInto<U>, U: Into<Value>>(
        &mut self,
        offset: u64,
        store: &Store,
        config: &Config,
    ) -> ExecResult<Signal> {
        let base_addr: i32 = self.pop_as()?;
        let base_addr: u32 = u32::from_le_bytes(base_addr.to_le_bytes());
        let addr = Self::mem_addr(base_addr, offset, config.features.memory64)? as usize;

        let result: T = self
            .memory(store)?
            .borrow_mut()
            .load_as(addr)
            .map_err(Trap::Memory)?;
        let result = result.extend_into();
        self.stack.push_value(result.into());
        Ok(Signal::Next)
    }
}

use wasmparser::InitExpr;
pub fn eval_const_expr(
    init_expr: &InitExpr,
    store: &Store,
    module_index: ModuleIndex,
) -> anyhow::Result<Value> {
    use crate::inst::transform_inst;
    let mut reader = init_expr.get_operators_reader();
    let base_offset = reader.original_position();
    let inst = transform_inst(&mut reader, base_offset)?;
    let val = match inst.kind {
        InstructionKind::I32Const { value } => Value::I32(value),
        InstructionKind::I64Const { value } => Value::I64(value),
        InstructionKind::F32Const { value } => Value::F32(value.bits()),
        InstructionKind::F64Const { value } => Value::F64(value.bits()),
        InstructionKind::RefNull { ty } => match Value::null_ref(ty) {
            Some(v) => v,
            None => panic!("unsupported ref type"),
        },
        InstructionKind::RefFunc { function_index } => Value::Ref(RefVal::FuncRef(
            FuncAddr::new_unsafe(module_index, function_index as usize),
        )),
        InstructionKind::GlobalGet { global_index } => {
            let addr = GlobalAddr::new_unsafe(module_index, global_index as usize);
            store.global(addr).borrow().value()
        }
        _ => panic!("Unsupported init_expr {:?}", inst.kind),
    };
    Ok(val)
}

#[derive(Debug)]
pub enum WasmError {
    ExecutionError(Trap),
    EntryFunctionNotFound(String),
    ReturnValueError(ReturnValError),
    HostExecutionError,
}

impl std::fmt::Display for WasmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WasmError::ExecutionError(err) => write!(f, "Failed to execute: {}", err),
            WasmError::EntryFunctionNotFound(func_name) => {
                write!(f, "Entry function \"{}\" not found", func_name)
            }
            WasmError::ReturnValueError(err) => {
                write!(f, "Failed to get returned value: {:?}", err)
            }
            WasmError::HostExecutionError => write!(f, "Failed to execute host func"),
        }
    }
}
