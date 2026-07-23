//! wmv — mv(1) GNU avec barre de progression sur les moves cross-filesystem.
//!
//! Même système de fichiers : rename(2) instantané, comme mv. Sinon,
//! fallback copie + suppression avec tout le confort de wcp : progression,
//! copies atomiques, --verify avant de supprimer la source, -j parallèle.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Result};
use clap::{CommandFactory, Parser};
use humansize::{format_size, DECIMAL};
use w_utils::mvops::{self, MoveOptions, MoveStats, PlannedAction};
use w_utils::options::{BackupControl, Overwrite, UpdateWhen};
use w_utils::copy::SourceSpec;
use w_utils::utils::{
    format_duration, is_interactive, json_escape, json_string_array, print_error, print_success, print_warn,
};

#[derive(Parser, Debug)]
#[command(
    name = "wmv",
    version,
    about = "wmv — mv(1) moderne avec barre de progression",
    long_about = "wmv déplace fichiers et répertoires comme mv(1) GNU : rename(2) \
instantané sur un même système de fichiers, copie + suppression sinon. Sur les \
moves cross-filesystem, wmv ajoute une barre de progression en direct, des copies \
atomiques (jamais de fichier partiel), et --verify pour contrôler chaque octet \
AVANT de supprimer la source."
)]
struct Args {
    /// Sources puis destination (dernier argument) ; avec -t, tout est source
    #[arg(value_name = "SOURCE... DEST", num_args = 1..,
          required_unless_present_any = ["generate_man", "generate_completions"])]
    paths: Vec<PathBuf>,

    /// Forcer l'écrasement sans demander (implicite : copies atomiques)
    #[arg(short, long)]
    force: bool,

    /// Demander avant d'écraser un fichier existant
    #[arg(short = 'i', long, overrides_with_all = ["no_clobber", "update"])]
    interactive: bool,

    /// Ne jamais écraser un fichier existant
    #[arg(short = 'n', long, overrides_with_all = ["interactive", "update"])]
    no_clobber: bool,

    /// N'écraser que si la source est plus récente (WHEN : all, none, none-fail, older)
    #[arg(short, long, value_name = "WHEN", num_args = 0..=1, require_equals = true,
          default_missing_value = "older",
          overrides_with_all = ["interactive", "no_clobber", "update"])]
    update: Option<UpdateWhen>,

    /// Sauvegarder chaque fichier écrasé (WHEN : none, numbered, existing, simple)
    #[arg(short = 'b', long, value_name = "WHEN", num_args = 0..=1, require_equals = true,
          default_missing_value = "existing", overrides_with = "backup")]
    backup: Option<BackupControl>,

    /// Suffixe des sauvegardes simples (défaut ~, env SIMPLE_BACKUP_SUFFIX)
    #[arg(short = 'S', long, value_name = "SUFFIXE")]
    suffix: Option<String>,

    /// Déplacer toutes les sources dans RÉP
    #[arg(short = 't', long, value_name = "RÉP", conflicts_with = "no_target_directory")]
    target_directory: Option<PathBuf>,

    /// Traiter la destination comme un fichier normal (jamais un répertoire)
    #[arg(short = 'T', long)]
    no_target_directory: bool,

    /// Retirer les « / » finaux des arguments sources
    #[arg(long)]
    strip_trailing_slashes: bool,

    /// Afficher chaque fichier déplacé
    #[arg(short, long)]
    verbose: bool,

    // --- Extensions wmv ---

    /// Forcer la barre de progression
    #[arg(long, conflicts_with = "no_progress")]
    progress: bool,

    /// Désactiver la barre de progression (pour les scripts)
    #[arg(long)]
    no_progress: bool,

    /// Afficher le plan (rename instantané ou copie) sans rien toucher
    #[arg(long)]
    dry_run: bool,

    /// Ignorer les fichiers déjà déplacés (reprise après interruption)
    #[arg(long)]
    resume: bool,

    /// Vérifier chaque fichier par somme xxh3 AVANT de supprimer la source
    #[arg(long)]
    verify: bool,

    /// Copies en parallèle lors d'un move cross-device (0 = auto)
    #[arg(short = 'j', long, value_name = "N", default_value = "0")]
    jobs: usize,

    /// Résumé final en JSON sur stdout (pour les scripts)
    #[arg(long)]
    json: bool,

    /// Exclure les fichiers correspondant à ce motif glob (répétable)
    #[arg(long, value_name = "MOTIF")]
    exclude: Vec<String>,

    /// Lire des motifs d'exclusion depuis un fichier (répétable)
    #[arg(long, value_name = "FICHIER")]
    exclude_from: Vec<PathBuf>,

    /// Limiter le débit de la phase copie (ex. 10m, 512k, 1.5g)
    #[arg(long, value_name = "DÉBIT")]
    bwlimit: Option<String>,

    /// Génère les complétions shell sur stdout (bash, zsh, fish...)
    #[arg(long, hide = true, value_name = "SHELL")]
    generate_completions: Option<clap_complete::Shell>,

    /// Génère la page man sur stdout (usage interne au packaging)
    #[arg(long, hide = true)]
    generate_man: bool,
}

fn build_cli() -> clap::Command {
    Args::command()
}

fn main() {
    let args = Args::parse();

    if args.generate_man {
        let mut out = std::io::stdout();
        let _ = clap_mangen::Man::new(build_cli()).render(&mut out);
        return;
    }
    if let Some(shell) = args.generate_completions {
        let mut out = std::io::stdout();
        clap_complete::generate(shell, &mut build_cli(), "wmv", &mut out);
        return;
    }

    w_utils::copy::install_ctrlc_handler();

    if let Err(err) = run(&args) {
        print_error(&format!("{:#}", err));
        std::process::exit(1);
    }
}

fn run(args: &Args) -> Result<()> {
    // 1. Sources et destination (sémantique mv).
    let (raw_sources, destination) = if let Some(dir) = &args.target_directory {
        if !dir.is_dir() {
            bail!("le répertoire cible '{}' n'existe pas", dir.display());
        }
        (args.paths.clone(), dir.clone())
    } else {
        if args.paths.len() < 2 {
            bail!(
                "opérande de destination manquante après '{}'",
                args.paths.first().map(|p| p.display().to_string()).unwrap_or_default()
            );
        }
        let (dest, srcs) = args.paths.split_last().expect("au moins 2 opérandes");
        if srcs.len() > 1 {
            if args.no_target_directory {
                bail!("opérande supplémentaire '{}'", srcs[1].display());
            }
            if !dest.is_dir() {
                if dest.symlink_metadata().is_ok() {
                    bail!("la cible '{}' n'est pas un répertoire", dest.display());
                }
                bail!("la cible '{}' n'existe pas", dest.display());
            }
        }
        (srcs.to_vec(), dest.clone())
    };

    // 2. Politique d'écrasement (clap a appliqué « le dernier gagne »).
    let overwrite = if args.interactive {
        Overwrite::Interactive
    } else if args.no_clobber {
        Overwrite::NoClobber
    } else {
        match args.update {
            Some(UpdateWhen::All) => Overwrite::Clobber,
            Some(UpdateWhen::None) => Overwrite::NoClobber,
            Some(UpdateWhen::NoneFail) => Overwrite::NoClobberFail,
            Some(UpdateWhen::Older) => Overwrite::Update,
            None => Overwrite::Clobber,
        }
    };

    let backup = match args.backup {
        Some(BackupControl::None) => None,
        other => other,
    };
    let backup_suffix = args
        .suffix
        .clone()
        .or_else(|| std::env::var("SIMPLE_BACKUP_SUFFIX").ok())
        .unwrap_or_else(|| String::from("~"));

    let sources: Vec<SourceSpec> = raw_sources
        .into_iter()
        .map(|path| {
            let trailing = path
                .as_os_str()
                .to_string_lossy()
                .ends_with(std::path::MAIN_SEPARATOR);
            SourceSpec { path, follow: trailing && !args.strip_trailing_slashes }
        })
        .collect();

    let bwlimit = match &args.bwlimit {
        Some(s) => Some(w_utils::utils::parse_rate(s).map_err(|e| anyhow::anyhow!("--bwlimit : {e}"))?),
        None => None,
    };

    let jobs = if args.jobs == 1 || overwrite == Overwrite::Interactive {
        1
    } else if args.jobs == 0 {
        std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).min(8)
    } else {
        args.jobs
    };

    let show_progress = match (args.progress, args.no_progress) {
        (true, _) => true,
        (_, true) => false,
        _ => is_interactive(),
    } && !args.json;

    let opts = MoveOptions {
        overwrite,
        backup,
        backup_suffix,
        dest_never_dir: args.no_target_directory,
        verbose: args.verbose,
        resume: args.resume,
        verify: args.verify,
        jobs,
        bwlimit,
        exclude: w_utils::copy::build_exclude_matcher(&args.exclude, &args.exclude_from)?,
        show_progress,
    };

    // --dry-run : annonce rename instantané ou copie + suppression, rien de plus.
    if args.dry_run {
        return print_dry_run(&sources, &destination, &opts, args.json);
    }

    let start = Instant::now();
    let stats = mvops::run_move(&sources, &destination, &opts);
    let elapsed = start.elapsed();

    for error in &stats.errors {
        print_error(error);
    }

    if args.json {
        println!("{}", json_summary(&stats, elapsed));
    } else if is_interactive() || args.verbose {
        print_summary(&stats, elapsed);
    }

    if !stats.errors.is_empty() {
        bail!("{} erreur(s) pendant le déplacement", stats.errors.len());
    }
    Ok(())
}

fn print_dry_run(sources: &[SourceSpec], destination: &Path, opts: &MoveOptions, json: bool) -> Result<()> {
    let mut errors = 0usize;
    let mut entries = Vec::new();
    for spec in sources {
        match mvops::plan_one(spec, destination, opts) {
            Ok(planned) => {
                let (action, detail) = match &planned.action {
                    PlannedAction::Rename => ("renommer", String::from("instantané")),
                    PlannedAction::CopyThenDelete { bytes, files } => (
                        "copier+suppr",
                        format!("cross-device, {} fichier(s), {}", files, format_size(*bytes, DECIMAL)),
                    ),
                };
                if json {
                    entries.push(format!(
                        "{{\"action\":\"{}\",\"src\":\"{}\",\"dst\":\"{}\",\"detail\":\"{}\"}}",
                        action,
                        json_escape(&spec.path.display().to_string()),
                        json_escape(&planned.dst_root.display().to_string()),
                        json_escape(&detail)
                    ));
                } else {
                    println!("{action:<12} {} -> {} ({detail})", spec.path.display(), planned.dst_root.display());
                }
            }
            Err(e) => {
                errors += 1;
                if !json {
                    print_error(&e);
                } else {
                    entries.push(format!("{{\"error\":\"{}\"}}", json_escape(&e)));
                }
            }
        }
    }
    if json {
        println!("[{}]", entries.join(","));
    }
    if errors > 0 {
        bail!("{errors} erreur(s) pendant l'analyse");
    }
    Ok(())
}

/// Résumé final humain : renames instantanés et copies cross-device.
fn print_summary(stats: &MoveStats, elapsed: std::time::Duration) {
    let mut parts = Vec::new();
    if stats.renamed > 0 {
        parts.push(format!("{} renommé(s)", stats.renamed));
    }
    if stats.moved_across > 0 {
        parts.push(format!(
            "{} déplacé(s) cross-device ({})",
            stats.moved_across,
            format_size(stats.copy.bytes_copied, DECIMAL)
        ));
    }
    if parts.is_empty() && stats.skipped == 0 && stats.errors.is_empty() {
        parts.push(String::from("rien à déplacer"));
    }
    let mut line = format!("{} en {}", parts.join(", "), format_duration(elapsed));
    if stats.skipped > 0 {
        line.push_str(&format!(", {} ignoré(s)", stats.skipped));
    }
    if stats.copy.verified > 0 {
        line.push_str(&format!(", {} vérifié(s)", stats.copy.verified));
    }
    if !stats.errors.is_empty() {
        line.push_str(&format!(", {} erreur(s)", stats.errors.len()));
    }
    if stats.errors.is_empty() {
        print_success(&line);
    } else {
        print_warn(&line);
    }
}

fn json_summary(stats: &MoveStats, elapsed: std::time::Duration) -> String {
    format!(
        "{{\"renamed\":{},\"moved_across\":{},\"skipped\":{},\"files_copied\":{},\"bytes_copied\":{},\
\"verified\":{},\"duration_secs\":{:.3},\"errors\":{}}}",
        stats.renamed,
        stats.moved_across,
        stats.skipped,
        stats.copy.files_copied,
        stats.copy.bytes_copied,
        stats.copy.verified,
        elapsed.as_secs_f64(),
        json_string_array(&stats.errors)
    )
}
