use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "carte", version, about = "Runs your apps' processes on demand")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Runs one process for the daemon; not for direct use.
    #[command(name = "__shim", hide = true)]
    Shim {
        /// The lifeline socket inherited from the daemon.
        #[arg(long)]
        fd: i32,
        command: String,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Some(Commands::Shim { fd, command }) => std::process::exit(carte::shim::run(fd, &command)),
        None => {
            println!("carte {}", env!("CARGO_PKG_VERSION"));
        }
    }
}
