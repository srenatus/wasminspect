use super::command::{Command, CommandContext, CommandResult};
use super::debugger::{Debugger, OutputPrinter};
use structopt::StructOpt;
use anyhow::Result;

pub struct DisassembleCommand {}

impl DisassembleCommand {
    pub fn new() -> Self {
        Self {}
    }
}

#[derive(StructOpt)]
struct Opts {
    #[structopt(short, long)]
    count: Option<usize>,
    #[structopt(short, long)]
    pc: bool,
}

impl<D: Debugger> Command<D> for DisassembleCommand {
    fn name(&self) -> &'static str {
        "disassemble"
    }

    fn description(&self) -> &'static str {
        "Disassemble instructions in the current function."
    }

    fn run(
        &self,
        debugger: &mut D,
        context: &CommandContext,
        args: Vec<&str>,
    ) -> Result<Option<CommandResult>> {
        let opts: Opts = Opts::from_iter_safe(args)?;
        let count = if opts.pc {
            Some(opts.count.unwrap_or(4))
        } else {
            opts.count
        };
        display_asm(debugger, context.printer.as_ref(), count, opts.pc)?;
        Ok(None)
    }
}

pub fn display_asm<D: Debugger>(
    debugger: &D,
    printer: &dyn OutputPrinter,
    count: Option<usize>,
    pc_rel: bool,
) -> Result<()> {
    let (insts, inst_index) = debugger.instructions()?;
    let begin = if pc_rel { inst_index } else { 0 };
    let end = if let Some(count) = count {
        begin + count
    } else {
        insts.len()
    };
    for (index, inst) in insts.iter().enumerate() {
        if !(begin..end).contains(&index) {
            continue;
        }
        let prefix = if index == inst_index { "->" } else { "  " };
        let output = format!("{} 0x{:>08x}: {:?}", prefix, inst.offset, inst.kind);
        printer.println(&output);
    }
    Ok(())
}
