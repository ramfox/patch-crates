use anyhow::{bail, Context, Result};
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
    /// Update each main
    Update,
    /// Reset main
    Reset,
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
        Commands::Update => update_and_check(&config.directories, &config.crates)?,
        Commands::Reset => reset(&config.directories)?,
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

    // If there are updated crates, update deny.toml if it exists
    if !updated_crates.is_empty() {
        // Run `cargo update` to update dependencies
        info!("Running `cargo update`...");
        cargo_update(&updated_crates)?;

        // Check if deny.toml exists and update it
        update_deny_toml(&updated_crates)?;

        // Commit changes
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
            .cloned()
            .collect();

        create_pull_request(branch_name, &all_relevant_crates)?;
        info!("Pull request created!");
    } else {
        info!("Dry run complete. Changes were committed but not pushed.");
    }
    Ok(())
}

fn create_and_checkout_branch(branch_name: &str) -> Result<()> {
    // Checkout the `main` branch
    info!("Checking out the `main` branch...");
    Cmd::new("git")
        .args(["checkout", "main"])
        .status()
        .with_context(|| "Failed to checkout `main` branch")?;

    // Pull the latest changes from `origin/main`
    info!("Pulling latest changes from `origin/main`...");
    Cmd::new("git")
        .args(["pull", "origin", "main"])
        .status()
        .with_context(|| "Failed to pull from `origin/main`")?;

    Cmd::new("git")
        .args(["checkout", "-b", branch_name])
        .status()
        .with_context(|| "Failed to create and checkout branch")?;
    Ok(())
}

fn cargo_update(updated_crates: &Vec<Crate>) -> anyhow::Result<()> {
    // Start building the command
    let mut cmd = Cmd::new("cargo");
    cmd.arg("update");

    // Add each crate to the command with the `--package` flag
    for krate in updated_crates {
        cmd.arg("--package").arg(&krate.name);
    }

    // Execute the command
    cmd.status()
        .with_context(|| "Failed to run `cargo update`")?;

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
        .args(["add", "Cargo.toml", "Cargo.lock"])
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
            "chore: patch to use main branch of iroh dependencies",
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
                .args(["checkout", "main"])
                .status()
                .with_context(|| "Failed to checkout `main` branch")?;

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

fn update_and_check(directories: &[PathBuf], crates: &Vec<Crate>) -> Result<()> {
    info!("");
    let mut successes = vec![];
    let mut main_failures = vec![];
    let mut update_failures = vec![];
    let mut check_failures = vec![];
    for dir in directories {
        let dir_name = dir.file_name().expect("checked").to_str().expect("checked");
        println!("Updating and checking {dir_name} on `main` branch");
        if std::env::set_current_dir(dir).is_ok() {
            if let Err(e) = checkout_and_pull() {
                error!("{e:?}");
                main_failures.push(dir_name);
                continue;
            };
            let referenced_crates = match list_relevant_crates(crates) {
                Err(e) => {
                    error!("{e:?}");
                    update_failures.push(dir_name);
                    continue;
                }
                Ok(r) => r,
            };
            if let Err(e) = cargo_update(&referenced_crates) {
                error!("Unable to run `cargo update` on {dir_name}: {e:?}");
                update_failures.push(dir_name);
                continue;
            }
            if let Err(e) = cargo_check() {
                error!("Error running `cargo check` for {dir_name}: {e:?}");
                check_failures.push(dir_name);
                continue;
            }
            successes.push(dir_name);
        }
    }

    if !successes.is_empty() {
        info!("repos successfully updated and checked:");
        for repo in successes {
            info!("\t{repo}");
        }
    }

    if !main_failures.is_empty() {
        info!("repos that could not checkout `main`:");
        for repo in main_failures {
            info!("\t{repo}");
        }
    }

    if !update_failures.is_empty() {
        info!("repos that did not run `cargo update` successfully:");
        for repo in update_failures {
            info!("\t{repo}");
        }
    }

    if !check_failures.is_empty() {
        info!("repos that had an error in `cargo check`:");
        for repo in check_failures {
            info!("\t{repo}");
        }
    }
    Ok(())
}

fn list_relevant_crates(crates: &[Crate]) -> Result<Vec<Crate>> {
    let cargo_toml_path = Path::new("Cargo.toml");
    let cargo_toml_content =
        fs::read_to_string(cargo_toml_path).with_context(|| "Failed to read Cargo.toml")?;

    // Parse Cargo.toml to find referenced dependencies
    let referenced_crates = parse_referenced_crates(&cargo_toml_content)?;
    let mut relevant_crates = vec![];
    for krate in crates {
        if referenced_crates.contains(&krate.name) {
            relevant_crates.push(krate.clone());
        }
    }
    Ok(relevant_crates)
}

fn cargo_check() -> Result<()> {
    let output = Cmd::new("cargo")
        .args(["check", "--all-targets", "--all-features"])
        .output()
        .with_context(|| "Failed to run `cargo check`")?;
    if !output.status.success() {
        bail!("`cargo check` failed with errors");
    }
    Ok(())
}

fn checkout_and_pull() -> Result<()> {
    info!("Checking out `main`");
    // Checkout main
    Cmd::new("git")
        .args(["checkout", "main"])
        .status()
        .with_context(|| "Failed to checkout `main`")?;
    // Pull the latest changes from `origin/main`
    info!("Pulling latest changes from `origin/main`...");
    Cmd::new("git")
        .args(["pull", "origin", "main"])
        .status()
        .with_context(|| "Failed to pull from `origin/main`")?;
    Ok(())
}

fn reset(directories: &[PathBuf]) -> Result<()> {
    let mut failures = vec![];
    let mut successes = vec![];
    for dir in directories {
        let dir_name = dir.file_name().expect("checked").to_str().expect("checked");
        println!("Reseting {dir_name}");
        if std::env::set_current_dir(dir).is_ok() {
            if let Err(e) = Cmd::new("git")
                .arg("reset")
                .arg("--hard")
                .status()
                .with_context(|| "Failed to run `cargo reset --hard`")
            {
                error!("{e:?}");
                failures.push(dir_name);
                continue;
            }
            successes.push(dir_name);
        }
    }

    if !successes.is_empty() {
        info!("repos successfully reset:");
        for repo in successes {
            info!("\t{repo}");
        }
    }

    if !failures.is_empty() {
        info!("repos that could not reset:");
        for repo in failures {
            info!("\t{repo}");
        }
    }
    Ok(())
}

fn update_deny_toml(updated_crates: &[Crate]) -> Result<()> {
    let deny_toml_path = Path::new("deny.toml");

    // Check if deny.toml exists
    if !deny_toml_path.exists() {
        info!("No deny.toml file found. Skipping update.");
        return Ok(());
    }

    // Read the existing deny.toml content
    let deny_toml_content =
        fs::read_to_string(deny_toml_path).with_context(|| "Failed to read deny.toml")?;

    // Parse the deny.toml file
    let mut deny_toml: toml::Value =
        toml::from_str(&deny_toml_content).with_context(|| "Failed to parse deny.toml")?;

    // Extract the list of unique git repo URLs from the updated crates
    let mut git_repos: HashSet<String> =
        updated_crates.iter().map(|c| c.repo_url.clone()).collect();

    // Check if the `sources.allow-git` section already exists
    if let Some(sources) = deny_toml.get_mut("sources") {
        if let Some(allow_git) = sources.get_mut("allow-git") {
            if let Some(existing_repos) = allow_git.as_array() {
                // Add existing repos to the set to deduplicate
                for repo in existing_repos {
                    if let Some(repo_str) = repo.as_str() {
                        git_repos.insert(repo_str.to_string());
                    }
                }
            }
        }
    }

    // Create or update the `sources.allow-git` section
    let allow_git_value = toml::Value::Array(
        git_repos
            .into_iter()
            .map(|repo| toml::Value::String(repo))
            .collect(),
    );

    deny_toml
        .as_table_mut()
        .unwrap()
        .entry("sources")
        .or_insert_with(|| toml::Value::Table(toml::value::Table::new()))
        .as_table_mut()
        .unwrap()
        .insert("allow-git".to_string(), allow_git_value);

    // Write the updated deny.toml back to the file
    let updated_deny_toml_content =
        toml::to_string_pretty(&deny_toml).with_context(|| "Failed to serialize deny.toml")?;
    fs::write(deny_toml_path, updated_deny_toml_content)
        .with_context(|| "Failed to write deny.toml")?;

    info!("Updated deny.toml with allowed git repositories.");
    Ok(())
}
