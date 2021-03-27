use std::collections::HashMap;

use crate::rpc::{self, WasmImportModule};
use wasminspect_debugger::{CommandContext, CommandResult, MainDebugger, Process};
use wasminspect_vm::{HostValue, WasmValue};

static VERSION: &str = "0.1.0";

pub fn handle_request(
    req: rpc::Request,
    process: &mut Process<MainDebugger>,
    context: &CommandContext,
) -> rpc::Response {
    match _handle_request(req, process, context) {
        Ok(res) => res,
        Err(err) => rpc::TextResponse::Error {
            message: err.to_string(),
        }
        .into(),
    }
}

fn to_vm_wasm_value(value: &rpc::WasmValue) -> WasmValue {
    match value {
        rpc::WasmValue::F32 { value } => WasmValue::F32(*value),
        rpc::WasmValue::F64 { value } => WasmValue::F64(*value),
        rpc::WasmValue::I32 { value } => WasmValue::I32(*value),
        rpc::WasmValue::I64 { value } => WasmValue::I64(*value),
    }
}

fn from_vm_wasm_value(value: &WasmValue) -> rpc::WasmValue {
    match value {
        WasmValue::F32(v) => rpc::WasmValue::F32 { value: *v },
        WasmValue::F64(v) => rpc::WasmValue::F64 { value: *v },
        WasmValue::I32(v) => rpc::WasmValue::I32 { value: *v },
        WasmValue::I64(v) => rpc::WasmValue::I64 { value: *v },
    }
}

fn remote_import_module(import_modules: Vec<WasmImportModule>) -> anyhow::Result<()> {
    let mut modules: HashMap<String, HashMap<String, HostValue>> = HashMap::new();
    for module in import_modules {
        for import in module.imports {
        }
        // modules.entry(import.module).or_default().insert(import.field, v)
    }
    Ok(())
}

fn _handle_request(
    req: rpc::Request,
    process: &mut Process<MainDebugger>,
    context: &CommandContext,
) -> Result<rpc::Response, anyhow::Error> {
    use rpc::BinaryRequestKind::*;
    use rpc::Request::*;
    use rpc::TextRequest::*;
    use rpc::*;

    match req {
        Text(Import { modules }) => {
            unimplemented!()
        }
        Binary(req) => match req.kind {
            Init => {
                process.debugger.reset_store();
                process.debugger.load_module(req.bytes)?;
                return Ok(rpc::Response::Text(TextResponse::Init));
            }
        },
        Text(Version) => {
            return Ok(TextResponse::Version {
                value: VERSION.to_string(),
            }
            .into());
        }
        Text(CallExported { name, args }) => {
            use wasminspect_debugger::RunResult;
            let func = process.debugger.lookup_func(&name)?;
            let args = args.iter().map(to_vm_wasm_value).collect();
            match process.debugger.execute_func(func, args) {
                Ok(RunResult::Finish(values)) => {
                    let values = values.iter().map(from_vm_wasm_value).collect();
                    return Ok(TextResponse::CallResult { values }.into());
                }
                Ok(RunResult::Breakpoint) => {
                    let mut result = process.run_loop(context)?;
                    loop {
                        match result {
                            CommandResult::ProcessFinish(values) => {
                                let values = values.iter().map(from_vm_wasm_value).collect();
                                return Ok(TextResponse::CallResult { values }.into());
                            }
                            CommandResult::Exit => {
                                match process.dispatch_command("process continue", context)? {
                                    Some(r) => {
                                        result = r;
                                    }
                                    None => {
                                        result = process.run_loop(context)?;
                                    }
                                }
                            }
                        }
                    }
                }
                Err(msg) => {
                    return Err(msg.into());
                }
            }
        }
    }
}
