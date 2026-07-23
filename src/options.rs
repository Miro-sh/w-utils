//! Options partagées par les outils de la suite (wcp, wmv...).
//!
//! Ces énumérations et structures décrivent les sémantiques communes de
//! cp(1)/mv(1) GNU : politique d'écrasement, suivi des liens, attributs
//! préservés, sauvegardes. Les CLI de chaque binaire les résolvent depuis
//! leurs propres drapeaux.

use anyhow::{bail, Result};
use clap::ValueEnum;

/// Attributs préservés lors de la copie (-a, -p, --preserve, --no-preserve).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Preserve {
    pub mode: bool,
    pub ownership: bool,
    pub timestamps: bool,
    pub links: bool,
    pub xattr: bool,
    pub context: bool,
}

impl Preserve {
    pub const ALL: Self = Self {
        mode: true,
        ownership: true,
        timestamps: true,
        links: true,
        xattr: true,
        context: true,
    };

    pub fn any(&self) -> bool {
        self.mode || self.ownership || self.timestamps || self.links || self.xattr || self.context
    }

    pub fn set(&mut self, attr: &str, value: bool) -> Result<()> {
        match attr.trim() {
            "mode" => self.mode = value,
            "ownership" => self.ownership = value,
            "timestamps" => self.timestamps = value,
            "links" => self.links = value,
            "xattr" => self.xattr = value,
            "context" => self.context = value,
            "all" => *self = if value { Self::ALL } else { Self::default() },
            other => bail!("attribut --preserve inconnu : '{other}'"),
        }
        Ok(())
    }
}

/// Politique d'écrasement d'une destination existante (le dernier drapeau gagne).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overwrite {
    /// Écraser (défaut)
    Clobber,
    /// Ne jamais écraser, ignorer (-n, --update=none)
    NoClobber,
    /// Échouer si la destination existe (--update=none-fail)
    NoClobberFail,
    /// N'écraser que si la source est plus récente (-u, --update=older)
    Update,
    /// Demander avant chaque écrasement (-i)
    Interactive,
}

/// Suivi des liens symboliques.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deref {
    /// Jamais (-P, -d)
    Never,
    /// Toujours (-L)
    Always,
    /// Seulement ceux de la ligne de commande (défaut, -H)
    CommandLine,
}

/// Quoi créer à la destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CopyMode {
    /// Copier le contenu (défaut)
    Copy,
    /// Liens durs (-l)
    Link,
    /// Liens symboliques (-s)
    Symlink,
}

/// Méthode de sauvegarde des fichiers écrasés (-b/--backup).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum BackupControl {
    /// Jamais de sauvegarde (alias : off)
    #[value(alias = "off")]
    None,
    /// Sauvegardes numérotées : fichier.~1~ (alias : t)
    #[value(alias = "t")]
    Numbered,
    /// Numérotée s'il en existe déjà, sinon simple (alias : nil)
    #[value(alias = "nil")]
    Existing,
    /// Simple : fichier~ (alias : never)
    #[value(alias = "never")]
    Simple,
}

/// --update=WHEN
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum UpdateWhen {
    All,
    None,
    #[value(name = "none-fail")]
    NoneFail,
    Older,
}

/// --reflink[=WHEN]
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ReflinkWhen {
    Always,
    Auto,
}

/// --sparse=WHEN
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SparseWhen {
    Auto,
    Always,
    Never,
}
