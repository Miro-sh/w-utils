//! Définition de la CLI (séparée de main pour pouvoir générer la page man).

use std::path::PathBuf;

use clap::{CommandFactory, Parser};

#[derive(Parser, Debug)]
#[command(
    name = "wcp",
    version,
    about = "wcp — une version moderne de cp avec barre de progression",
    long_about = "wcp copie fichiers et répertoires comme cp(1), avec une barre de \
progression en direct (pourcentage, vitesse, ETA) et des copies atomiques : \
chaque fichier est écrit sous un nom temporaire puis renommé, donc une \
interruption ne laisse jamais de fichier partiel à la destination."
)]
pub struct Args {
    /// Fichier ou répertoire source
    #[arg(required_unless_present = "generate_man")]
    pub source: Option<PathBuf>,

    /// Fichier ou répertoire de destination
    #[arg(required_unless_present = "generate_man")]
    pub destination: Option<PathBuf>,

    /// Copie récursive (requis pour les répertoires, comme cp -r)
    #[arg(short, long)]
    pub recursive: bool,

    /// Force l'affichage de la barre de progression
    #[arg(long, conflicts_with = "no_progress")]
    pub progress: bool,

    /// Désactive la barre de progression (mode silencieux, pour les scripts)
    #[arg(long)]
    pub no_progress: bool,

    /// Mode verbeux : affiche chaque fichier copié (comme cp -v)
    #[arg(short, long)]
    pub verbose: bool,

    /// Mode archive : préserve permissions et horodatages
    #[arg(short, long)]
    pub archive: bool,

    /// Simule la copie : affiche ce qui serait fait, sans rien écrire
    #[arg(long)]
    pub dry_run: bool,

    /// Reprend une copie interrompue : ignore les fichiers déjà présents
    /// à la destination avec la bonne taille
    #[arg(long)]
    pub resume: bool,

    /// Génère la page man sur stdout (usage : wcp --generate-man | gzip > wcp.1.gz)
    #[arg(long, hide = true)]
    pub generate_man: bool,
}

/// Construit le Command clap complet (utilisé par clap_mangen).
pub fn build_cli() -> clap::Command {
    Args::command()
}
