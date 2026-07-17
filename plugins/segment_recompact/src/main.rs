//! Thin CLI over the recompact library — see lib.rs for the actual surgery.

use std::env;
use std::process::exit;

use recompact::{cmd_assemble, cmd_extract, cmd_verify, USAGE};

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("{USAGE}");
        exit(2);
    }
    let rc = match args[1].as_str() {
        "extract" => cmd_extract(&args[2..]),
        "assemble" => cmd_assemble(&args[2..]),
        "verify" => cmd_verify(&args[2..]),
        _ => {
            eprintln!("{USAGE}");
            2
        }
    };
    exit(rc);
}
