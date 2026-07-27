mod args;
mod render;

use args::{Cli, Command};
use clap::Parser;
use filesyncer_core::{Profile, ProfileSet, Session, create_profile_roots, profile};

const EXIT_OK: i32 = 0;
const EXIT_ERROR: i32 = 1;
const EXIT_CONFLICTS: i32 = 2;

fn main() {
    let cli = Cli::parse();
    let code = match run(cli) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("error: {message}");
            EXIT_ERROR
        }
    };
    std::process::exit(code);
}

fn run(cli: Cli) -> Result<i32, String> {
    match cli.command {
        Command::List => cmd_list(),
        Command::Add {
            id,
            name,
            local,
            remote,
        } => cmd_add(id, name, local, remote),
        Command::Status { profile } => cmd_status(&profile),
        Command::Sync {
            profile,
            yes,
            dry_run,
            force,
        } => cmd_sync(&profile, yes, dry_run, force),
    }
}

fn load_profile(id: &str) -> Result<Profile, String> {
    let path = profile::config_path();
    let set = ProfileSet::load(&path).map_err(|e| e.to_string())?;
    set.get(id)
        .cloned()
        .ok_or_else(|| format!("no profile `{id}` in {}", path.display()))
}

fn cmd_list() -> Result<i32, String> {
    let path = profile::config_path();
    let set = ProfileSet::load(&path).map_err(|e| e.to_string())?;
    if set.profiles.is_empty() {
        println!("No profiles configured. Add one with `filesyncer add <id> <local> <remote>`.");
        return Ok(EXIT_OK);
    }
    for p in &set.profiles {
        println!("{}  {}", p.id, p.name);
        println!("    local:  {}", p.local.display());
        println!("    remote: {}", p.remote.display());
    }
    Ok(EXIT_OK)
}

fn cmd_add(
    id: String,
    name: Option<String>,
    local: std::path::PathBuf,
    remote: std::path::PathBuf,
) -> Result<i32, String> {
    let path = profile::config_path();
    let mut set = ProfileSet::load(&path).map_err(|e| e.to_string())?;
    if set.get(&id).is_some() {
        return Err(format!("profile `{id}` already exists"));
    }

    let profile = Profile {
        name: name.unwrap_or_else(|| id.clone()),
        id,
        local,
        remote,
    };
    // Validates the roots and stamps both markers; fails before anything is saved.
    create_profile_roots(&profile).map_err(|e| e.to_string())?;

    set.profiles.push(profile.clone());
    set.save(&path).map_err(|e| e.to_string())?;
    println!(
        "Added profile `{}`. Markers written to both roots.",
        profile.id
    );
    Ok(EXIT_OK)
}

fn cmd_status(id: &str) -> Result<i32, String> {
    let profile = load_profile(id)?;
    let session = Session::open(&profile).map_err(|e| e.to_string())?;
    println!("{}  {}", profile.id, profile.name);
    println!("    local:  {}", profile.local.display());
    println!("    remote: {}", profile.remote.display());
    match session.last_sync().map_err(|e| e.to_string())? {
        Some(ts) => println!("    last synced: unix {ts}"),
        None => println!("    last synced: never"),
    }
    println!(
        "    tracked paths: {}",
        session.tracked_count().map_err(|e| e.to_string())?
    );
    println!("\n(status does not scan; run `filesyncer sync {id} --dry-run` to see pending work)");
    Ok(EXIT_OK)
}

fn cmd_sync(id: &str, yes: bool, dry_run: bool, force: bool) -> Result<i32, String> {
    let profile = load_profile(id)?;
    let session = Session::open(&profile).map_err(|e| e.to_string())?;
    let plan = session.prepare(force).map_err(|e| e.to_string())?;

    print!("{}", render::plan_summary(&plan));

    let conflicts_present = !plan.conflicts.is_empty();

    if dry_run {
        return Ok(if conflicts_present {
            EXIT_CONFLICTS
        } else {
            EXIT_OK
        });
    }
    if plan.is_noop() {
        return Ok(if conflicts_present {
            EXIT_CONFLICTS
        } else {
            EXIT_OK
        });
    }
    if !yes && !render::confirm("Apply this plan?").map_err(|e| e.to_string())? {
        println!("Aborted.");
        return Ok(EXIT_OK);
    }

    let total = std::cell::Cell::new(0usize);
    let summary = session
        .run(
            &plan,
            &|p| match p {
                filesyncer_core::Progress::Started { total: n } => total.set(n),
                filesyncer_core::Progress::Item {
                    index, rel_path, ..
                } => {
                    println!("[{}/{}] {rel_path}", index + 1, total.get());
                }
                filesyncer_core::Progress::Finished => {}
            },
            &|| false,
        )
        .map_err(|e| e.to_string())?;

    print!("{}", render::run_summary(&summary));

    if !summary.errors.is_empty() {
        return Ok(EXIT_ERROR);
    }
    Ok(if summary.conflicts > 0 {
        EXIT_CONFLICTS
    } else {
        EXIT_OK
    })
}
