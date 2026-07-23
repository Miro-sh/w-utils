//! Définition de la CLI wcp : drapeaux de cp(1) GNU + extensions wcp.

use std::path::PathBuf;

use anyhow::{bail, Result};
use clap::{CommandFactory, Parser};

use w_utils::copy::{Reflink, SourceSpec, Sparse};
pub use w_utils::options::{
    BackupControl, CopyMode, Deref, Overwrite, Preserve, ReflinkWhen, SparseWhen, UpdateWhen,
};

// ---------------------------------------------------------------------------
// Arguments en ligne de commande
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(
    name = "wcp",
    version,
    about = "wcp — cp(1) moderne avec barre de progression",
    long_about = "wcp copie fichiers et répertoires comme cp(1) GNU : mêmes drapeaux, \
mêmes sémantiques de destination, mêmes codes de sortie. En plus : barre de \
progression en direct (pourcentage, vitesse, ETA), copies atomiques (jamais de \
fichier partiel à la destination) et quelques gardes-fous (espace disque vérifié \
avant de commencer, copie d'un répertoire dans lui-même refusée)."
)]
pub struct Args {
    /// Sources puis destination (dernier argument) ; avec -t, tout est source
    #[arg(value_name = "SOURCE... DEST", num_args = 1..,
          required_unless_present_any = ["generate_man", "generate_completions"])]
    pub paths: Vec<PathBuf>,

    /// Copie récursive (requis pour les répertoires)
    #[arg(short = 'r', short_alias = 'R', long)]
    pub recursive: bool,

    /// Mode archive : -dR --preserve=all
    #[arg(short, long)]
    pub archive: bool,

    /// Équivalent de --no-dereference --preserve=links
    #[arg(short = 'd', overrides_with_all = ["dereference", "no_dereference", "dereference_command_line"])]
    pub no_dereference_links: bool,

    /// Ne jamais suivre les liens symboliques
    #[arg(short = 'P', long, overrides_with_all = ["dereference", "no_dereference_links", "dereference_command_line"])]
    pub no_dereference: bool,

    /// Suivre tous les liens symboliques
    #[arg(short = 'L', long, overrides_with_all = ["no_dereference", "no_dereference_links", "dereference_command_line"])]
    pub dereference: bool,

    /// Suivre seulement les liens symboliques de la ligne de commande (défaut)
    #[arg(short = 'H', overrides_with_all = ["dereference", "no_dereference", "no_dereference_links"])]
    pub dereference_command_line: bool,

    /// Demander avant d'écraser un fichier existant
    #[arg(short = 'i', long, overrides_with_all = ["no_clobber", "update"])]
    pub interactive: bool,

    /// Ne jamais écraser un fichier existant
    #[arg(short = 'n', long, overrides_with_all = ["interactive", "update"])]
    pub no_clobber: bool,

    /// N'écraser que si la source est plus récente (WHEN : all, none, none-fail, older)
    #[arg(short, long, value_name = "WHEN", num_args = 0..=1, require_equals = true,
          default_missing_value = "older",
          overrides_with_all = ["interactive", "no_clobber", "update"])]
    pub update: Option<UpdateWhen>,

    /// Supprimer la destination si elle ne peut pas être ouverte
    /// (accepté pour compatibilité : les copies atomiques rendent -f implicite)
    #[arg(short, long)]
    pub force: bool,

    /// Supprimer chaque destination existante avant de copier
    #[arg(long)]
    pub remove_destination: bool,

    /// Préserver les attributs : mode,ownership,timestamps,links,xattr,context,all
    #[arg(short = 'p', long, value_name = "LISTE", num_args = 0..=1, require_equals = true,
          default_missing_value = "mode,ownership,timestamps", overrides_with = "preserve")]
    pub preserve: Option<String>,

    /// Ne pas préserver les attributs listés
    #[arg(long, value_name = "LISTE", require_equals = true, overrides_with = "no_preserve")]
    pub no_preserve: Option<String>,

    /// Recréer le chemin complet de la source sous la destination
    #[arg(long)]
    pub parents: bool,

    /// Rester sur le même système de fichiers
    #[arg(short = 'x', long)]
    pub one_file_system: bool,

    /// Créer des liens durs au lieu de copier
    #[arg(short = 'l', long, conflicts_with = "symbolic_link")]
    pub link: bool,

    /// Créer des liens symboliques au lieu de copier
    #[arg(short = 's', long)]
    pub symbolic_link: bool,

    /// Sauvegarder chaque fichier écrasé (WHEN : none, numbered, existing, simple)
    #[arg(short = 'b', long, value_name = "WHEN", num_args = 0..=1, require_equals = true,
          default_missing_value = "existing", overrides_with = "backup")]
    pub backup: Option<BackupControl>,

    /// Suffixe des sauvegardes simples (défaut ~, env SIMPLE_BACKUP_SUFFIX)
    #[arg(short = 'S', long, value_name = "SUFFIXE")]
    pub suffix: Option<String>,

    /// Copier toutes les sources dans RÉP
    #[arg(short = 't', long, value_name = "RÉP", conflicts_with = "no_target_directory")]
    pub target_directory: Option<PathBuf>,

    /// Traiter la destination comme un fichier normal (jamais un répertoire)
    #[arg(short = 'T', long)]
    pub no_target_directory: bool,

    /// Retirer les « / » finaux des arguments sources
    #[arg(long)]
    pub strip_trailing_slashes: bool,

    /// Copie légère par blocs partagés, btrfs/xfs/APFS (WHEN : always, auto)
    #[arg(long, value_name = "WHEN", num_args = 0..=1, require_equals = true,
          default_missing_value = "auto", overrides_with = "reflink")]
    pub reflink: Option<ReflinkWhen>,

    /// Gestion des fichiers creux (WHEN : auto, always, never ; défaut auto)
    #[arg(long, value_name = "WHEN", default_value = "auto")]
    pub sparse: SparseWhen,

    /// Ne copier que les attributs, pas le contenu des fichiers
    #[arg(long)]
    pub attributes_only: bool,

    /// Copier le contenu des fichiers spéciaux en mode récursif
    #[arg(long)]
    pub copy_contents: bool,

    /// Afficher chaque fichier copié
    #[arg(short, long)]
    pub verbose: bool,

    // --- Extensions wcp ---

    /// Forcer la barre de progression
    #[arg(long, conflicts_with = "no_progress")]
    pub progress: bool,

    /// Désactiver la barre de progression (pour les scripts)
    #[arg(long)]
    pub no_progress: bool,

    /// Afficher le plan de copie sans rien écrire
    #[arg(long)]
    pub dry_run: bool,

    /// Ignorer les fichiers déjà entièrement copiés (reprise après interruption)
    #[arg(long)]
    pub resume: bool,

    /// Vérifier chaque fichier copié par somme de contrôle xxh3
    #[arg(long)]
    pub verify: bool,

    /// Copies en parallèle (0 = auto = nombre de cœurs, max 8)
    #[arg(short = 'j', long, value_name = "N", default_value = "0")]
    pub jobs: usize,

    /// Résumé final en JSON sur stdout (pour les scripts)
    #[arg(long)]
    pub json: bool,

    /// Exclure les fichiers correspondant à ce motif glob (répétable)
    #[arg(long, value_name = "MOTIF")]
    pub exclude: Vec<String>,

    /// Lire des motifs d'exclusion depuis un fichier (répétable)
    #[arg(long, value_name = "FICHIER")]
    pub exclude_from: Vec<PathBuf>,

    /// Limiter le débit de copie (ex. 10m, 512k, 1.5g)
    #[arg(long, value_name = "DÉBIT")]
    pub bwlimit: Option<String>,

    /// Génère les complétions shell sur stdout (bash, zsh, fish...)
    #[arg(long, hide = true, value_name = "SHELL")]
    pub generate_completions: Option<clap_complete::Shell>,

    /// Génère la page man sur stdout (usage interne au packaging)
    #[arg(long, hide = true)]
    pub generate_man: bool,
}

/// Configuration effective, après interprétation des drapeaux (sémantique cp).
#[derive(Debug)]
pub struct Config {
    pub sources: Vec<SourceSpec>,
    pub destination: PathBuf,
    pub dest_never_dir: bool,
    pub recursive: bool,
    pub deref: Deref,
    pub mode: CopyMode,
    pub overwrite: Overwrite,
    pub preserve: Preserve,
    /// L'utilisateur a demandé 'context' explicitement (avertir : non géré).
    pub explicit_context: bool,
    pub backup: Option<BackupControl>,
    pub backup_suffix: String,
    pub parents: bool,
    pub one_file_system: bool,
    pub reflink: Reflink,
    pub sparse: Sparse,
    pub remove_destination: bool,
    pub attributes_only: bool,
    pub copy_contents: bool,
    pub verbose: bool,
    pub resume: bool,
    pub dry_run: bool,
    /// Some(true) = --progress, Some(false) = --no-progress, None = auto.
    pub progress: Option<bool>,
    pub verify: bool,
    /// 0 = auto (nombre de cœurs, plafonné).
    pub jobs: usize,
    pub json: bool,
    pub exclude: Vec<String>,
    pub exclude_from: Vec<PathBuf>,
    pub bwlimit: Option<u64>,
}

impl Args {
    /// Interprète les drapeaux et produit la configuration effective.
    pub fn resolve(&self) -> Result<Config> {
        // 1. Sources et destination.
        let (raw_sources, destination) = if let Some(dir) = &self.target_directory {
            if !dir.is_dir() {
                bail!("le répertoire cible '{}' n'existe pas", dir.display());
            }
            (self.paths.clone(), dir.clone())
        } else {
            if self.paths.len() < 2 {
                bail!(
                    "opérande de destination manquante après '{}'",
                    self.paths.first().map(|p| p.display().to_string()).unwrap_or_default()
                );
            }
            let (dest, srcs) = self.paths.split_last().expect("au moins 2 opérandes");
            if srcs.len() > 1 {
                if self.no_target_directory {
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

        if self.parents && !destination.is_dir() {
            bail!("avec --parents, la destination doit être un répertoire existant");
        }

        // 2. Suivi des liens symboliques (-H est le comportement par défaut ;
        // -a implique -d, sauf si un drapeau explicite comme -L contredit).
        let deref = if self.dereference {
            Deref::Always
        } else if self.no_dereference || self.no_dereference_links || self.archive {
            Deref::Never
        } else {
            Deref::CommandLine
        };

        // 3. Mode de copie.
        let mode = if self.link {
            CopyMode::Link
        } else if self.symbolic_link {
            CopyMode::Symlink
        } else {
            CopyMode::Copy
        };

        // 4. Politique d'écrasement (clap a déjà appliqué « le dernier gagne »).
        let overwrite = if self.interactive {
            Overwrite::Interactive
        } else if self.no_clobber {
            Overwrite::NoClobber
        } else {
            match self.update {
                Some(UpdateWhen::All) => Overwrite::Clobber,
                Some(UpdateWhen::None) => Overwrite::NoClobber,
                Some(UpdateWhen::NoneFail) => Overwrite::NoClobberFail,
                Some(UpdateWhen::Older) => Overwrite::Update,
                None => Overwrite::Clobber,
            }
        };

        // 5. Attributs préservés : -a pose la base, --preserve/--no-preserve ajustent.
        let mut preserve = if self.archive { Preserve::ALL } else { Preserve::default() };
        let mut explicit_context = false;
        if let Some(list) = &self.preserve {
            for attr in list.split(',') {
                preserve.set(attr, true)?;
                if matches!(attr.trim(), "context" | "all") {
                    explicit_context = true;
                }
            }
        }
        if self.no_dereference_links {
            preserve.links = true;
        }
        if let Some(list) = &self.no_preserve {
            for attr in list.split(',') {
                preserve.set(attr, false)?;
            }
        }

        // 6. Sauvegardes.
        let backup = match self.backup {
            Some(BackupControl::None) => None,
            other => other,
        };
        let backup_suffix = self
            .suffix
            .clone()
            .or_else(|| std::env::var("SIMPLE_BACKUP_SUFFIX").ok())
            .unwrap_or_else(|| String::from("~"));

        // 7. Un « / » final sur une source force le suivi du lien (comme cp),
        // sauf si --strip-trailing-slashes le retire.
        let sources = raw_sources
            .into_iter()
            .map(|path| {
                let trailing = path
                    .as_os_str()
                    .to_string_lossy()
                    .ends_with(std::path::MAIN_SEPARATOR);
                SourceSpec { path, follow: trailing && !self.strip_trailing_slashes }
            })
            .collect();

        let reflink = match self.reflink {
            Some(ReflinkWhen::Always) => Reflink::Always,
            Some(ReflinkWhen::Auto) => Reflink::Auto,
            None => Reflink::Off,
        };
        let sparse = match self.sparse {
            SparseWhen::Auto => Sparse::Auto,
            SparseWhen::Always => Sparse::Always,
            SparseWhen::Never => Sparse::Never,
        };
        let progress = if self.no_progress {
            Some(false)
        } else if self.progress {
            Some(true)
        } else {
            None
        };
        let bwlimit = match &self.bwlimit {
            Some(s) => Some(
                w_utils::utils::parse_rate(s).map_err(|e| anyhow::anyhow!("--bwlimit : {e}"))?,
            ),
            None => None,
        };

        Ok(Config {
            sources,
            destination,
            dest_never_dir: self.no_target_directory,
            recursive: self.recursive || self.archive,
            deref,
            mode,
            overwrite,
            preserve,
            explicit_context,
            backup,
            backup_suffix,
            parents: self.parents,
            one_file_system: self.one_file_system,
            reflink,
            sparse,
            remove_destination: self.remove_destination,
            attributes_only: self.attributes_only,
            copy_contents: self.copy_contents,
            verbose: self.verbose,
            resume: self.resume,
            dry_run: self.dry_run,
            progress,
            verify: self.verify,
            jobs: self.jobs,
            json: self.json,
            exclude: self.exclude.clone(),
            exclude_from: self.exclude_from.clone(),
            bwlimit,
        })
    }
}

/// Construit le Command clap complet (utilisé par clap_mangen).
pub fn build_cli() -> clap::Command {
    Args::command()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(argv: &[&str]) -> Result<Config> {
        Args::try_parse_from(argv).map_err(|e| anyhow::anyhow!("{e}"))?.resolve()
    }

    #[test]
    fn simple_file_copy() {
        let cfg = resolve(&["wcp", "a", "b"]).unwrap();
        assert_eq!(cfg.sources.len(), 1);
        assert_eq!(cfg.destination, PathBuf::from("b"));
        assert_eq!(cfg.overwrite, Overwrite::Clobber);
        assert_eq!(cfg.deref, Deref::CommandLine);
        assert!(!cfg.recursive);
    }

    #[test]
    fn missing_destination_is_an_error() {
        assert!(resolve(&["wcp", "a"]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn multiple_sources_need_an_existing_dir() {
        let t = tempfile::TempDir::new().unwrap();
        let destdir = t.path().join("dest");
        std::fs::create_dir(&destdir).unwrap();
        let dest = destdir.to_str().unwrap().to_string();

        let cfg = resolve(&["wcp", "x", "y", &dest]).unwrap();
        assert_eq!(cfg.sources.len(), 2);

        // Répertoire inexistant ou fichier simple : erreur avec plusieurs sources.
        let missing = t.path().join("nope").to_str().unwrap().to_string();
        assert!(resolve(&["wcp", "x", "y", &missing]).is_err());
        let file = t.path().join("f");
        std::fs::write(&file, b"").unwrap();
        let file = file.to_str().unwrap().to_string();
        assert!(resolve(&["wcp", "x", "y", &file]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn target_directory_takes_all_as_sources() {
        let t = tempfile::TempDir::new().unwrap();
        let destdir = t.path().join("dest");
        std::fs::create_dir(&destdir).unwrap();
        let dest = destdir.to_str().unwrap().to_string();
        let cfg = resolve(&["wcp", "-t", &dest, "a", "b", "c"]).unwrap();
        assert_eq!(cfg.sources.len(), 3);
        assert_eq!(cfg.destination, destdir);
    }

    #[test]
    fn no_target_directory_rejects_extra_operands() {
        assert!(resolve(&["wcp", "-T", "a", "b", "c"]).is_err());
        let cfg = resolve(&["wcp", "-T", "a", "b"]).unwrap();
        assert!(cfg.dest_never_dir);
    }

    #[test]
    fn archive_implies_deref_recursive_preserve_all() {
        let cfg = resolve(&["wcp", "-a", "a", "b"]).unwrap();
        assert!(cfg.recursive);
        assert_eq!(cfg.deref, Deref::Never);
        assert_eq!(cfg.preserve, Preserve::ALL);
        assert!(!cfg.explicit_context);
    }

    #[test]
    fn preserve_and_no_preserve_combine() {
        let cfg = resolve(&["wcp", "--preserve=mode,links", "a", "b"]).unwrap();
        assert!(cfg.preserve.mode && cfg.preserve.links);
        assert!(!cfg.preserve.timestamps && !cfg.preserve.ownership);

        let cfg = resolve(&["wcp", "-a", "--no-preserve=ownership,xattr", "a", "b"]).unwrap();
        assert!(!cfg.preserve.ownership && !cfg.preserve.xattr);
        assert!(cfg.preserve.mode && cfg.preserve.timestamps && cfg.preserve.links);

        assert!(resolve(&["wcp", "--preserve=bogus", "a", "b"]).is_err());
    }

    #[test]
    fn short_p_preserves_mode_ownership_timestamps() {
        let cfg = resolve(&["wcp", "-p", "a", "b"]).unwrap();
        assert!(cfg.preserve.mode && cfg.preserve.ownership && cfg.preserve.timestamps);
        assert!(!cfg.preserve.links);
    }

    #[test]
    fn no_dereference_links_sets_never_and_links() {
        let cfg = resolve(&["wcp", "-d", "a", "b"]).unwrap();
        assert_eq!(cfg.deref, Deref::Never);
        assert!(cfg.preserve.links);
    }

    #[test]
    fn overwrite_last_flag_wins() {
        let cfg = resolve(&["wcp", "-n", "-i", "a", "b"]).unwrap();
        assert_eq!(cfg.overwrite, Overwrite::Interactive);
        let cfg = resolve(&["wcp", "-i", "-n", "a", "b"]).unwrap();
        assert_eq!(cfg.overwrite, Overwrite::NoClobber);
    }

    #[test]
    fn update_variants() {
        let cfg = resolve(&["wcp", "-u", "a", "b"]).unwrap();
        assert_eq!(cfg.overwrite, Overwrite::Update);
        let cfg = resolve(&["wcp", "--update=none", "a", "b"]).unwrap();
        assert_eq!(cfg.overwrite, Overwrite::NoClobber);
        let cfg = resolve(&["wcp", "--update=none-fail", "a", "b"]).unwrap();
        assert_eq!(cfg.overwrite, Overwrite::NoClobberFail);
        let cfg = resolve(&["wcp", "--update=all", "a", "b"]).unwrap();
        assert_eq!(cfg.overwrite, Overwrite::Clobber);
    }

    #[test]
    fn backup_default_and_none() {
        let cfg = resolve(&["wcp", "-b", "a", "b"]).unwrap();
        assert_eq!(cfg.backup, Some(BackupControl::Existing));
        assert_eq!(cfg.backup_suffix, "~");
        let cfg = resolve(&["wcp", "--backup=none", "a", "b"]).unwrap();
        assert_eq!(cfg.backup, None);
        let cfg = resolve(&["wcp", "--backup=numbered", "-S", ".bak", "a", "b"]).unwrap();
        assert_eq!(cfg.backup, Some(BackupControl::Numbered));
        assert_eq!(cfg.backup_suffix, ".bak");
    }

    #[test]
    fn trailing_slash_marks_follow() {
        let cfg = resolve(&["wcp", "dir/", "out"]).unwrap();
        assert!(cfg.sources[0].follow);
        let cfg = resolve(&["wcp", "--strip-trailing-slashes", "dir/", "out"]).unwrap();
        assert!(!cfg.sources[0].follow);
        let cfg = resolve(&["wcp", "dir", "out"]).unwrap();
        assert!(!cfg.sources[0].follow);
    }

    #[test]
    fn deref_last_flag_wins() {
        let cfg = resolve(&["wcp", "-P", "-L", "a", "b"]).unwrap();
        assert_eq!(cfg.deref, Deref::Always);
        let cfg = resolve(&["wcp", "-L", "-P", "a", "b"]).unwrap();
        assert_eq!(cfg.deref, Deref::Never);
    }

    #[test]
    fn recursive_accepts_capital_r() {
        let cfg = resolve(&["wcp", "-R", "a", "b"]).unwrap();
        assert!(cfg.recursive);
    }

    #[test]
    fn bwlimit_is_parsed_at_resolve() {
        let cfg = resolve(&["wcp", "--bwlimit", "10m", "a", "b"]).unwrap();
        assert_eq!(cfg.bwlimit, Some(10 * 1024 * 1024));
        assert!(resolve(&["wcp", "--bwlimit", "vite", "a", "b"]).is_err());
        let cfg = resolve(&["wcp", "a", "b"]).unwrap();
        assert_eq!(cfg.bwlimit, None);
    }

    #[test]
    fn extras_default_off() {
        let cfg = resolve(&["wcp", "a", "b"]).unwrap();
        assert!(!cfg.verify && !cfg.json);
        assert_eq!(cfg.jobs, 0);
        assert!(cfg.exclude.is_empty() && cfg.exclude_from.is_empty());

        let cfg = resolve(&["wcp", "--verify", "--json", "-j", "4", "--exclude", "*.log", "a", "b"]).unwrap();
        assert!(cfg.verify && cfg.json);
        assert_eq!(cfg.jobs, 4);
        assert_eq!(cfg.exclude, vec![String::from("*.log")]);
    }
}
