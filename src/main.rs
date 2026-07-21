//! bcp — Better CP : une version moderne de `cp` avec barre de progression.

mod copy;
mod progress;
mod utils;

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;
use humansize::{format_size, DECIMAL};

use copy::{CopyOptions, PlanEntry};
use progress::CopyProgress;
use utils::{format_duration, is_interactive, print_error, print_success, print_warn};

#[derive(Parser, Debug)]
#[command(
    name = "bcp",
    version,
    about = "Better CP — une version moderne de cp avec barre de progression"
)]
struct Args {
    /// Fichier ou répertoire source
    source: PathBuf,

    /// Fichier ou répertoire de destination
    destination: PathBuf,

    /// Copie récursive (requis pour les répertoires, comme cp -r)
    #[arg(short, long)]
    recursive: bool,

    /// Force l'affichage de la barre de progression
    #[arg(long, conflicts_with = "no_progress")]
    progress: bool,

    /// Désactive la barre de progression (mode silencieux, pour les scripts)
    #[arg(long)]
    no_progress: bool,

    /// Mode verbeux : affiche chaque fichier copié (comme cp -v)
    #[arg(short, long)]
    verbose: bool,

    /// Mode archive : préserve permissions et horodatages
    #[arg(short, long)]
    archive: bool,
}

fn main() {
    let args = Args::parse();
    copy::install_ctrlc_handler();

    if let Err(err) = run(&args) {
        print_error(&format!("{:#}", err));
        std::process::exit(1);
    }
}

fn run(args: &Args) -> Result<()> {
    let interactive = is_interactive();
    // Priorité : --no-progress > --progress > auto (terminal interactif).
    let show_progress = if args.no_progress {
        false
    } else if args.progress {
        true
    } else {
        interactive
    };

    // 1. Analyse de la source et construction du plan de copie.
    let plan = copy::build_plan(&args.source, &args.destination, args.recursive)?;

    // 2. Vérification de l'espace disque AVANT d'écrire quoi que ce soit.
    copy::check_disk_space(plan.total_bytes, &args.destination)?;

    // 3. Copie.
    let progress = CopyProgress::new(plan.total_bytes, show_progress);
    let opts = CopyOptions {
        archive: args.archive,
        verbose: args.verbose,
    };

    let start = Instant::now();
    let stats = copy::execute_plan(&plan, &opts, &progress)?;
    let elapsed = start.elapsed();
    progress.finish();

    // 4. Compte-rendu.
    for warning in &stats.warnings {
        print_warn(warning);
    }
    for error in &stats.errors {
        print_error(error);
    }

    if interactive || args.verbose {
        print_summary(&plan, &stats, elapsed);
    }

    if !stats.errors.is_empty() {
        anyhow::bail!("{} erreur(s) pendant la copie", stats.errors.len());
    }
    Ok(())
}

/// Résumé final : ligne détaillée pour un fichier unique, agrégée sinon.
fn print_summary(plan: &copy::CopyPlan, stats: &copy::CopyStats, elapsed: Duration) {
    if plan.file_count == 1 && stats.errors.is_empty() {
        if let Some(PlanEntry::File { src, dst, size }) =
            plan.entries.iter().find(|e| matches!(e, PlanEntry::File { .. }))
        {
            print_success(&format!(
                "Copié {} → {} ({})",
                src.display(),
                dst.display(),
                format_size(*size, DECIMAL)
            ));
        }
        return;
    }

    let secs = elapsed.as_secs_f64();
    let speed = if secs > 0.001 {
        format!("{}/s", format_size((stats.bytes_copied as f64 / secs) as u64, DECIMAL))
    } else {
        String::from("—")
    };
    print_success(&format!(
        "{} élément(s) copié(s) ({}) en {} — {}",
        stats.files_copied + stats.dirs_created,
        format_size(stats.bytes_copied, DECIMAL),
        format_duration(elapsed),
        speed
    ));
}
