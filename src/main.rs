use anyhow::Result;
use cargo_hole::cmdline::cargo_hole_cli_main;

fn main() -> Result<()> {
    cargo_hole_cli_main()?;
    Ok(())
}