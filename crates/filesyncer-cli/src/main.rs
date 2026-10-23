mod args;
mod render;

use args::{Cli, Command};
use clap::Parser;
use filesyncer_core::{
    Compare, Profile, ProfileSet, Reporter, Session, create_profile_roots, profile,
};
use std::io::IsTerminal;

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
            name,
            local,
            remote,
        } => cmd_add(name, local, remote),
        Command::Status { profile } => cmd_status(&profile),
        Command::Sync {
            profile,
            yes,
            dry_run,
            force,
            verify,
        } => {
            let compare = if verify {
                Compare::Verify
            } else {
                Compare::Quick
            };
            cmd_sync(&profile, yes, dry_run, force, compare)
        }
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
    name: String,
    local: std::path::PathBuf,
    remote: std::path::PathBuf,
) -> Result<i32, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("a name is required".into());
    }
    // The name doubles as the profile id, which becomes a state-database
    // filename and marker content, so it must stay filesystem-safe.
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err("names may contain only letters, digits, hyphens, and underscores".into());
    }

    let path = profile::config_path();
    let mut set = ProfileSet::load(&path).map_err(|e| e.to_string())?;
    // Reject a clash with any existing profile's name or id.
    if set.profiles.iter().any(|p| p.name == name || p.id == name) {
        return Err(format!("a profile named `{name}` already exists"));
    }

    let profile = Profile {
        id: name.to_string(),
        name: name.to_string(),
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

fn cmd_sync(
    id: &str,
    yes: bool,
    dry_run: bool,
    force: bool,
    compare: Compare,
) -> Result<i32, String> {
    let profile = load_profile(id)?;
    let session = Session::open(&profile).map_err(|e| e.to_string())?;

    let scan = std::cell::RefCell::new(render::ScanReporter::new(
        std::io::stdout(),
        std::io::stdout().is_terminal(),
    ));
    // Ctrl-C remains the way to abort a command line scan, which is safe
    // because a scan writes nothing.
    let sink = |p| scan.borrow_mut().handle(p);
    let plan = session
        .prepare(force, compare, &Reporter::new(&sink, &|| false))
        .map_err(|e| e.to_string())?
        .expect("the CLI never cancels a scan");

    print!("{}", render::plan_summary(&plan));

    let conflicts_present = !plan.conflicts.is_empty();

    if dry_run {
        return Ok(if conflicts_present {
            EXIT_CONFLICTS
        } else {
            EXIT_OK
        });
    }
    if plan.actions.is_empty() {
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
    let apply_progress = |p| match p {
        filesyncer_core::Progress::Started { total: n, .. } => total.set(n.unwrap_or(0)),
        filesyncer_core::Progress::Item {
            index, rel_path, ..
        } => {
            println!("[{}/{}] {rel_path}", index + 1, total.get());
        }
        filesyncer_core::Progress::Finished { .. } => {}
    };
    let summary = session
        .run(&plan, &Reporter::new(&apply_progress, &|| false))
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
