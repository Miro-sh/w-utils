//! bcp — Better CP : une version moderne de `cp` avec barre de progression.

mod cli;
mod copy;
mod progress;
mod utils;

use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;
use humansize::{format_size, DECIMAL};

use cli::Args;
use copy::{CopyOptions, PlanEntry};
use progress::CopyProgress;
use utils::{format_duration, is_interactive, print_error, print_success, print_warn};

fn main() {
    let args = Args::parse();

    // Sortie spéciale : génération de la page man (utilisée par le packaging).
    if args.generate_man {
        let mut out = std::io::stdout();
        clap_mangen::Man::new(cli::build_cli())
            .render(&mut out)
            .expect("échec du rendu de la page man");
        return;
    }

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
    // (clap garantit leur présence sauf pour --generate-man, déjà traité.)
    let source = args.source.as_deref().expect("source manquante");
    let destination = args.destination.as_deref().expect("destination manquante");

    let plan = copy::build_plan(source, destination, args.recursive)?;

    // 2. Vérification de l'espace disque AVANT d'écrire quoi que ce soit.
    copy::check_disk_space(plan.total_bytes, destination)?;

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
