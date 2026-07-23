//! Barre de progression globale, style `rsync --info=progress2`.
//!
//! La barre reste cachée pendant la première seconde de la copie : si le
//! transfert est plus rapide que ça, on évite un « flash » visuel inutile.
//! Quand la progression est désactivée (pipe, --no-progress), la barre reste
//! invisible et `inc()` ne coûte quasiment rien.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

/// Délai avant d'afficher la barre (évite le flash sur les copies courtes).
const SHOW_AFTER: Duration = Duration::from_secs(1);

pub struct CopyProgress {
    bar: ProgressBar,
    enabled: bool,
    visible: AtomicBool,
    start: Instant,
}

impl CopyProgress {
    pub fn new(total_bytes: u64, enabled: bool) -> Self {
        let bar = ProgressBar::new(total_bytes);
        bar.set_style(
            ProgressStyle::with_template(
                "[{wide_bar:.cyan/blue}] {percent:>3}%  {bytes}/{total_bytes}  {bytes_per_sec}  ETA {eta}  {msg}",
            )
            .expect("template indicatif invalide")
            .progress_chars("█░"),
        );
        // Toujours créée cachée : elle ne s'affiche qu'au bout d'1 s de copie.
        bar.set_draw_target(ProgressDrawTarget::hidden());
        Self {
            bar,
            enabled,
            visible: AtomicBool::new(false),
            start: Instant::now(),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Avance la barre et la rend visible si la copie dure plus d'1 s.
    pub fn inc(&self, n: u64) {
        self.bar.inc(n);
        if self.enabled && !self.visible.load(Ordering::Relaxed) && self.start.elapsed() >= SHOW_AFTER {
            self.bar.set_draw_target(ProgressDrawTarget::stderr_with_hz(10));
            self.visible.store(true, Ordering::Relaxed);
        }
    }

    /// Affiche le nom du fichier en cours à droite de la barre.
    pub fn set_current_file(&self, name: &str) {
        if self.enabled {
            self.bar.set_message(name.to_string());
        }
    }

    /// Affiche une ligne de log sans casser la barre (mode verbeux).
    pub fn log(&self, msg: &str) {
        if self.visible.load(Ordering::Relaxed) {
            self.bar.suspend(|| println!("{msg}"));
        } else {
            println!("{msg}");
        }
    }

    /// Termine la barre : laisse l'état final affiché si elle était visible,
    /// sinon ne produit aucune sortie.
    pub fn finish(&self) {
        if self.visible.load(Ordering::Relaxed) {
            self.bar.finish();
        } else {
            self.bar.finish_and_clear();
        }
    }
}
