mod args;
mod render;

fn main() {
    let _ = <args::Cli as clap::Parser>::parse();
    eprintln!("not yet implemented");
    std::process::exit(1);
}
