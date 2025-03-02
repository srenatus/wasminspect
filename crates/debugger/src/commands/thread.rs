use super::command::{Command, CommandContext, CommandResult};
use super::debugger::{Debugger, StepStyle};
use super::disassemble::display_asm;
use super::list::{display_source, next_line_info};
use super::symbol::demangle_symbol;

pub struct ThreadCommand {}

impl ThreadCommand {
    pub fn new() -> Self {
        Self {}
    }
}

use anyhow::Result;
use structopt::StructOpt;

#[derive(StructOpt)]
enum Opts {
    #[structopt(name = "info")]
    Info,
    #[structopt(name = "backtrace")]
    Backtrace,
    #[structopt(name = "step-in")]
    StepIn,
    #[structopt(name = "step-over")]
    StepOver,
    #[structopt(name = "step-out")]
    StepOut,
    #[structopt(name = "step-inst-in")]
    StepInstIn,
    #[structopt(name = "step-inst-over")]
    StepInstOver,
}

impl<D: Debugger> Command<D> for ThreadCommand {
    fn name(&self) -> &'static str {
        "thread"
    }

    fn description(&self) -> &'static str {
        "Commands for operating the thread."
    }

    fn run(
        &self,
        debugger: &mut D,
        context: &CommandContext,
        args: Vec<&str>,
    ) -> Result<Option<CommandResult>> {
        let opts = Opts::from_iter_safe(args.clone())?;
        match opts {
            Opts::Info => {
                let frames = debugger.frame();
                let frame_name = frames.last().unwrap();
                let (insts, next_index) = debugger.instructions()?;
                let current_index = if next_index == 0 { 0 } else { next_index - 1 };
                let current_inst = insts[current_index].clone();
                let code_offset = current_inst.offset;
                let output = if let Some(line_info) = context.sourcemap.find_line_info(code_offset)
                {
                    format!(
                        "0x{:x} `{} at {}:{}:{}`",
                        code_offset,
                        frame_name,
                        line_info.filepath,
                        line_info
                            .line
                            .map(|l| format!("{}", l))
                            .unwrap_or_else(|| "".to_string()),
                        Into::<u64>::into(line_info.column)
                    )
                } else {
                    format!("0x{:x} `{}`", code_offset, frame_name)
                };
                context.printer.println(&output);
            }
            Opts::Backtrace => {
                for (index, frame) in debugger.frame().iter().rev().enumerate() {
                    let output = format!("{}: {}", index, demangle_symbol(frame));
                    context.printer.println(&output);
                }
            }
            Opts::StepIn | Opts::StepOver => {
                let style = match opts {
                    Opts::StepIn => StepStyle::InstIn,
                    Opts::StepOver => StepStyle::InstOver,
                    _ => panic!(),
                };
                let initial_line_info = next_line_info(debugger, context.sourcemap.as_ref())?;
                while {
                    debugger.step(style)?;
                    let line_info = next_line_info(debugger, context.sourcemap.as_ref())?;
                    initial_line_info.filepath == line_info.filepath
                        && initial_line_info.line == line_info.line
                } {}
                let line_info = next_line_info(debugger, context.sourcemap.as_ref())?;
                display_source(line_info, context.printer.as_ref())?;
            }
            Opts::StepOut => {
                debugger.step(StepStyle::Out)?;
                let line_info = next_line_info(debugger, context.sourcemap.as_ref())?;
                display_source(line_info, context.printer.as_ref())?;
            }
            Opts::StepInstIn | Opts::StepInstOver => {
                let style = match opts {
                    Opts::StepInstIn => StepStyle::InstIn,
                    Opts::StepInstOver => StepStyle::InstOver,
                    _ => panic!(),
                };
                debugger.step(style)?;
                display_asm(debugger, context.printer.as_ref(), Some(4), true)?;
            }
        }
        Ok(None)
    }
}
