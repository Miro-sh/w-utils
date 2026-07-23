//! wcp — cp(1) GNU avec barre de progression (suite w-utils).

mod cli;

use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;
use humansize::{format_size, DECIMAL};
use w_utils::{copy, progress, utils};

use cli::{Args, Config, CopyMode, Overwrite};
use copy::{CopyOptions, PlanEntry};
use progress::CopyProgress;
use utils::{format_duration, is_interactive, print_error, print_success, print_warn};

fn main() {
    let args = Args::parse();

    // Sorties spéciales : page man et complétions (utilisées par le packaging).
    if args.generate_man {
        let mut out = std::io::stdout();
        // Erreur ignorée : un pipe fermé en amont (| head) ne doit pas paniquer.
        let _ = clap_mangen::Man::new(cli::build_cli()).render(&mut out);
        return;
    }
    if let Some(shell) = args.generate_completions {
        let mut out = std::io::stdout();
        clap_complete::generate(shell, &mut cli::build_cli(), "wcp", &mut out);
        return;
    }

    copy::install_ctrlc_handler();

    if let Err(err) = run(&args) {
        print_error(&format!("{:#}", err));
        std::process::exit(1);
    }
}

fn run(args: &Args) -> Result<()> {
    let cfg = args.resolve()?;

    if cfg.explicit_context {
        print_warn("l'attribut 'context' (SELinux/SMACK) n'est pas géré : ignoré");
    }

    // Pas de barre pour les modes sans données (liens, attributs seuls),
    // ni en sortie JSON (destinée aux scripts).
    let show_progress = match cfg.progress {
        Some(forced) => forced,
        None => is_interactive(),
    } && cfg.mode == CopyMode::Copy
        && !cfg.attributes_only
        && !cfg.json;

    // -j : auto = nombre de cœurs (plafonné à 8) ; -i reste séquentiel
    // (les questions se posent une par une).
    let jobs = if cfg.jobs == 1 || cfg.overwrite == Overwrite::Interactive {
        1
    } else if cfg.jobs == 0 {
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(8)
    } else {
        cfg.jobs
    };

    // 1. Analyse des sources et construction du plan de copie.
    let plan_cfg = copy::PlanConfig {
        recursive: cfg.recursive,
        deref: cfg.deref,
        parents: cfg.parents,
        one_file_system: cfg.one_file_system,
        copy_contents: cfg.copy_contents,
        preserve_links: cfg.preserve.links,
        dest_never_dir: cfg.dest_never_dir,
        remove_destination: cfg.remove_destination,
        exclude: build_exclude_matcher(&cfg)?,
    };
    let plan = copy::build_plan(&cfg.sources, &cfg.destination, &plan_cfg)?;

    // Erreurs d'analyse (source absente, répertoire sans -r...) : affichées
    // comme cp, mais elles n'empêchent pas le reste du plan de s'exécuter.
    for error in &plan.errors {
        print_error(error);
    }

    // --dry-run : on affiche le plan et on s'arrête là, rien n'est écrit.
    if cfg.dry_run {
        if cfg.json {
            println!("{}", json_plan(&plan));
        } else {
            print_dry_run(&plan, &cfg);
        }
        return if plan.errors.is_empty() {
            Ok(())
        } else {
            anyhow::bail!("{} erreur(s) pendant l'analyse", plan.errors.len())
        };
    }

    // 2. Vérification de l'espace disque AVANT d'écrire quoi que ce soit.
    if cfg.mode == CopyMode::Copy && !cfg.attributes_only {
        copy::check_disk_space(plan.total_bytes, &cfg.destination)?;
    }

    // 3. Copie.
    let progress = CopyProgress::new(plan.total_bytes, show_progress);
    let opts = CopyOptions {
        mode: cfg.mode,
        overwrite: cfg.overwrite,
        preserve: cfg.preserve,
        backup: cfg.backup,
        backup_suffix: cfg.backup_suffix.clone(),
        reflink: cfg.reflink,
        sparse: cfg.sparse,
        remove_destination: cfg.remove_destination,
        attributes_only: cfg.attributes_only,
        verbose: cfg.verbose,
        resume: cfg.resume,
        verify: cfg.verify,
        jobs,
        bwlimit: cfg.bwlimit,
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

    if cfg.json {
        println!("{}", json_summary(&plan, &stats, elapsed));
    } else if is_interactive() || cfg.verbose {
        print_summary(&plan, &stats, elapsed);
    }

    let total_errors = stats.errors.len() + plan.errors.len();
    if total_errors > 0 {
        anyhow::bail!("{} erreur(s) pendant la copie", total_errors);
    }
    Ok(())
}

/// Construit le matcher --exclude / --exclude-from.
fn build_exclude_matcher(cfg: &Config) -> Result<Option<globset::GlobSet>> {
    copy::build_exclude_matcher(&cfg.exclude, &cfg.exclude_from)
}

// ---------------------------------------------------------------------------
// Sortie JSON (--json)
// ---------------------------------------------------------------------------

use utils::{json_escape, json_string_array};

fn json_summary(plan: &copy::CopyPlan, stats: &copy::CopyStats, elapsed: Duration) -> String {
    format!(
        "{{\"files_copied\":{},\"dirs_created\":{},\"bytes_copied\":{},\"already_present\":{},\
\"skipped\":{},\"excluded\":{},\"verified\":{},\"duration_secs\":{:.3},\"warnings\":{},\"errors\":{}}}",
        stats.files_copied,
        stats.dirs_created,
        stats.bytes_copied,
        stats.already_present,
        stats.skipped,
        plan.excluded,
        stats.verified,
        elapsed.as_secs_f64(),
        json_string_array(&stats.warnings),
        json_string_array(&stats.errors)
    )
}

fn json_plan(plan: &copy::CopyPlan) -> String {
    let mut entries = String::from("[");
    let mut first = true;
    for e in &plan.entries {
        let (action, src, dst, size) = match e {
            PlanEntry::File { src, dst, size, .. } => ("copy", src, dst, *size),
            PlanEntry::Symlink { src, dst } => ("symlink", src, dst, 0),
            PlanEntry::Dir { src, dst } => ("mkdir", src, dst, 0),
            PlanEntry::Fifo { src, dst } => ("fifo", src, dst, 0),
            PlanEntry::Special { src, dst } => ("special", src, dst, 0),
            PlanEntry::HardLink { link, dst } => ("hardlink", link, dst, 0),
        };
        if !first {
            entries.push(',');
        }
        first = false;
        entries.push_str(&format!(
            "{{\"action\":\"{}\",\"src\":\"{}\",\"dst\":\"{}\",\"size\":{}}}",
            action,
            json_escape(&src.display().to_string()),
            json_escape(&dst.display().to_string()),
            size
        ));
    }
    entries.push(']');
    format!(
        "{{\"entries\":{},\"total_bytes\":{},\"file_count\":{},\"excluded\":{}}}",
        entries, plan.total_bytes, plan.file_count, plan.excluded
    )
}

/// --dry-run : affiche le plan sans toucher au disque, en signalant les
/// fichiers qui seraient écrasés ou ignorés.
fn print_dry_run(plan: &copy::CopyPlan, cfg: &Config) {
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
            PlanEntry::HardLink { link, dst } => {
                println!("lien dur    {} == {}", link.display(), dst.display());
            }
            PlanEntry::Fifo { src, dst } => {
                println!("fifo        {} -> {}", src.display(), dst.display());
            }
            PlanEntry::Special { src, dst } => {
                println!("spécial     {} -> {}", src.display(), dst.display());
            }
            PlanEntry::File { src, dst, size, .. } => {
                let exists = copy::would_overwrite(dst);
                if exists && cfg.overwrite == Overwrite::NoClobber {
                    println!("ignoré      {} (existe déjà)", dst.display());
                } else if cfg.resume && copy::is_up_to_date(dst, *size) {
                    println!("présent     {} -> {} (ignoré)", src.display(), dst.display());
                } else if exists {
                    overwrites += 1;
                    let backup = if cfg.backup.is_some() { " (sauvegarde)" } else { "" };
                    println!(
                        "{}",
                        format!(
                            "écraser     {} -> {} ({}){backup}",
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
    if plan.excluded > 0 {
        println!("{} élément(s) exclu(s)", plan.excluded);
    }
}

/// Résumé final : ligne détaillée pour un fichier unique, agrégée sinon.
fn print_summary(plan: &copy::CopyPlan, stats: &copy::CopyStats, elapsed: Duration) {
    if plan.file_count == 1
        && stats.errors.is_empty()
        && stats.already_present == 0
        && stats.skipped == 0
    {
        if let Some(PlanEntry::File { src, dst, size, .. }) =
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
    if stats.skipped > 0 {
        line.push_str(&format!(", {} ignoré(s)", stats.skipped));
    }
    if plan.excluded > 0 {
        line.push_str(&format!(", {} exclu(s)", plan.excluded));
    }
    if stats.verified > 0 {
        line.push_str(&format!(", {} vérifié(s)", stats.verified));
    }
    if !stats.warnings.is_empty() {
        line.push_str(&format!(", {} avertissement(s)", stats.warnings.len()));
    }
    if !stats.errors.is_empty() {
        line.push_str(&format!(", {} erreur(s)", stats.errors.len()));
    }
    print_success(&line);
}
