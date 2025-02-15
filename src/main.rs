use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use log::{error, info};
use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command as Cmd;

#[derive(Deserialize)]
struct Config {
    /// List of directories that need to be patched.
    ///
    /// Should be the absolute path to the directory.
    directories: Vec<PathBuf>,
    /// List of crates to patch and their githubs.
    crates: Vec<Crate>,
    /// Name of the branch.
    branch_name: String,
}

#[derive(Debug, Deserialize, Clone)] // Add `Clone` here
struct Crate {
    /// Name of the crate.
    name: String,
    /// URL of the repo
    repo_url: String,
}

#[derive(Parser)]
#[command(name = "patch-iroh-main")]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    #[arg(long, help = "Path to config file")]
    config: PathBuf,

    #[arg(long, short, help = "Enable verbose logging")]
    verbose: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Patch the crates
    Patch {
        /// Whether to execute the full process (push and create PR).
        #[arg(long, default_value_t = false)]
        execute: bool,
    },
    /// Cleanup branches
    Cleanup,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    // Initialize env_logger
    env_logger::Builder::from_default_env()
        .filter_level(if cli.verbose {
            log::LevelFilter::Info
        } else {
            log::LevelFilter::Warn
        })
        .init();

    let config = load_config(&cli.config)?;

    match cli.command {
        Commands::Patch { execute } => patch_crates(
            &config.directories,
            &config.branch_name,
            &config.crates,
            execute,
        )?,
        Commands::Cleanup => cleanup_branches(&config.directories)?,
    }

    Ok(())
}

fn load_config(path: &PathBuf) -> Result<Config> {
    let config_content = fs::read_to_string(path)
        .with_context(|| format!("Failed to read config file at {}", path.display()))?;
    let config: Config =
        toml::from_str(&config_content).with_context(|| "Failed to parse config file")?;

    // Validate that all directories are absolute paths
    for dir in &config.directories {
        if !dir.is_absolute() {
            return Err(anyhow::anyhow!(
                "Directory path '{}' is not absolute",
                dir.display()
            ));
        }
    }

    Ok(config)
}

fn patch_crates(
    directories: &[PathBuf],
    branch_name: &str,
    crates: &[Crate],
    execute: bool,
) -> Result<()> {
    // info!("Patching crates...");
    let mut successful = vec![];
    let mut unsuccessful = vec![];
    for dir in directories {
        match patch_crate(dir, branch_name, crates, execute) {
            Err(e) => {
                error!("{e:?}");
                unsuccessful.push(dir);
            }
            Ok(()) => {
                successful.push(dir);
            }
        }
    }
    if !successful.is_empty() {
        info!("crates successfully patched:");
        for cr in successful {
            let filename = cr.file_name().unwrap().to_string_lossy();
            info!("\t{filename}");
        }
    }

    if !unsuccessful.is_empty() {
        info!("crates that could not be patched:");
        for cr in unsuccessful {
            let filename = cr.file_name().unwrap().to_string_lossy();
            info!("\t{filename}");
        }
    }
    Ok(())
}

fn patch_crate(
    directory: &PathBuf,
    branch_name: &str,
    crates: &[Crate],
    execute: bool,
) -> Result<()> {
    std::env::set_current_dir(directory)?;
    let dir_name = directory.file_name().expect("checked");
    info!("Working with repo {dir_name:?}");
    // Check if the branch already exists
    let branch_exists = Cmd::new("git")
        .args(["rev-parse", "--verify", branch_name])
        .status()
        .map(|status| status.success())
        .unwrap_or(false);

    if !branch_exists {
        create_and_checkout_branch(branch_name)?;
    } else {
        info!(
            "Branch '{}' already exists. Skipping branch creation.",
            branch_name
        );
    }

    // Ensure patches are in Cargo.toml and get the list of updated crates
    let updated_crates = ensure_patches_in_cargo_toml(crates)?;

    // Commit changes if there are updated crates
    if !updated_crates.is_empty() {
        commit_changes(&updated_crates)?;
    }

    // Push and create PR if `execute` is true
    if execute {
        push_branch(branch_name)?;

        // Get all crates in [patch.crates-io] that are in our list of crates
        let cargo_toml_content =
            fs::read_to_string("Cargo.toml").with_context(|| "Failed to read Cargo.toml")?;
        let existing_patches = parse_existing_patches(&cargo_toml_content)?;
        let all_relevant_crates: Vec<Crate> = crates
            .iter()
            .filter(|c| existing_patches.contains(&c.name))
            .cloned() // Use `cloned()` to convert `&Crate` to `Crate`
            .collect();

        create_pull_request(branch_name, &all_relevant_crates)?;
        info!("Pull request created!");
    } else {
        info!("Dry run complete. Changes were committed but not pushed.");
    }
    Ok(())
}

fn create_and_checkout_branch(branch_name: &str) -> Result<()> {
    Cmd::new("git")
        .args(["checkout", "-b", branch_name])
        .status()
        .with_context(|| "Failed to create and checkout branch")?;
    Ok(())
}

fn ensure_patches_in_cargo_toml(crates: &[Crate]) -> Result<Vec<Crate>> {
    let cargo_toml_path = Path::new("Cargo.toml");
    let cargo_toml_content =
        fs::read_to_string(cargo_toml_path).with_context(|| "Failed to read Cargo.toml")?;

    // Parse Cargo.toml to find referenced dependencies
    let referenced_crates = parse_referenced_crates(&cargo_toml_content)?;

    // Parse existing patches from [patch.crates-io]
    let existing_patches = parse_existing_patches(&cargo_toml_content)?;

    // Open Cargo.toml for appending
    let mut cargo_toml = fs::OpenOptions::new()
        .append(true)
        .open(cargo_toml_path)
        .with_context(|| "Failed to open Cargo.toml for appending")?;

    // Ensure [patch.crates-io] section exists
    if !cargo_toml_content.contains("[patch.crates-io]") {
        writeln!(cargo_toml, "\n[patch.crates-io]")
            .with_context(|| "Failed to write to Cargo.toml")?;
    }

    // Track crates that were updated
    let mut updated_crates = Vec::new();

    // Add patches for crates that are referenced but not already patched
    for crate_entry in crates {
        if referenced_crates.contains(&crate_entry.name)
            && !existing_patches.contains(&crate_entry.name)
        {
            let patch_line = format!(
                "{} = {{ git = \"{}\", branch = \"main\" }}",
                crate_entry.name, crate_entry.repo_url
            );
            writeln!(cargo_toml, "{}", patch_line)
                .with_context(|| "Failed to write to Cargo.toml")?;
            updated_crates.push(crate_entry.clone()); // Clone `crate_entry` properly
        }
    }

    Ok(updated_crates)
}

fn parse_referenced_crates(cargo_toml_content: &str) -> Result<HashSet<String>> {
    let mut referenced_crates = HashSet::new();

    // Parse [dependencies] and [dev-dependencies] sections
    let toml: toml::Value =
        toml::from_str(cargo_toml_content).with_context(|| "Failed to parse Cargo.toml")?;

    if let Some(dependencies) = toml.get("dependencies") {
        if let Some(deps) = dependencies.as_table() {
            for crate_name in deps.keys() {
                referenced_crates.insert(crate_name.to_string());
            }
        }
    }

    if let Some(dev_dependencies) = toml.get("dev-dependencies") {
        if let Some(deps) = dev_dependencies.as_table() {
            for crate_name in deps.keys() {
                referenced_crates.insert(crate_name.to_string());
            }
        }
    }

    Ok(referenced_crates)
}

fn parse_existing_patches(cargo_toml_content: &str) -> Result<HashSet<String>> {
    let mut existing_patches = HashSet::new();

    // Parse [patch.crates-io] section
    let toml: toml::Value =
        toml::from_str(cargo_toml_content).with_context(|| "Failed to parse Cargo.toml")?;

    if let Some(patch) = toml.get("patch") {
        if let Some(crates_io) = patch.get("crates-io") {
            if let Some(patches) = crates_io.as_table() {
                for crate_name in patches.keys() {
                    existing_patches.insert(crate_name.to_string());
                }
            }
        }
    }

    Ok(existing_patches)
}

fn commit_changes(updated_crates: &[Crate]) -> Result<()> {
    // Generate the commit message body (same as PR body)
    let commit_body = format!(
        "This PR updates the following dependencies to use their main branches:\n\n{}",
        updated_crates
            .iter()
            .map(|c| format!("- `{}` from `{}`", c.name, c.repo_url))
            .collect::<Vec<_>>()
            .join("\n")
    );

    // Combine the first line and body into the full commit message
    let commit_message = format!(
        "chore: add patch for `iroh` dependencies\n\n{}",
        commit_body
    );

    // Stage the changes
    Cmd::new("git")
        .args(["add", "Cargo.toml"])
        .status()
        .with_context(|| "Failed to stage changes")?;

    // Commit the changes with the formatted message
    Cmd::new("git")
        .args(["commit", "-m", &commit_message])
        .status()
        .with_context(|| "Failed to commit changes")?;

    Ok(())
}

fn push_branch(branch_name: &str) -> Result<()> {
    Cmd::new("git")
        .args(["push", "origin", branch_name])
        .status()
        .with_context(|| "Failed to push branch")?;
    Ok(())
}

fn create_pull_request(branch_name: &str, relevant_crates: &[Crate]) -> Result<()> {
    // Generate the PR body with the list of patched dependencies
    let pr_body = format!(
        "This PR updates the following dependencies to use their main branches:\n\n{}",
        relevant_crates
            .iter()
            .map(|c| format!("- `{}` from `{}`", c.name, c.repo_url))
            .collect::<Vec<_>>()
            .join("\n")
    );

    Cmd::new("gh")
        .args([
            "pr",
            "create",
            "--title",
            "Patch crates to use main branch of iroh dependencies",
            "--body",
            &pr_body,
            "--base",
            "main",
            "--head",
            branch_name,
        ])
        .status()
        .with_context(|| "Failed to create pull request")?;
    Ok(())
}

fn cleanup_branches(directories: &[PathBuf]) -> Result<()> {
    info!("Cleaning up patch-iroh-main branches in all directories...");
    for dir in directories {
        info!("Cleaning up in {}", dir.display());
        if std::env::set_current_dir(dir).is_ok() {
            Cmd::new("git")
                .args(["branch", "-D", "patch-iroh-main"])
                .status()
                .ok();
            Cmd::new("git")
                .args(["push", "origin", "--delete", "patch-iroh-main"])
                .status()
                .ok();
        }
    }
    info!("Branches cleaned up.");
    Ok(())
}
