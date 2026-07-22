//! wcp — une version moderne de `cp` avec barre de progression (suite w-utils).

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

    // --dry-run : on affiche le plan et on s'arrête là, rien n'est écrit.
    if args.dry_run {
        print_dry_run(&plan, args.resume);
        return Ok(());
    }

    // 2. Vérification de l'espace disque AVANT d'écrire quoi que ce soit.
    copy::check_disk_space(plan.total_bytes, destination)?;

    // 3. Copie.
    let progress = CopyProgress::new(plan.total_bytes, show_progress);
    let opts = CopyOptions {
        archive: args.archive,
        verbose: args.verbose,
        resume: args.resume,
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

/// --dry-run : affiche le plan sans toucher au disque, en signalant les
/// fichiers qui seraient écrasés.
fn print_dry_run(plan: &copy::CopyPlan, resume: bool) {
    use colored::Colorize;

    let mut dirs = 0usize;
    let mut overwrites = 0usize;

    for entry in &plan.entries {
        match entry {
            PlanEntry::Dir { dst, .. } => {
                dirs += 1;
                if !dst.exists() {
                    println!("créer dir   {}", dst.display());
                }
            }
            PlanEntry::Symlink { src, dst } => {
                println!("lien        {} -> {}", src.display(), dst.display());
            }
            PlanEntry::File { src, dst, size } => {
                if resume && copy::is_up_to_date(dst, *size) {
                    println!("présent     {} -> {} (ignoré)", src.display(), dst.display());
                } else if copy::would_overwrite(dst) {
                    overwrites += 1;
                    println!(
                        "{}",
                        format!(
                            "écraser     {} -> {} ({})",
                            src.display(),
                            dst.display(),
                            format_size(*size, DECIMAL)
                        )
                        .yellow()
                    );
                } else {
                    println!(
                        "copier      {} -> {} ({})",
                        src.display(),
                        dst.display(),
                        format_size(*size, DECIMAL)
                    );
                }
            }
        }
    }

    let summary = format!(
        "{} fichier(s), {} répertoire(s), {} au total",
        plan.file_count,
        dirs,
        format_size(plan.total_bytes, DECIMAL)
    );
    // Flush avant les warnings stderr pour garder l'ordre dans un pipe.
    use std::io::Write;
    let _ = std::io::stdout().flush();
    if overwrites > 0 {
        print_warn(&format!("{summary} — attention : {overwrites} écrasement(s)"));
    } else {
        println!("{summary}");
    }
    if !plan.skipped.is_empty() {
        print_warn(&format!("{} fichier(s) spéciaux ignoré(s)", plan.skipped.len()));
    }
}

/// Résumé final : ligne détaillée pour un fichier unique, agrégée sinon.
fn print_summary(plan: &copy::CopyPlan, stats: &copy::CopyStats, elapsed: Duration) {
    if plan.file_count == 1 && stats.errors.is_empty() && stats.already_present == 0 {
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
    let mut line = format!(
        "{} élément(s) copié(s) ({}) en {} — {}",
        stats.files_copied + stats.dirs_created,
        format_size(stats.bytes_copied, DECIMAL),
        format_duration(elapsed),
        speed
    );
    if stats.already_present > 0 {
        line.push_str(&format!(", {} déjà présent(s)", stats.already_present));
    }
    if !stats.warnings.is_empty() {
        line.push_str(&format!(", {} ignoré(s)", stats.warnings.len()));
    }
    if !stats.errors.is_empty() {
        line.push_str(&format!(", {} erreur(s)", stats.errors.len()));
    }
    print_success(&line);
}
