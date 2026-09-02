//! `mitosctl` - the command-line client for mitos-services' control
//! socket. Connects, sends one command, prints the response, exits.
//! `mitosctl status`, `mitosctl reload`, `mitosctl ping` (default:
//! status).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

const SOCKET_PATH: &str = "/run/mitos-services/control.sock";

fn main() {
    let command = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "status".to_string())
        .to_uppercase();

    let mut stream = match UnixStream::connect(SOCKET_PATH) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("mitosctl: couldn't connect to {SOCKET_PATH}: {e}");
            eprintln!("(is mitos-services running?)");
            std::process::exit(1);
        }
    };

    if stream.write_all(format!("{command}\n").as_bytes()).is_err() {
        eprintln!("mitosctl: couldn't send command");
        std::process::exit(1);
    }

    let mut response = String::new();
    if stream.read_to_string(&mut response).is_err() {
        eprintln!("mitosctl: couldn't read response");
        std::process::exit(1);
    }
    print!("{response}");
}
