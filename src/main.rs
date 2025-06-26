use clap::{arg, command, Parser};


#[derive(Parser)]
#[command(
    author,
    version,
    about = "Server Agent",
    long_about = "A terminal proxy based on grpc, which uses grpc to communicate with the outside world and implement terminal command execution, file transfer and other functions"
)]

#[derive(Debug)]
struct Arguments {
    #[arg(short, long)]
    certpath: String,
    #[arg(short, long)]
    token: String,
    #[arg(short, long)]
    addr: String,
}

fn main() {
    env_logger::init();

    let args = Arguments::parse();

    println!("{:?}",args);
}
