//! `terrio formats`.

#[derive(clap::Args)]
pub struct Args {
    /// Print JSON.
    #[arg(long)]
    json: bool,
}

pub fn run(a: Args) -> terrio::Result<()> {
    let drivers = terrio::formats::all_drivers();
    if a.json {
        println!("{}", serde_json::to_string_pretty(&drivers).map_err(|e| terrio::Error::InvalidData(e.to_string()))?);
        return Ok(());
    }
    println!("{:<16} {:<7} {:<5} {:<35} Extensions", "Name", "Kind", "Mode", "Description");
    for d in drivers {
        let mode = format!("{}{}", if d.read { "r" } else { "-" }, if d.write { "w" } else { "-" });
        let kind = format!("{:?}", d.kind).to_lowercase();
        println!("{:<16} {:<7} {:<5} {:<35} {}", d.name, kind, mode, d.long_name, d.extensions.join(", "));
    }
    Ok(())
}
