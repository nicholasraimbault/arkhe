//! ark — CLI for the arkhe init system
//!
//! This binary reads plain text files from /run/arkhe/ and /etc/sv/.
//! It does NOT communicate with the supervisor via IPC.
//! If this binary disappears, the system still works.
//!
//! See docs/CLI.md for the full design.

use std::env;
use std::process;

mod cli;

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        print_usage();
        process::exit(1);
    }

    let result = match args[1].as_str() {
        "status" => cli::status(&args[2..]),
        "start" => cli::start(&args[2..]),
        "stop" => cli::stop(&args[2..]),
        "restart" => cli::restart(&args[2..]),
        "reload" => cli::reload(&args[2..]),
        "log" => cli::log(&args[2..]),
        "check" => cli::check(&args[2..]),
        "new" => cli::new_service(&args[2..]),
        "enable" => cli::enable(&args[2..]),
        "disable" => cli::disable(&args[2..]),
        "--help" | "-h" | "help" => {
            print_usage();
            Ok(())
        }
        cmd => {
            eprintln!("ark: unknown command '{cmd}'");
            print_usage();
            process::exit(2);
        }
    };

    if let Err(e) = result {
        eprintln!("ark: {e}");
        process::exit(1);
    }
}

fn print_usage() {
    eprintln!(
        "arkhe init system

usage: ark <command> [args]

commands:
  status [service]    show service status
  start <service>     start a service
  stop <service>      stop a service
  restart <service>   restart a service
  reload              rescan /etc/sv/ for new services immediately
  log <service>       show service logs
  check               audit system health
  new <name>          scaffold a new service
  enable <service>    enable service at boot
  disable <service>   disable service at boot"
    );
}
