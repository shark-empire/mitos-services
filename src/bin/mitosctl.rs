//! `mitosctl` - the command-line client for mitos-services' control
//! socket. Connects, sends one line, prints the response, exits.
//! `mitosctl status`, `mitosctl reload`, `mitosctl ping`, `mitosctl
//! targets`, `mitosctl isolate <target>`, `mitosctl launch <path>
//! [args...]`, `mitosctl apps` (default: status).
//!
//! Only the command word itself is case-insensitive (`isolate` and
//! `ISOLATE` both work) - everything after it is forwarded exactly as
//! typed, since it might be a case-sensitive path or target name.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;

const SOCKET_PATH: &str = "/run/mitos-services/control.sock";

fn main() {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "status".to_string());
    let rest: Vec<String> = args.collect();
    let line = if rest.is_empty() {
        command.to_uppercase()
    } else {
        format!("{} {}", command.to_uppercase(), rest.join(" "))
    };

    let mut stream = match UnixStream::connect(SOCKET_PATH) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("mitosctl: couldn't connect to {SOCKET_PATH}: {e}");
            eprintln!("(is mitos-services running?)");
            std::process::exit(1);
        }
    };

    if stream.write_all(format!("{line}\n").as_bytes()).is_err() {
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
