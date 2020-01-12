mod address;
mod executor;
mod export;
mod func;
mod global;
mod host;
mod instance;
mod linker;
mod memory;
mod module;
mod stack;
mod store;
mod table;

pub use self::address::*;
pub use self::executor::{Executor, Signal};
pub use self::executor::{Trap, WasmError};
pub use self::func::{FunctionInstance, InstIndex};
pub use self::global::GlobalInstance as HostGlobal;
pub use self::host::{HostContext, HostFuncBody, HostValue};
pub use self::instance::WasmInstance;
pub use self::memory::MemoryInstance as HostMemory;
pub use self::module::ModuleIndex;
pub use self::stack::{CallFrame, ProgramCounter};
pub use self::store::Store;
pub use self::table::TableInstance as HostTable;

pub const WASM_PAGE_SIZE: usize = 0x10000;