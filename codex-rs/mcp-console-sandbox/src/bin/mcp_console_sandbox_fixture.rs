use std::io::Read;
use std::io::Write;

fn main() -> std::io::Result<()> {
    let mut bytes = Vec::new();
    std::io::stdin().read_to_end(&mut bytes)?;
    std::io::stdout().write_all(&bytes)?;
    std::io::stderr().write_all(&bytes)
}
