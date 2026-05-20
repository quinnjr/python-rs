mod ast;
mod builtins;
mod bytecode;
mod cmodules;
mod compiler;
mod error;
mod lexer;
mod object;
mod parser;
mod vm;

use error::PythonError;
use vm::VM;

/// Run a Python source string and print output lines to stdout.
fn run(source: &str) -> Result<(), PythonError> {
    let output = run_and_capture(source)?;
    for line in &output {
        println!("{line}");
    }
    Ok(())
}

/// Run a Python source string and return captured output lines.
pub fn run_and_capture(source: &str) -> Result<Vec<String>, PythonError> {
    let tokens = lexer::tokenize(source)?;
    let module = parser::parse(tokens)?;
    let (code_objects, heap) = compiler::compile(&module)?;
    let mut vm = VM::new(code_objects, heap);
    vm.run()?;
    Ok(vm.output)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: python-rs <script.py>");
        std::process::exit(1);
    }

    let filename = &args[1];
    let source = match std::fs::read_to_string(filename) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error reading {filename}: {e}");
            std::process::exit(1);
        }
    };

    if let Err(e) = run(&source) {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
