//! ssh's askpass for the SFTP tests, whose own program is the test harness
//! (see `disk::sftp::set_askpass_program`).

fn main() {
    std::process::exit(disk::sftp::askpass_main().unwrap_or(2));
}
