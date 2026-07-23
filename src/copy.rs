//! Logique de copie : planification, exécution et sécurité.
//!
//! Chaque fichier est d'abord copié sous un nom temporaire dans le dossier
//! de destination, puis renommé atomiquement : en cas d'interruption
//! (Ctrl+C, erreur disque), la destination ne contient jamais de fichier
//! partiellement copié.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::process;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use filetime::{set_file_times, set_symlink_file_times, FileTime};
use walkdir::WalkDir;

use crate::cli::{BackupControl, CopyMode, Deref, Overwrite, Preserve};
use crate::progress::CopyProgress;

/// Buffer standard pour la copie avec progression.
const BUFFER_SMALL: usize = 256 * 1024;
/// Buffer élargi pour les très gros fichiers (moins d'appels système).
const BUFFER_LARGE: usize = 4 * 1024 * 1024;
/// Au-delà d'1 Gio on passe sur le gros buffer.
const LARGE_FILE_THRESHOLD: u64 = 1 << 30;

/// --reflink : copie légère par blocs partagés (btrfs, xfs, APFS).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reflink {
    Off,
    Auto,
    Always,
}

/// --sparse : gestion des fichiers creux.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sparse {
    Auto,
    Always,
    Never,
}

/// Une source de la ligne de commande, avec l'effet du « / » final éventuel.
#[derive(Debug, Clone)]
pub struct SourceSpec {
    pub path: PathBuf,
    /// « source/ » force le suivi d'un lien symbolique (comme cp).
    pub follow: bool,
}

pub struct CopyOptions {
    pub mode: CopyMode,
    pub overwrite: Overwrite,
    pub preserve: Preserve,
    pub backup: Option<BackupControl>,
    pub backup_suffix: String,
    pub reflink: Reflink,
    pub sparse: Sparse,
    pub remove_destination: bool,
    pub attributes_only: bool,
    pub verbose: bool,
    pub resume: bool,
    /// --verify : somme de contrôle xxh3 comparée après chaque fichier copié.
    pub verify: bool,
    /// -j N : copies en parallèle (1 = séquentiel).
    pub jobs: usize,
    /// --bwlimit : débit max en octets/s (partagé entre les threads).
    pub bwlimit: Option<u64>,
}

impl Default for CopyOptions {
    fn default() -> Self {
        Self {
            mode: CopyMode::Copy,
            overwrite: Overwrite::Clobber,
            preserve: Preserve::default(),
            backup: None,
            backup_suffix: String::from("~"),
            reflink: Reflink::Off,
            sparse: Sparse::Auto,
            remove_destination: false,
            attributes_only: false,
            verbose: false,
            resume: false,
            verify: false,
            jobs: 1,
            bwlimit: None,
        }
    }
}

/// Une entrée du plan de copie.
pub enum PlanEntry {
    File { src: PathBuf, dst: PathBuf, size: u64, sparse_hint: bool },
    Symlink { src: PathBuf, dst: PathBuf },
    Dir { src: PathBuf, dst: PathBuf },
    /// Fifo à recréer (cp -r sur un fifo).
    Fifo { src: PathBuf, dst: PathBuf },
    /// Fichier spécial lu comme un flux (cp /dev/null out, --copy-contents).
    Special { src: PathBuf, dst: PathBuf },
    /// Deuxième occurrence d'un inode déjà copié (--preserve=links) :
    /// on crée un lien dur vers la première destination.
    HardLink { link: PathBuf, dst: PathBuf },
}

/// Résultat de la phase d'analyse, avant toute écriture.
pub struct CopyPlan {
    pub entries: Vec<PlanEntry>,
    pub total_bytes: u64,
    pub file_count: usize,
    /// Fichiers spéciaux (sockets, devices, autres monts avec -x) ignorés.
    pub skipped: Vec<PathBuf>,
    /// Éléments écartés par --exclude.
    pub excluded: usize,
    /// Erreurs non fatales par source (comme cp, le reste est quand même traité).
    pub errors: Vec<String>,
}

#[derive(Default)]
pub struct CopyStats {
    pub files_copied: usize,
    pub dirs_created: usize,
    pub bytes_copied: u64,
    pub already_present: usize,
    /// Refusés par la politique d'écrasement (-n, -u, -i).
    pub skipped: usize,
    /// Fichiers dont la somme de contrôle a été vérifiée (--verify).
    pub verified: usize,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

impl CopyStats {
    /// Fusionne les stats d'un thread worker dans le total global.
    fn merge(&mut self, other: CopyStats) {
        self.files_copied += other.files_copied;
        self.dirs_created += other.dirs_created;
        self.bytes_copied += other.bytes_copied;
        self.already_present += other.already_present;
        self.skipped += other.skipped;
        self.verified += other.verified;
        self.warnings.extend(other.warnings);
        self.errors.extend(other.errors);
    }
}

// ---------------------------------------------------------------------------
// Planification
// ---------------------------------------------------------------------------

pub struct PlanConfig {
    pub recursive: bool,
    pub deref: Deref,
    pub parents: bool,
    pub one_file_system: bool,
    pub copy_contents: bool,
    pub preserve_links: bool,
    pub dest_never_dir: bool,
    pub remove_destination: bool,
    /// Motifs glob d'exclusion (--exclude / --exclude-from), testés sur le
    /// chemin relatif à la racine copiée.
    pub exclude: Option<globset::GlobSet>,
}

/// Analyse les sources et construit la liste complète des opérations.
/// Comme cp, une source en erreur n'empêche pas les autres d'être traitées.
pub fn build_plan(sources: &[SourceSpec], destination: &Path, cfg: &PlanConfig) -> Result<CopyPlan> {
    let mut plan = CopyPlan {
        entries: Vec::new(),
        total_bytes: 0,
        file_count: 0,
        skipped: Vec::new(),
        excluded: 0,
        errors: Vec::new(),
    };
    // Inodes déjà planifiés, pour --preserve=links : (dev, ino) -> 1re destination.
    let mut seen: HashMap<(u64, u64), PathBuf> = HashMap::new();
    for spec in sources {
        if let Err(e) = plan_one_source(spec, destination, cfg, &mut plan, &mut seen) {
            plan.errors.push(format!("{e:#}"));
        }
    }
    Ok(plan)
}

fn plan_one_source(
    spec: &SourceSpec,
    destination: &Path,
    cfg: &PlanConfig,
    plan: &mut CopyPlan,
    seen: &mut HashMap<(u64, u64), PathBuf>,
) -> Result<()> {
    // cp suit les liens de la ligne de commande sauf avec -P/-d ;
    // un « / » final force le suivi même avec -P.
    let follow = spec.follow || cfg.deref != Deref::Never;
    let meta = stat(&spec.path, follow)
        .with_context(|| format!("impossible d'accéder à '{}'", spec.path.display()))?;
    let dst_root = dest_root_for(&spec.path, destination, cfg);

    if meta.is_dir() {
        if !cfg.recursive {
            bail!("-r non spécifié ; omission du répertoire '{}'", spec.path.display());
        }
        ensure_not_nested(&spec.path, &dst_root)?;
        walk_source(&spec.path, &dst_root, &meta, cfg, plan, seen)
    } else if meta.file_type().is_symlink() && !follow {
        plan.entries.push(PlanEntry::Symlink { src: spec.path.clone(), dst: dst_root });
        Ok(())
    } else if meta.is_file() {
        add_file_entry(spec.path.clone(), dst_root, &meta, cfg, plan, seen)
    } else if cfg.recursive && is_fifo_meta(&meta) && !cfg.copy_contents {
        // cp -r recrée les fifos à l'identique.
        plan.entries.push(PlanEntry::Fifo { src: spec.path.clone(), dst: dst_root });
        Ok(())
    } else if !cfg.recursive || cfg.copy_contents {
        // Comme cp : un fichier spécial passé directement (ou via
        // --copy-contents) est lu comme un flux (/dev/null, fifo, device...).
        plan.entries.push(PlanEntry::Special { src: spec.path.clone(), dst: dst_root });
        Ok(())
    } else {
        bail!("'{}' est un fichier spécial, copie non supportée", spec.path.display())
    }
}

/// Enregistre un fichier ordinaire : écriture à travers les liens, détection
/// « même fichier » par inode, déduplication des liens durs (--preserve=links).
fn add_file_entry(
    src: PathBuf,
    dst: PathBuf,
    meta: &fs::Metadata,
    cfg: &PlanConfig,
    plan: &mut CopyPlan,
    seen: &mut HashMap<(u64, u64), PathBuf>,
) -> Result<()> {
    let dst = resolve_write_through(&dst, cfg.remove_destination)?;
    let (dev, ino, nlink) = file_ids(meta);

    // cp refuse de copier un fichier sur lui-même, y compris via un lien dur.
    if let Ok(dm) = fs::metadata(&dst) {
        if dm.is_file() {
            let (ddev, dino, _) = file_ids(&dm);
            if (ddev, dino) == (dev, ino) {
                bail!("'{}' et '{}' sont le même fichier", src.display(), dst.display());
            }
        }
    }

    if cfg.preserve_links && nlink > 1 {
        if let Some(first) = seen.get(&(dev, ino)) {
            plan.entries.push(PlanEntry::HardLink { link: first.clone(), dst });
            return Ok(());
        }
        seen.insert((dev, ino), dst.clone());
    }

    plan.total_bytes += meta.len();
    plan.file_count += 1;
    plan.entries.push(PlanEntry::File { src, dst, size: meta.len(), sparse_hint: is_sparse(meta) });
    Ok(())
}

/// cp écrit À TRAVERS un lien symbolique de destination (le contenu de la
/// cible est remplacé, le lien conservé), sauf avec --remove-destination qui
/// le supprime d'abord. Un lien rompu est refusé ("not writing through
/// dangling symlink").
fn resolve_write_through(dst: &Path, remove_destination: bool) -> Result<PathBuf> {
    if remove_destination {
        return Ok(dst.to_path_buf());
    }
    match dst.symlink_metadata() {
        Ok(m) if m.file_type().is_symlink() => match fs::canonicalize(dst) {
            Ok(target) => Ok(target),
            Err(_) => bail!("pas d'écriture à travers le lien symbolique rompu '{}'", dst.display()),
        },
        _ => Ok(dst.to_path_buf()),
    }
}

fn walk_source(
    src_root: &Path,
    dst_root: &Path,
    root_meta: &fs::Metadata,
    cfg: &PlanConfig,
    plan: &mut CopyPlan,
    seen: &mut HashMap<(u64, u64), PathBuf>,
) -> Result<()> {
    // La racine elle-même, pour préserver les répertoires vides.
    plan.entries.push(PlanEntry::Dir { src: src_root.to_path_buf(), dst: dst_root.to_path_buf() });
    let (root_dev, _, _) = file_ids(root_meta);
    let follow_all = cfg.deref == Deref::Always;

    let mut it = WalkDir::new(src_root).follow_links(follow_all).min_depth(1).into_iter();
    loop {
        let entry = match it.next() {
            None => break,
            Some(Ok(e)) => e,
            Some(Err(e)) => {
                plan.errors.push(format!("erreur lors du parcours : {e}"));
                continue;
            }
        };
        let rel = entry.path().strip_prefix(src_root).unwrap_or(entry.path());
        // --exclude : un répertoire exclu n'est pas descendu du tout.
        if is_excluded(rel, cfg) {
            plan.excluded += 1;
            if entry.file_type().is_dir() {
                it.skip_current_dir();
            }
            continue;
        }
        let dst = dst_root.join(rel);
        let ft = entry.file_type();

        if ft.is_dir() {
            // -x : ne pas descendre sur un autre système de fichiers.
            if cfg.one_file_system {
                if let Ok(m) = entry.metadata() {
                    if file_ids(&m).0 != root_dev {
                        plan.skipped.push(entry.path().to_path_buf());
                        it.skip_current_dir();
                        continue;
                    }
                }
            }
            plan.entries.push(PlanEntry::Dir { src: entry.path().to_path_buf(), dst });
        } else if ft.is_symlink() {
            plan.entries.push(PlanEntry::Symlink { src: entry.path().to_path_buf(), dst });
        } else if ft.is_file() {
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(e) => {
                    plan.errors.push(format!("{} : {}", entry.path().display(), e));
                    continue;
                }
            };
            if let Err(e) = add_file_entry(entry.path().to_path_buf(), dst, &meta, cfg, plan, seen) {
                plan.errors.push(format!("{e:#}"));
            }
        } else if is_fifo_ft(&ft) && !cfg.copy_contents {
            plan.entries.push(PlanEntry::Fifo { src: entry.path().to_path_buf(), dst });
        } else if cfg.copy_contents {
            plan.entries.push(PlanEntry::Special { src: entry.path().to_path_buf(), dst });
        } else {
            plan.skipped.push(entry.path().to_path_buf());
        }
    }
    Ok(())
}

fn stat(p: &Path, follow: bool) -> io::Result<fs::Metadata> {
    if follow { fs::metadata(p) } else { fs::symlink_metadata(p) }
}

/// --exclude : le motif est testé sur le chemin relatif à la racine copiée.
fn is_excluded(rel: &Path, cfg: &PlanConfig) -> bool {
    match &cfg.exclude {
        Some(set) => set.is_match(rel),
        None => false,
    }
}

/// (dev, ino, nlink) : identifie un fichier de façon unique sur son système
/// de fichiers (détection « même fichier » et des liens durs).
#[cfg(unix)]
fn file_ids(m: &fs::Metadata) -> (u64, u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (m.dev(), m.ino(), m.nlink())
}

#[cfg(not(unix))]
fn file_ids(_m: &fs::Metadata) -> (u64, u64, u64) {
    (0, 0, 1)
}

/// Heuristique de --sparse=auto : le fichier occupe moins de blocs que sa taille.
#[cfg(unix)]
fn is_sparse(m: &fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    m.len() > 0 && m.blocks().saturating_mul(512) < m.len()
}

#[cfg(not(unix))]
fn is_sparse(_m: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn is_fifo_meta(m: &fs::Metadata) -> bool {
    use std::os::unix::fs::FileTypeExt;
    m.file_type().is_fifo()
}

#[cfg(not(unix))]
fn is_fifo_meta(_m: &fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
fn is_fifo_ft(ft: &fs::FileType) -> bool {
    use std::os::unix::fs::FileTypeExt;
    ft.is_fifo()
}

#[cfg(not(unix))]
fn is_fifo_ft(_ft: &fs::FileType) -> bool {
    false
}

/// Sémantique de cp : si la destination est un répertoire existant, on copie
/// la source *dedans* en gardant son nom ; sinon la destination est le nom final.
/// --parents recrée le chemin complet de la source sous la destination.
/// Bonus UX : un « / » final signale un répertoire à créer (comme rsync).
fn dest_root_for(source: &Path, destination: &Path, cfg: &PlanConfig) -> PathBuf {
    if cfg.parents {
        return destination.join(strip_root(source));
    }
    if cfg.dest_never_dir {
        return destination.to_path_buf();
    }
    if destination.is_dir() || has_trailing_slash(destination) {
        match source.file_name() {
            Some(name) => destination.join(name),
            None => destination.to_path_buf(),
        }
    } else {
        destination.to_path_buf()
    }
}

/// --parents : chemin de la source sans la racine (« /a/b » -> « a/b »).
fn strip_root(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::RootDir | Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Path normalise et supprime les « / » finaux : il faut regarder la chaîne brute.
fn has_trailing_slash(p: &Path) -> bool {
    p.as_os_str().to_string_lossy().ends_with(std::path::MAIN_SEPARATOR)
}

/// Refuse `wcp -r a a/b` (récursion infinie) et `wcp f f` (auto-écrasement).
fn ensure_not_nested(source: &Path, dst_root: &Path) -> Result<()> {
    let src_abs = normalize_lexical(&absolutize(source));
    let dst_abs = normalize_lexical(&absolutize(dst_root));
    if dst_abs == src_abs {
        bail!("la source et la destination sont identiques : '{}'", source.display());
    }
    if dst_abs.starts_with(&src_abs) {
        bail!(
            "impossible de copier '{}' dans un de ses sous-répertoires ('{}')",
            source.display(),
            dst_root.display()
        );
    }
    Ok(())
}

fn absolutize(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")).join(p)
    }
}

/// Résout lexicalement les `.` et `..` sans toucher au système de fichiers
/// (la destination n'existe pas forcément encore, canonicalize échouerait).
fn normalize_lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Vérification d'espace disque
// ---------------------------------------------------------------------------

/// Vrai si la destination existe déjà (y compris lien cassé) : la copie
/// écraserait ce fichier. Utilisé par --dry-run.
pub fn would_overwrite(dst: &Path) -> bool {
    dst.symlink_metadata().is_ok()
}

/// --resume : un fichier déjà présent avec la bonne taille est considéré
/// comme copié. Le renommage atomique garantit qu'il n'est pas partiel :
/// une copie interrompue ne laisse rien sous le nom final.
pub fn is_up_to_date(dst: &Path, src_size: u64) -> bool {
    fs::metadata(dst).map(|m| m.is_file() && m.len() == src_size).unwrap_or(false)
}

/// Échoue tôt si la destination n'a clairement pas assez de place.
pub fn check_disk_space(total_bytes: u64, destination: &Path) -> Result<()> {
    let probe = existing_ancestor(destination);
    if let Ok(available) = fs2::available_space(&probe) {
        if total_bytes > available {
            bail!(
                "espace disque insuffisant : {} requis mais seulement {} disponibles sur '{}'",
                humansize::format_size(total_bytes, humansize::DECIMAL),
                humansize::format_size(available, humansize::DECIMAL),
                probe.display()
            );
        }
    }
    // Si la requête statfs échoue (FS exotique), on continue sans bloquer.
    Ok(())
}

fn existing_ancestor(p: &Path) -> PathBuf {
    let mut cur = p;
    loop {
        if cur.exists() {
            return cur.to_path_buf();
        }
        match cur.parent() {
            Some(parent) => cur = parent,
            None => return PathBuf::from("/"),
        }
    }
}

// ---------------------------------------------------------------------------
// Exécution
// ---------------------------------------------------------------------------

/// Exécute le plan. Les erreurs sont collectées par fichier (comme cp, on
/// continue le reste de la copie) et remontées dans les stats.
///
/// Quatre phases : répertoires, puis fichiers/liens (parallèle avec -j N),
/// puis les liens durs dédupliqués (qui dépendent de la première copie),
/// puis les métadonnées des répertoires (en dernier, comme cp).
pub fn execute_plan(plan: &CopyPlan, opts: &CopyOptions, progress: &CopyProgress) -> Result<CopyStats> {
    let mut stats = CopyStats::default();

    for skipped in &plan.skipped {
        stats.warnings.push(format!("ignoré (fichier spécial) : {}", skipped.display()));
    }

    // Phase 1 : les répertoires (création quasi gratuite, séquentielle).
    let mut dirs: Vec<&PlanEntry> = Vec::new();
    for entry in &plan.entries {
        if let PlanEntry::Dir { dst, .. } = entry {
            dirs.push(entry);
            if dst.symlink_metadata().is_ok() && !dst.is_dir() {
                stats.errors.push(format!(
                    "impossible d'écraser '{}' par un répertoire",
                    dst.display()
                ));
                continue;
            }
            let existed = dst.exists();
            match fs::create_dir_all(dst) {
                Ok(()) => stats.dirs_created += usize::from(!existed),
                Err(e) => stats.errors.push(format!("{} : {}", dst.display(), e)),
            }
        }
    }

    // Phase 2 : fichiers, liens symboliques, fifos et fichiers spéciaux.
    let work: Vec<&PlanEntry> = plan
        .entries
        .iter()
        .filter(|e| !matches!(e, PlanEntry::Dir { .. } | PlanEntry::HardLink { .. }))
        .collect();
    let throttle = opts.bwlimit.map(Throttle::new);
    let workers = opts.jobs.clamp(1, work.len().max(1));
    let phase2 = if workers <= 1 {
        run_sequential(&work, opts, progress, throttle.as_ref())
    } else {
        run_parallel(&work, opts, progress, throttle.as_ref(), workers)
    };
    stats.merge(phase2);

    // Phase 3 : les liens durs dédupliqués (--preserve=links) pointent vers
    // la première copie du fichier : forcément après la phase 2.
    for entry in &plan.entries {
        if let PlanEntry::HardLink { .. } = entry {
            process_hardlink(entry, opts, progress, &mut stats);
        }
    }

    // Phase 4 : métadonnées des répertoires APPLIQUÉES EN DERNIER, sinon
    // l'écriture des fichiers écraserait les horodatages des dossiers.
    if opts.preserve.any() {
        for entry in dirs.into_iter().rev() {
            if let PlanEntry::Dir { src, dst } = entry {
                if let Err(e) = apply_metadata(src, dst, &opts.preserve) {
                    stats.errors.push(format!(
                        "impossible de préserver les attributs de '{}' : {}",
                        dst.display(),
                        e
                    ));
                }
            }
        }
    }

    Ok(stats)
}

fn run_sequential(
    work: &[&PlanEntry],
    opts: &CopyOptions,
    progress: &CopyProgress,
    throttle: Option<&Throttle>,
) -> CopyStats {
    let mut stats = CopyStats::default();
    for entry in work {
        process_work_entry(entry, opts, progress, throttle, &mut stats);
    }
    stats
}

/// -j N : les entrées sont distribuées aux threads via un compteur partagé.
/// Chaque thread accumule ses stats localement, fusionnées à la fin.
fn run_parallel(
    work: &[&PlanEntry],
    opts: &CopyOptions,
    progress: &CopyProgress,
    throttle: Option<&Throttle>,
    workers: usize,
) -> CopyStats {
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                s.spawn(|| {
                    let mut local = CopyStats::default();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= work.len() {
                            break;
                        }
                        process_work_entry(work[i], opts, progress, throttle, &mut local);
                    }
                    local
                })
            })
            .collect();
        let mut total = CopyStats::default();
        for h in handles {
            match h.join() {
                Ok(local) => total.merge(local),
                Err(_) => total.errors.push(String::from("un thread de copie a paniqué")),
            }
        }
        total
    })
}

/// Traite une entrée « données » du plan (fichier, lien symbolique, fifo,
/// fichier spécial). Appelable depuis n'importe quel thread worker.
fn process_work_entry(
    entry: &PlanEntry,
    opts: &CopyOptions,
    progress: &CopyProgress,
    throttle: Option<&Throttle>,
    stats: &mut CopyStats,
) {
    match entry {
        PlanEntry::Symlink { src, dst } => match decide_overwrite(opts.overwrite, src, dst, false) {
            Decision::Skip => stats.skipped += 1,
            Decision::Fail(msg) => stats.errors.push(msg),
            Decision::Go => {
                if opts.resume
                    && dst.symlink_metadata().map(|m| m.file_type().is_symlink()).unwrap_or(false)
                {
                    stats.already_present += 1;
                    return;
                }
                let result = backup_existing(dst, opts)
                    .and_then(|()| ensure_parent(dst))
                    .and_then(|()| copy_symlink(src, dst))
                    .and_then(|()| preserve_symlink_metadata(src, dst, &opts.preserve));
                match result {
                    Ok(()) => {
                        stats.files_copied += 1;
                        if opts.verbose {
                            progress.log(&format!("{} -> {} (lien symbolique)", src.display(), dst.display()));
                        }
                    }
                    Err(e) => stats.errors.push(format!("{} -> {} : {}", src.display(), dst.display(), e)),
                }
            }
        },
        PlanEntry::File { src, dst, size, sparse_hint } => {
            if dst.symlink_metadata().map(|m| m.is_dir()).unwrap_or(false) {
                stats.errors.push(format!(
                    "impossible d'écraser le répertoire '{}' par un fichier",
                    dst.display()
                ));
                return;
            }
            match decide_overwrite(opts.overwrite, src, dst, true) {
                Decision::Skip => stats.skipped += 1,
                Decision::Fail(msg) => stats.errors.push(msg),
                Decision::Go => {
                    if opts.resume && is_up_to_date(dst, *size) {
                        stats.already_present += 1;
                        return;
                    }
                    if let Some(name) = src.file_name() {
                        progress.set_current_file(&name.to_string_lossy());
                    }
                    let result = backup_existing(dst, opts)
                        .and_then(|()| remove_destination_if_asked(dst, opts))
                        .and_then(|()| ensure_parent(dst))
                        .and_then(|()| copy_regular(src, dst, *size, *sparse_hint, opts, progress, throttle));
                    match result {
                        Ok((n, verified)) => {
                            stats.files_copied += 1;
                            stats.bytes_copied += n;
                            stats.verified += usize::from(verified);
                            if opts.verbose {
                                let check = if verified { ", vérifié" } else { "" };
                                progress.log(&format!(
                                    "{} -> {} ({}{check})",
                                    src.display(),
                                    dst.display(),
                                    humansize::format_size(*size, humansize::DECIMAL)
                                ));
                            }
                        }
                        Err(e) => stats.errors.push(format!("{} -> {} : {}", src.display(), dst.display(), e)),
                    }
                }
            }
        }
        PlanEntry::Fifo { src, dst } => match decide_overwrite(opts.overwrite, src, dst, false) {
            Decision::Skip => stats.skipped += 1,
            Decision::Fail(msg) => stats.errors.push(msg),
            Decision::Go => {
                let result = backup_existing(dst, opts)
                    .and_then(|()| ensure_parent(dst))
                    .and_then(|()| remove_existing(dst))
                    .and_then(|()| create_fifo(dst))
                    .and_then(|()| apply_metadata(src, dst, &opts.preserve));
                match result {
                    Ok(()) => {
                        stats.files_copied += 1;
                        if opts.verbose {
                            progress.log(&format!("{} -> {} (fifo)", src.display(), dst.display()));
                        }
                    }
                    Err(e) => stats.errors.push(format!("{} -> {} : {}", src.display(), dst.display(), e)),
                }
            }
        },
        PlanEntry::Special { src, dst } => {
            if dst.symlink_metadata().map(|m| m.is_dir()).unwrap_or(false) {
                stats.errors.push(format!(
                    "impossible d'écraser le répertoire '{}' par un fichier",
                    dst.display()
                ));
                return;
            }
            match decide_overwrite(opts.overwrite, src, dst, true) {
                Decision::Skip => stats.skipped += 1,
                Decision::Fail(msg) => stats.errors.push(msg),
                Decision::Go => {
                    let result = backup_existing(dst, opts)
                        .and_then(|()| ensure_parent(dst))
                        .and_then(|()| copy_stream(src, dst, progress, throttle))
                        .and_then(|n| {
                            apply_metadata(src, dst, &opts.preserve)?;
                            Ok(n)
                        });
                    match result {
                        Ok(n) => {
                            stats.files_copied += 1;
                            stats.bytes_copied += n;
                            if opts.verbose {
                                progress.log(&format!("{} -> {} (contenu spécial)", src.display(), dst.display()));
                            }
                        }
                        Err(e) => stats.errors.push(format!("{} -> {} : {}", src.display(), dst.display(), e)),
                    }
                }
            }
        }
        PlanEntry::Dir { .. } | PlanEntry::HardLink { .. } => unreachable!("traité dans sa propre phase"),
    }
}

/// Phase 3 : un lien dur vers la première copie d'un inode déjà transféré.
fn process_hardlink(entry: &PlanEntry, opts: &CopyOptions, progress: &CopyProgress, stats: &mut CopyStats) {
    let PlanEntry::HardLink { link, dst } = entry else { return };
    if dst.symlink_metadata().map(|m| m.is_dir()).unwrap_or(false) {
        stats.errors.push(format!(
            "impossible d'écraser le répertoire '{}' par un lien dur",
            dst.display()
        ));
        return;
    }
    match decide_overwrite(opts.overwrite, link, dst, true) {
        Decision::Skip => stats.skipped += 1,
        Decision::Fail(msg) => stats.errors.push(msg),
        Decision::Go => {
            let result = backup_existing(dst, opts)
                .and_then(|()| ensure_parent(dst))
                .and_then(|()| remove_existing(dst))
                .and_then(|()| fs::hard_link(link, dst));
            match result {
                Ok(()) => {
                    stats.files_copied += 1;
                    if opts.verbose {
                        progress.log(&format!("{} == {} (lien dur)", link.display(), dst.display()));
                    }
                }
                Err(e) => stats.errors.push(format!("{} -> {} : {}", link.display(), dst.display(), e)),
            }
        }
    }
}

fn ensure_parent(dst: &Path) -> io::Result<()> {
    match dst.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => fs::create_dir_all(parent),
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Politique d'écrasement (-i, -n, -u, --update)
// ---------------------------------------------------------------------------

enum Decision {
    Go,
    Skip,
    Fail(String),
}

fn decide_overwrite(policy: Overwrite, src: &Path, dst: &Path, follow: bool) -> Decision {
    if dst.symlink_metadata().is_err() {
        return Decision::Go;
    }
    match policy {
        Overwrite::Clobber => Decision::Go,
        Overwrite::NoClobber => Decision::Skip,
        Overwrite::NoClobberFail => Decision::Fail(format!("non remplacé : '{}'", dst.display())),
        Overwrite::Update => {
            let newer_or_equal = match (stat(src, follow), stat(dst, follow)) {
                (Ok(s), Ok(d)) => match (s.modified(), d.modified()) {
                    (Ok(sm), Ok(dm)) => dm >= sm,
                    _ => false,
                },
                _ => false,
            };
            if newer_or_equal { Decision::Skip } else { Decision::Go }
        }
        Overwrite::Interactive => {
            if confirm_overwrite(dst) { Decision::Go } else { Decision::Skip }
        }
    }
}

/// -i : question sur stderr, réponse lue sur stdin (comme cp).
fn confirm_overwrite(dst: &Path) -> bool {
    eprint!("wcp : écraser '{}' ? ", dst.display());
    let _ = io::stderr().flush();
    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    let answer = line.trim().to_lowercase();
    answer.starts_with('y') || answer.starts_with('o')
}

// ---------------------------------------------------------------------------
// Sauvegardes (-b/--backup, -S/--suffix)
// ---------------------------------------------------------------------------

/// -b : renomme la destination existante avant écrasement.
fn backup_existing(dst: &Path, opts: &CopyOptions) -> io::Result<()> {
    let Some(control) = opts.backup else { return Ok(()) };
    if dst.symlink_metadata().is_err() {
        return Ok(());
    }
    let backup = match control {
        BackupControl::None => return Ok(()),
        BackupControl::Simple => simple_backup_name(dst, &opts.backup_suffix),
        BackupControl::Numbered => numbered_backup_name(dst),
        BackupControl::Existing => {
            if has_numbered_backup(dst) {
                numbered_backup_name(dst)
            } else {
                simple_backup_name(dst, &opts.backup_suffix)
            }
        }
    };
    fs::rename(dst, &backup)
}

fn simple_backup_name(dst: &Path, suffix: &str) -> PathBuf {
    let mut name = dst.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    dst.with_file_name(name)
}

/// Sauvegardes numérotées : « nom.~N~ » avec N = max existant + 1.
fn numbered_backup_name(dst: &Path) -> PathBuf {
    let n = max_numbered_backup(dst).unwrap_or(0) + 1;
    let mut name = dst.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".~{n}~"));
    dst.with_file_name(name)
}

fn has_numbered_backup(dst: &Path) -> bool {
    max_numbered_backup(dst).is_some()
}

fn max_numbered_backup(dst: &Path) -> Option<u32> {
    let base = dst.file_name()?.to_string_lossy().into_owned();
    let parent = dst.parent()?;
    let mut max: Option<u32> = None;
    for entry in fs::read_dir(parent).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if let Some(n) = parse_numbered(&base, &name) {
            max = Some(max.map_or(n, |m: u32| m.max(n)));
        }
    }
    max
}

/// « nom.~12~ » -> 12 si le préfixe correspond.
fn parse_numbered(base: &str, candidate: &str) -> Option<u32> {
    let rest = candidate.strip_prefix(base)?;
    let num = rest.strip_prefix(".~")?.strip_suffix('~')?;
    num.parse().ok()
}

// ---------------------------------------------------------------------------
// Copie d'un fichier : temporaire + renommage atomique
// ---------------------------------------------------------------------------

/// --remove-destination : supprime la destination avant de copier. -f est de
/// toute façon implicite : le renommage atomique remplace les fichiers même
/// en lecture seule.
fn remove_destination_if_asked(dst: &Path, opts: &CopyOptions) -> io::Result<()> {
    if opts.remove_destination {
        remove_existing(dst)?;
    }
    Ok(())
}

fn remove_existing(dst: &Path) -> io::Result<()> {
    match dst.symlink_metadata() {
        Ok(m) if m.is_dir() && !m.file_type().is_symlink() => fs::remove_dir_all(dst),
        Ok(_) => fs::remove_file(dst),
        Err(_) => Ok(()),
    }
}

/// Copie d'un fichier ordinaire selon le mode demandé.
/// Retourne (octets copiés, vérifié par somme de contrôle).
fn copy_regular(
    src: &Path,
    dst: &Path,
    size: u64,
    sparse_hint: bool,
    opts: &CopyOptions,
    progress: &CopyProgress,
    throttle: Option<&Throttle>,
) -> io::Result<(u64, bool)> {
    match opts.mode {
        CopyMode::Link => {
            remove_existing(dst)?;
            fs::hard_link(src, dst)?;
            Ok((0, false))
        }
        CopyMode::Symlink => {
            // cp : une source relative n'a de sens que si la destination est
            // dans le répertoire courant.
            if src.is_relative() && !in_current_dir(dst) {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "les liens symboliques relatifs ne sont possibles que dans le répertoire courant",
                ));
            }
            remove_existing(dst)?;
            make_symlink(src, dst)?;
            Ok((0, false))
        }
        CopyMode::Copy if opts.attributes_only => {
            if dst.symlink_metadata().is_err() {
                fs::File::create(dst)?;
            }
            apply_metadata(src, dst, &opts.preserve)?;
            Ok((0, false))
        }
        CopyMode::Copy => {
            let sparse = match opts.sparse {
                Sparse::Always => true,
                Sparse::Never => false,
                Sparse::Auto => sparse_hint,
            };
            let progress_opt = if progress.is_enabled() { Some(progress) } else { None };
            copy_file_atomic(src, dst, size, sparse, opts.reflink, &opts.preserve, opts.verify, progress_opt, throttle)
        }
    }
}

fn in_current_dir(p: &Path) -> bool {
    match p.parent() {
        None => true,
        Some(parent) => parent.as_os_str().is_empty() || parent == Path::new("."),
    }
}

/// Copie `src` vers `dst` sans jamais laisser de fichier partiel visible.
/// Avec --verify, la somme xxh3 du temporaire est comparée à la source AVANT
/// le renommage : seules des données vérifiées atterrissent sous le nom final.
#[allow(clippy::too_many_arguments)]
pub fn copy_file_atomic(
    src: &Path,
    dst: &Path,
    size_hint: u64,
    sparse: bool,
    reflink: Reflink,
    preserve: &Preserve,
    verify: bool,
    progress: Option<&CopyProgress>,
    throttle: Option<&Throttle>,
) -> io::Result<(u64, bool)> {
    let tmp = temp_path_for(dst);
    set_current_tmp(&tmp);

    let result = copy_data(src, &tmp, size_hint, sparse, reflink, progress, throttle).and_then(|outcome| {
        // std::fs::copy préserve déjà les permissions ; les autres chemins non.
        if !outcome.perms_copied {
            apply_permissions(src, &tmp)?;
        }
        apply_metadata(src, &tmp, preserve)
            .map_err(|e| io::Error::new(e.kind(), format!("impossible de préserver les attributs : {e}")))?;
        if verify {
            verify_identical(src, &tmp)?;
        }
        rename_over(&tmp, dst)?;
        Ok((outcome.bytes, verify))
    });

    clear_current_tmp(&tmp);
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

struct CopyOutcome {
    bytes: u64,
    perms_copied: bool,
}

#[allow(clippy::too_many_arguments)]
fn copy_data(
    src: &Path,
    tmp: &Path,
    size_hint: u64,
    sparse: bool,
    reflink: Reflink,
    progress: Option<&CopyProgress>,
    throttle: Option<&Throttle>,
) -> io::Result<CopyOutcome> {
    // --reflink : blocs partagés CoW, quasi instantané sur btrfs/xfs/APFS.
    // Incompatible avec --bwlimit (aucun octet ne transite par nous).
    if reflink != Reflink::Off && throttle.is_none() {
        match try_reflink(src, tmp) {
            Ok(()) => return Ok(CopyOutcome { bytes: size_hint, perms_copied: false }),
            Err(e) if reflink == Reflink::Always => return Err(e),
            // auto : échec silencieux, on retombe sur la copie classique.
            Err(_) => {}
        }
    }
    if sparse {
        return copy_sparse_buffered(src, tmp, progress, throttle)
            .map(|bytes| CopyOutcome { bytes, perms_copied: false });
    }
    if progress.is_some() || throttle.is_some() {
        // Barre affichée ou débit limité : copie manuelle par buffer.
        return copy_buffered(src, tmp, size_hint, progress, throttle)
            .map(|bytes| CopyOutcome { bytes, perms_copied: false });
    }
    // Silencieux : std::fs::copy (copy_file_range sous Linux, quasi zero-copy).
    fs::copy(src, tmp).map(|bytes| CopyOutcome { bytes, perms_copied: true })
}

fn copy_buffered(
    src: &Path,
    dst: &Path,
    size_hint: u64,
    progress: Option<&CopyProgress>,
    throttle: Option<&Throttle>,
) -> io::Result<u64> {
    let mut reader = fs::File::open(src)?;
    let mut writer = fs::File::create(dst)?;
    let buf_size = if size_hint >= LARGE_FILE_THRESHOLD { BUFFER_LARGE } else { BUFFER_SMALL };
    let mut buf = vec![0u8; buf_size];
    let mut total = 0u64;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        writer.write_all(&buf[..n])?;
        total += n as u64;
        if let Some(p) = progress {
            p.inc(n as u64);
        }
        if let Some(t) = throttle {
            t.add(n as u64);
        }
    }
    writer.flush()?;
    Ok(total)
}

/// Copie en sautant les blocs entièrement nuls (--sparse) : la destination
/// reste « creuse ». Le set_len final garantit la taille logique exacte.
fn copy_sparse_buffered(
    src: &Path,
    dst: &Path,
    progress: Option<&CopyProgress>,
    throttle: Option<&Throttle>,
) -> io::Result<u64> {
    let mut reader = fs::File::open(src)?;
    let mut writer = fs::File::create(dst)?;
    let mut buf = vec![0u8; BUFFER_SMALL];
    let mut total = 0u64;
    loop {
        let n = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if buf[..n].iter().all(|&b| b == 0) {
            writer.seek(SeekFrom::Current(n as i64))?;
        } else {
            writer.write_all(&buf[..n])?;
        }
        total += n as u64;
        if let Some(p) = progress {
            p.inc(n as u64);
        }
        if let Some(t) = throttle {
            t.add(n as u64);
        }
    }
    writer.set_len(total)?;
    writer.flush()?;
    Ok(total)
}

/// Copie d'un flux de taille inconnue (fifo, /dev/*, --copy-contents),
/// elle aussi atomique via un temporaire + renommage.
fn copy_stream(
    src: &Path,
    dst: &Path,
    progress: &CopyProgress,
    throttle: Option<&Throttle>,
) -> io::Result<u64> {
    let tmp = temp_path_for(dst);
    set_current_tmp(&tmp);
    let progress_opt = if progress.is_enabled() { Some(progress) } else { None };

    let result = (|| -> io::Result<u64> {
        let mut reader = fs::File::open(src)?;
        let mut writer = fs::File::create(&tmp)?;
        let mut buf = vec![0u8; BUFFER_SMALL];
        let mut total = 0u64;
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            };
            writer.write_all(&buf[..n])?;
            total += n as u64;
            if let Some(p) = progress_opt {
                p.inc(n as u64);
            }
            if let Some(t) = throttle {
                t.add(n as u64);
            }
        }
        writer.flush()?;
        rename_over(&tmp, dst)?;
        Ok(total)
    })();

    clear_current_tmp(&tmp);
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

// ---------------------------------------------------------------------------
// --verify et --bwlimit
// ---------------------------------------------------------------------------

/// --verify : relit les deux fichiers et compare leurs sommes xxh3.
fn verify_identical(src: &Path, dst: &Path) -> io::Result<()> {
    let (h_src, h_dst) = (hash_file(src)?, hash_file(dst)?);
    if h_src != h_dst {
        return Err(io::Error::new(
            ErrorKind::InvalidData,
            "vérification échouée : la copie diffère de la source",
        ));
    }
    Ok(())
}

fn hash_file(p: &Path) -> io::Result<u64> {
    let mut f = fs::File::open(p)?;
    let mut h = xxhash_rust::xxh3::Xxh3::new();
    let mut buf = vec![0u8; BUFFER_LARGE];
    loop {
        match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => h.update(&buf[..n]),
            Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(h.digest())
}

/// Limitation de débit globale (--bwlimit), partagée entre les threads :
/// après chaque buffer écrit, on dort si on devance le débit demandé.
pub struct Throttle {
    rate: u64,
    start: Instant,
    bytes: AtomicU64,
}

impl Throttle {
    pub fn new(rate: u64) -> Self {
        Self { rate: rate.max(1), start: Instant::now(), bytes: AtomicU64::new(0) }
    }

    fn add(&self, n: u64) {
        let total = self.bytes.fetch_add(n, Ordering::Relaxed) + n;
        let expected = Duration::from_secs_f64(total as f64 / self.rate as f64);
        let elapsed = self.start.elapsed();
        if expected > elapsed + Duration::from_millis(1) {
            std::thread::sleep(expected - elapsed);
        }
    }
}

/// FICLONE sous Linux, clonefile(2) sous macOS ; erreur ailleurs.
#[cfg(target_os = "linux")]
fn try_reflink(src: &Path, dst: &Path) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;

    let s = fs::File::open(src)?;
    let d = fs::File::create(dst)?;
    let ret = unsafe { libc::ioctl(d.as_raw_fd(), libc::FICLONE as _, s.as_raw_fd()) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn try_reflink(src: &Path, dst: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let c_src = std::ffi::CString::new(src.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "chemin invalide"))?;
    let c_dst = std::ffi::CString::new(dst.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "chemin invalide"))?;
    let ret = unsafe { libc::clonefile(c_src.as_ptr(), c_dst.as_ptr(), 0) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn try_reflink(_src: &Path, _dst: &Path) -> io::Result<()> {
    Err(io::Error::new(ErrorKind::Unsupported, "reflink non supporté"))
}

// ---------------------------------------------------------------------------
// Préservation des attributs (mode, ownership, timestamps, xattr)
// ---------------------------------------------------------------------------

/// Applique les attributs demandés, dans l'ordre de cp : propriétaire AVANT
/// les permissions (chown peut réinitialiser les bits setuid).
fn apply_metadata(src: &Path, dst: &Path, preserve: &Preserve) -> io::Result<()> {
    if !preserve.any() {
        return Ok(());
    }
    let meta = fs::symlink_metadata(src)?;
    if preserve.ownership {
        apply_ownership(&meta, dst)?;
    }
    if preserve.mode {
        fs::set_permissions(dst, meta.permissions())?;
    }
    if preserve.timestamps {
        set_file_times(
            dst,
            FileTime::from_last_access_time(&meta),
            FileTime::from_last_modification_time(&meta),
        )?;
    }
    if preserve.xattr {
        copy_xattrs(src, dst)?;
    }
    Ok(())
}

/// Propriétaire/groupe et horodatages d'un lien symbolique (lchown,
/// utimensat AT_SYMLINK_NOFOLLOW), sans toucher à sa cible.
fn preserve_symlink_metadata(src: &Path, dst: &Path, preserve: &Preserve) -> io::Result<()> {
    if !preserve.ownership && !preserve.timestamps {
        return Ok(());
    }
    let meta = fs::symlink_metadata(src)?;
    if preserve.ownership {
        apply_ownership(&meta, dst)?;
    }
    if preserve.timestamps {
        // Souvent non supporté par le FS : jamais bloquant.
        let _ = set_symlink_file_times(
            dst,
            FileTime::from_last_access_time(&meta),
            FileTime::from_last_modification_time(&meta),
        );
    }
    Ok(())
}

/// chown vers le propriétaire/groupe de la source. Sans effet si déjà
/// identique (évite les EPERM inutiles pour un utilisateur non-root).
#[cfg(unix)]
fn apply_ownership(meta: &fs::Metadata, dst: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;

    let (uid, gid) = (meta.uid(), meta.gid());
    if let Ok(dm) = fs::symlink_metadata(dst) {
        if dm.uid() == uid && dm.gid() == gid {
            return Ok(());
        }
    }
    let c_path = std::ffi::CString::new(dst.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "chemin invalide"))?;
    let ret = unsafe { libc::lchown(c_path.as_ptr(), uid, gid) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(unix))]
fn apply_ownership(_meta: &fs::Metadata, _dst: &Path) -> io::Result<()> {
    Ok(())
}

fn apply_permissions(src: &Path, dst: &Path) -> io::Result<()> {
    fs::set_permissions(dst, fs::metadata(src)?.permissions())
}

/// Copie les attributs étendus (ACL incluses). Linux seulement ; les échecs
/// individuels sont ignorés (xattrs non supportées sur certains FS).
#[cfg(target_os = "linux")]
fn copy_xattrs(src: &Path, dst: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let c_src = std::ffi::CString::new(src.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "chemin invalide"))?;
    let c_dst = std::ffi::CString::new(dst.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "chemin invalide"))?;

    unsafe {
        let size = libc::llistxattr(c_src.as_ptr(), std::ptr::null_mut(), 0);
        if size <= 0 {
            return Ok(());
        }
        let mut list = vec![0u8; size as usize];
        let size = libc::llistxattr(c_src.as_ptr(), list.as_mut_ptr().cast(), list.len());
        if size <= 0 {
            return Ok(());
        }
        for name in list[..size as usize].split(|&b| b == 0) {
            if name.is_empty() {
                continue;
            }
            let vlen = libc::lgetxattr(c_src.as_ptr(), name.as_ptr().cast(), std::ptr::null_mut(), 0);
            if vlen < 0 {
                continue;
            }
            let mut value = vec![0u8; vlen as usize];
            let vlen = libc::lgetxattr(c_src.as_ptr(), name.as_ptr().cast(), value.as_mut_ptr().cast(), value.len());
            if vlen < 0 {
                continue;
            }
            let _ = libc::lsetxattr(
                c_dst.as_ptr(),
                name.as_ptr().cast(),
                value.as_ptr().cast(),
                vlen as usize,
                0,
            );
        }
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn copy_xattrs(_src: &Path, _dst: &Path) -> io::Result<()> {
    Ok(())
}

/// fs::rename écrase la cible sous Unix, pas sous Windows.
fn rename_over(tmp: &Path, dst: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        fs::rename(tmp, dst)
    }
    #[cfg(not(unix))]
    {
        if dst.exists() {
            fs::remove_file(dst)?;
        }
        fs::rename(tmp, dst)
    }
}

/// Nom du fichier temporaire : caché, dans le même dossier que la cible
/// (donc sur le même système de fichiers → le renommage est atomique).
fn temp_path_for(dst: &Path) -> PathBuf {
    let mut name = OsString::from(".wcp-tmp-");
    name.push(process::id().to_string());
    name.push("-");
    if let Some(n) = dst.file_name() {
        name.push(n);
    }
    dst.with_file_name(name)
}

// ---------------------------------------------------------------------------
// Liens symboliques et fifos
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn make_symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(unix))]
fn make_symlink(target: &Path, _link: &Path) -> io::Result<()> {
    Err(io::Error::new(
        ErrorKind::Unsupported,
        format!("lien symbolique non supporté sur cette plateforme : {}", target.display()),
    ))
}

#[cfg(unix)]
fn copy_symlink(src: &Path, dst: &Path) -> io::Result<()> {
    let target = fs::read_link(src)?;
    if dst.symlink_metadata().is_ok() {
        // cp refuse d'écraser un vrai répertoire par un lien symbolique.
        if dst.is_dir() && !dst.symlink_metadata().map(|m| m.file_type().is_symlink()).unwrap_or(false) {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                format!("impossible d'écraser le répertoire '{}' par un lien symbolique", dst.display()),
            ));
        }
        fs::remove_file(dst)?;
    }
    make_symlink(&target, dst)
}

#[cfg(not(unix))]
fn copy_symlink(src: &Path, _dst: &Path) -> io::Result<()> {
    Err(io::Error::new(
        ErrorKind::Unsupported,
        format!("lien symbolique non supporté sur cette plateforme : {}", src.display()),
    ))
}

#[cfg(unix)]
fn create_fifo(dst: &Path) -> io::Result<()> {
    use std::os::unix::ffi::OsStrExt;

    let c_path = std::ffi::CString::new(dst.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(ErrorKind::InvalidInput, "chemin invalide"))?;
    let ret = unsafe { libc::mkfifo(c_path.as_ptr(), 0o666) };
    if ret != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(unix))]
fn create_fifo(dst: &Path) -> io::Result<()> {
    Err(io::Error::new(
        ErrorKind::Unsupported,
        format!("fifo non supporté sur cette plateforme : {}", dst.display()),
    ))
}

// ---------------------------------------------------------------------------
// Gestion propre de Ctrl+C
// ---------------------------------------------------------------------------

/// Fichiers temporaires en cours d'écriture (plusieurs en parallèle avec -j N).
static CURRENT_TMPS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();

fn tmp_slot() -> &'static Mutex<HashSet<PathBuf>> {
    CURRENT_TMPS.get_or_init(|| Mutex::new(HashSet::new()))
}

fn set_current_tmp(p: &Path) {
    if let Ok(mut g) = tmp_slot().lock() {
        g.insert(p.to_path_buf());
    }
}

fn clear_current_tmp(p: &Path) {
    if let Ok(mut g) = tmp_slot().lock() {
        g.remove(p);
    }
}

/// Installe un handler Ctrl+C qui supprime les fichiers temporaires en cours
/// avant de quitter (code 130, comme la convention shell).
pub fn install_ctrlc_handler() {
    let _ = ctrlc::set_handler(|| {
        if let Some(slot) = CURRENT_TMPS.get() {
            // try_lock : si un thread tient le verrou, tant pis, mieux vaut
            // quitter vite que risquer un deadlock dans un handler.
            if let Ok(guard) = slot.try_lock() {
                for p in guard.iter() {
                    let _ = fs::remove_file(p);
                }
            }
        }
        std::process::exit(130);
    });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(p: &Path, content: &[u8]) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    fn no_progress() -> CopyProgress {
        CopyProgress::new(0, false)
    }

    fn quiet_opts() -> CopyOptions {
        CopyOptions::default()
    }

    fn spec(p: &Path) -> SourceSpec {
        SourceSpec { path: p.to_path_buf(), follow: false }
    }

    fn plan_cfg(recursive: bool) -> PlanConfig {
        PlanConfig {
            recursive,
            deref: Deref::CommandLine,
            parents: false,
            one_file_system: false,
            copy_contents: false,
            preserve_links: false,
            dest_never_dir: false,
            remove_destination: false,
            exclude: None,
        }
    }

    fn quick_plan(src: &Path, dst: &Path, recursive: bool) -> CopyPlan {
        build_plan(&[spec(src)], dst, &plan_cfg(recursive)).unwrap()
    }

    fn make_tree(t: &TempDir) -> PathBuf {
        let src = t.path().join("d");
        write(&src.join("a.txt"), b"aaa");
        write(&src.join("sub/b.txt"), b"bbbbb");
        fs::create_dir_all(src.join("empty")).unwrap();
        src
    }

    // --- Planification ---

    #[test]
    fn plan_single_file() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("a.txt");
        write(&src, b"hello");
        let plan = quick_plan(&src, &t.path().join("out"), false);
        assert_eq!(plan.file_count, 1);
        assert_eq!(plan.total_bytes, 5);
        assert_eq!(plan.entries.len(), 1);
        assert!(plan.errors.is_empty());
    }

    #[test]
    fn plan_requires_recursive_for_dirs() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("d");
        fs::create_dir(&src).unwrap();
        let plan = quick_plan(&src, &t.path().join("out"), false);
        assert!(plan.entries.is_empty());
        assert!(!plan.errors.is_empty());
    }

    #[test]
    fn plan_missing_source_reported_but_not_fatal() {
        let t = TempDir::new().unwrap();
        let ok = t.path().join("ok.txt");
        write(&ok, b"x");
        let plan = build_plan(
            &[spec(&t.path().join("nope")), spec(&ok)],
            &t.path().join("out"),
            &plan_cfg(false),
        )
        .unwrap();
        assert_eq!(plan.errors.len(), 1);
        assert_eq!(plan.file_count, 1);
    }

    #[test]
    fn plan_recursive_walks_tree() {
        let t = TempDir::new().unwrap();
        let src = make_tree(&t);
        let plan = quick_plan(&src, &t.path().join("out"), true);
        assert_eq!(plan.file_count, 2);
        assert_eq!(plan.total_bytes, 8);
        // racine + 2 sous-répertoires + 2 fichiers
        assert_eq!(plan.entries.len(), 5);
    }

    #[test]
    fn copies_single_file_content() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("a.txt");
        let dst = t.path().join("b.txt");
        write(&src, b"contenu test");
        let plan = quick_plan(&src, &dst, false);
        let stats = execute_plan(&plan, &quiet_opts(), &no_progress()).unwrap();
        assert!(stats.errors.is_empty());
        assert_eq!(fs::read(&dst).unwrap(), b"contenu test");
        assert_eq!(stats.bytes_copied, 12);
    }

    #[test]
    fn copies_tree_recursively() {
        let t = TempDir::new().unwrap();
        let src = make_tree(&t);
        let out = t.path().join("out");
        let plan = quick_plan(&src, &out, true);
        let stats = execute_plan(&plan, &quiet_opts(), &no_progress()).unwrap();
        assert!(stats.errors.is_empty());
        assert_eq!(fs::read(out.join("a.txt")).unwrap(), b"aaa");
        assert_eq!(fs::read(out.join("sub/b.txt")).unwrap(), b"bbbbb");
        assert!(out.join("empty").is_dir());
    }

    #[test]
    fn multiple_sources_land_in_dest_dir() {
        let t = TempDir::new().unwrap();
        let a = t.path().join("a.txt");
        let b = t.path().join("b.txt");
        write(&a, b"A");
        write(&b, b"B");
        let destdir = t.path().join("dest");
        fs::create_dir(&destdir).unwrap();
        let plan = build_plan(&[spec(&a), spec(&b)], &destdir, &plan_cfg(false)).unwrap();
        let stats = execute_plan(&plan, &quiet_opts(), &no_progress()).unwrap();
        assert!(stats.errors.is_empty());
        assert_eq!(fs::read(destdir.join("a.txt")).unwrap(), b"A");
        assert_eq!(fs::read(destdir.join("b.txt")).unwrap(), b"B");
    }

    #[test]
    fn dest_existing_dir_keeps_source_name() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("a.txt");
        write(&src, b"x");
        let destdir = t.path().join("dest");
        fs::create_dir(&destdir).unwrap();
        let plan = quick_plan(&src, &destdir, false);
        execute_plan(&plan, &quiet_opts(), &no_progress()).unwrap();
        assert_eq!(fs::read(destdir.join("a.txt")).unwrap(), b"x");
    }

    #[test]
    fn dest_dir_into_existing_dir_keeps_name() {
        let t = TempDir::new().unwrap();
        let src = make_tree(&t);
        let destdir = t.path().join("backup");
        fs::create_dir(&destdir).unwrap();
        let plan = quick_plan(&src, &destdir, true);
        execute_plan(&plan, &quiet_opts(), &no_progress()).unwrap();
        assert!(destdir.join("d/a.txt").exists());
        assert!(destdir.join("d/sub/b.txt").exists());
    }

    #[test]
    fn refuses_copy_into_itself() {
        let t = TempDir::new().unwrap();
        let src = make_tree(&t);
        let plan = quick_plan(&src, &src.join("sub"), true);
        assert!(!plan.errors.is_empty());
    }

    #[test]
    fn refuses_identical_source_and_dest() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("a.txt");
        write(&src, b"x");
        let plan = quick_plan(&src, &src, false);
        assert!(!plan.errors.is_empty());
    }

    #[test]
    fn no_temp_files_left_behind() {
        let t = TempDir::new().unwrap();
        let src = make_tree(&t);
        let out = t.path().join("out");
        let plan = quick_plan(&src, &out, true);
        execute_plan(&plan, &quiet_opts(), &no_progress()).unwrap();
        for entry in WalkDir::new(t.path()) {
            let entry = entry.unwrap();
            assert!(!entry.file_name().to_string_lossy().starts_with(".wcp-tmp"));
        }
    }

    #[test]
    fn copy_with_progress_path_matches_fast_path() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("big.bin");
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8).collect();
        write(&src, &data);
        let dst = t.path().join("big-copy.bin");
        let plan = quick_plan(&src, &dst, false);
        // Barre "activée" : force le chemin de copie par buffer.
        let stats = execute_plan(&plan, &quiet_opts(), &CopyProgress::new(plan.total_bytes, true)).unwrap();
        assert!(stats.errors.is_empty());
        assert_eq!(fs::read(&dst).unwrap(), data);
    }

    #[test]
    fn dest_trailing_slash_creates_dir() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("a.txt");
        write(&src, b"x");
        // "newdir/" n'existe pas encore : le "/" final signale un répertoire.
        let dst = t.path().join("newdir").join("");
        let plan = quick_plan(&src, &dst, false);
        execute_plan(&plan, &quiet_opts(), &no_progress()).unwrap();
        assert_eq!(fs::read(t.path().join("newdir/a.txt")).unwrap(), b"x");
    }

    #[test]
    fn would_overwrite_detects_existing() {
        let t = TempDir::new().unwrap();
        let dst = t.path().join("x");
        assert!(!would_overwrite(&dst));
        write(&dst, b"1");
        assert!(would_overwrite(&dst));
    }

    #[test]
    fn resume_skips_already_copied_files() {
        let t = TempDir::new().unwrap();
        let src = make_tree(&t);
        let backup = t.path().join("backup");
        fs::create_dir(&backup).unwrap();
        let opts = CopyOptions { resume: true, ..Default::default() };

        let plan = quick_plan(&src, &backup, true);
        let s1 = execute_plan(&plan, &opts, &no_progress()).unwrap();
        assert_eq!(s1.already_present, 0);

        // Relancer la même commande avec --resume : rien à refaire.
        let plan2 = quick_plan(&src, &backup, true);
        let s2 = execute_plan(&plan2, &opts, &no_progress()).unwrap();
        assert_eq!(s2.files_copied, 0);
        assert_eq!(s2.already_present, 2);
        assert_eq!(s2.bytes_copied, 0);
    }

    #[test]
    fn resume_recopies_on_size_mismatch() {
        let t = TempDir::new().unwrap();
        let src = make_tree(&t);
        let backup = t.path().join("backup");
        fs::create_dir(&backup).unwrap();
        let opts = CopyOptions { resume: true, ..Default::default() };

        let plan = quick_plan(&src, &backup, true);
        execute_plan(&plan, &opts, &no_progress()).unwrap();

        // Fichier destination "corrompu" (taille différente) : --resume le refait.
        fs::write(backup.join("d/a.txt"), b"XXXXXX").unwrap();
        let plan2 = quick_plan(&src, &backup, true);
        let s2 = execute_plan(&plan2, &opts, &no_progress()).unwrap();
        assert_eq!(s2.files_copied, 1);
        assert_eq!(s2.already_present, 1);
        assert_eq!(fs::read(backup.join("d/a.txt")).unwrap(), b"aaa");
    }

    #[test]
    fn normalize_handles_dotdot() {
        assert_eq!(normalize_lexical(Path::new("/a/b/../c")), PathBuf::from("/a/c"));
        assert_eq!(normalize_lexical(Path::new("/a/./b")), PathBuf::from("/a/b"));
    }

    // --- Politique d'écrasement ---

    #[test]
    fn no_clobber_never_overwrites() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("a.txt");
        let dst = t.path().join("b.txt");
        write(&src, b"nouveau");
        write(&dst, b"ancien");
        let opts = CopyOptions { overwrite: Overwrite::NoClobber, ..Default::default() };
        let plan = quick_plan(&src, &dst, false);
        let stats = execute_plan(&plan, &opts, &no_progress()).unwrap();
        assert_eq!(stats.skipped, 1);
        assert_eq!(stats.files_copied, 0);
        assert_eq!(fs::read(&dst).unwrap(), b"ancien");
    }

    #[test]
    fn update_skips_when_dest_is_newer() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("a.txt");
        let dst = t.path().join("b.txt");
        write(&src, b"nouveau");
        write(&dst, b"ancien");
        // Source plus vieille que la destination -> ignoré.
        let old = FileTime::from_unix_time(1_000_000, 0);
        set_file_times(&src, old, old).unwrap();
        let opts = CopyOptions { overwrite: Overwrite::Update, ..Default::default() };
        let plan = quick_plan(&src, &dst, false);
        let stats = execute_plan(&plan, &opts, &no_progress()).unwrap();
        assert_eq!(stats.skipped, 1);
        assert_eq!(fs::read(&dst).unwrap(), b"ancien");
    }

    #[test]
    fn update_overwrites_when_source_is_newer() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("a.txt");
        let dst = t.path().join("b.txt");
        write(&src, b"nouveau");
        write(&dst, b"ancien");
        let old = FileTime::from_unix_time(1_000_000, 0);
        set_file_times(&dst, old, old).unwrap();
        let opts = CopyOptions { overwrite: Overwrite::Update, ..Default::default() };
        let plan = quick_plan(&src, &dst, false);
        let stats = execute_plan(&plan, &opts, &no_progress()).unwrap();
        assert_eq!(stats.files_copied, 1);
        assert_eq!(fs::read(&dst).unwrap(), b"nouveau");
    }

    #[test]
    fn none_fail_policy_reports_an_error() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("a.txt");
        let dst = t.path().join("b.txt");
        write(&src, b"x");
        write(&dst, b"y");
        let opts = CopyOptions { overwrite: Overwrite::NoClobberFail, ..Default::default() };
        let plan = quick_plan(&src, &dst, false);
        let stats = execute_plan(&plan, &opts, &no_progress()).unwrap();
        assert_eq!(stats.errors.len(), 1);
        assert_eq!(fs::read(&dst).unwrap(), b"y");
    }

    // --- Sauvegardes ---

    #[test]
    fn backup_simple_renames_with_suffix() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("a.txt");
        let dst = t.path().join("b.txt");
        write(&src, b"nouveau");
        write(&dst, b"ancien");
        let opts = CopyOptions { backup: Some(BackupControl::Simple), ..Default::default() };
        let plan = quick_plan(&src, &dst, false);
        let stats = execute_plan(&plan, &opts, &no_progress()).unwrap();
        assert!(stats.errors.is_empty());
        assert_eq!(fs::read(&dst).unwrap(), b"nouveau");
        assert_eq!(fs::read(t.path().join("b.txt~")).unwrap(), b"ancien");
    }

    #[test]
    fn backup_numbered_increments() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("a.txt");
        let dst = t.path().join("b.txt");
        write(&src, b"n1");
        write(&dst, b"v0");
        let opts = CopyOptions { backup: Some(BackupControl::Numbered), ..Default::default() };

        let plan = quick_plan(&src, &dst, false);
        execute_plan(&plan, &opts, &no_progress()).unwrap();
        assert_eq!(fs::read(t.path().join("b.txt.~1~")).unwrap(), b"v0");

        write(&src, b"n2");
        let plan2 = quick_plan(&src, &dst, false);
        execute_plan(&plan2, &opts, &no_progress()).unwrap();
        assert_eq!(fs::read(t.path().join("b.txt.~2~")).unwrap(), b"n1");
        assert_eq!(fs::read(&dst).unwrap(), b"n2");
    }

    #[test]
    fn parse_numbered_names() {
        assert_eq!(parse_numbered("f", "f.~3~"), Some(3));
        assert_eq!(parse_numbered("f", "f~"), None);
        assert_eq!(parse_numbered("f", "f.~x~"), None);
        assert_eq!(parse_numbered("f", "g.~1~"), None);
    }

    // --- Liens symboliques en source et en destination ---

    #[cfg(unix)]
    #[test]
    fn symlinks_are_recreated() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("d");
        write(&src.join("real.txt"), b"data");
        std::os::unix::fs::symlink("real.txt", src.join("link.txt")).unwrap();
        let out = t.path().join("out");
        let plan = quick_plan(&src, &out, true);
        let stats = execute_plan(&plan, &quiet_opts(), &no_progress()).unwrap();
        assert!(stats.errors.is_empty());
        let link = out.join("link.txt");
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(fs::read_link(&link).unwrap(), PathBuf::from("real.txt"));
    }

    #[cfg(unix)]
    #[test]
    fn command_line_symlink_is_followed_by_default() {
        let t = TempDir::new().unwrap();
        let real = t.path().join("real.txt");
        write(&real, b"data");
        let link = t.path().join("link.txt");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let dst = t.path().join("out.txt");
        let plan = quick_plan(&link, &dst, false);
        execute_plan(&plan, &quiet_opts(), &no_progress()).unwrap();
        assert!(!dst.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(fs::read(&dst).unwrap(), b"data");
    }

    #[cfg(unix)]
    #[test]
    fn no_dereference_keeps_command_line_symlink() {
        let t = TempDir::new().unwrap();
        let real = t.path().join("real.txt");
        write(&real, b"data");
        let link = t.path().join("link.txt");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let dst = t.path().join("out.txt");
        let cfg = PlanConfig { deref: Deref::Never, ..plan_cfg(false) };
        let plan = build_plan(&[spec(&link)], &dst, &cfg).unwrap();
        execute_plan(&plan, &quiet_opts(), &no_progress()).unwrap();
        assert!(dst.symlink_metadata().unwrap().file_type().is_symlink());
    }

    #[cfg(unix)]
    #[test]
    fn dereference_copies_symlink_targets_as_files() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("d");
        write(&src.join("real.txt"), b"data");
        std::os::unix::fs::symlink("real.txt", src.join("link.txt")).unwrap();
        let out = t.path().join("out");
        let cfg = PlanConfig { recursive: true, deref: Deref::Always, ..plan_cfg(true) };
        let plan = build_plan(&[spec(&src)], &out, &cfg).unwrap();
        let stats = execute_plan(&plan, &quiet_opts(), &no_progress()).unwrap();
        assert!(stats.errors.is_empty());
        let copied = out.join("link.txt");
        assert!(!copied.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(fs::read(&copied).unwrap(), b"data");
    }

    #[cfg(unix)]
    #[test]
    fn writes_through_destination_symlink() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("src.txt");
        write(&src, b"nouveau");
        let target = t.path().join("target.txt");
        write(&target, b"ancien");
        let dst = t.path().join("lien.txt");
        std::os::unix::fs::symlink(&target, &dst).unwrap();

        let plan = quick_plan(&src, &dst, false);
        assert!(plan.errors.is_empty());
        execute_plan(&plan, &quiet_opts(), &no_progress()).unwrap();
        // Le lien est conservé, la cible a le nouveau contenu (comme cp).
        assert!(dst.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(fs::read(&target).unwrap(), b"nouveau");
    }

    #[cfg(unix)]
    #[test]
    fn dangling_destination_symlink_is_refused() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("src.txt");
        write(&src, b"x");
        let dst = t.path().join("lien.txt");
        std::os::unix::fs::symlink(t.path().join("absent"), &dst).unwrap();
        let plan = quick_plan(&src, &dst, false);
        assert!(!plan.errors.is_empty());
        assert!(plan.entries.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn remove_destination_replaces_symlink_itself() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("src.txt");
        write(&src, b"nouveau");
        let target = t.path().join("target.txt");
        write(&target, b"ancien");
        let dst = t.path().join("lien.txt");
        std::os::unix::fs::symlink(&target, &dst).unwrap();

        let cfg = PlanConfig { remove_destination: true, ..plan_cfg(false) };
        let opts = CopyOptions { remove_destination: true, ..Default::default() };
        let plan = build_plan(&[spec(&src)], &dst, &cfg).unwrap();
        let stats = execute_plan(&plan, &opts, &no_progress()).unwrap();
        assert!(stats.errors.is_empty());
        // Le lien a disparu, la cible est intacte.
        assert!(!dst.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(fs::read(&dst).unwrap(), b"nouveau");
        assert_eq!(fs::read(&target).unwrap(), b"ancien");
    }

    #[cfg(unix)]
    #[test]
    fn same_file_via_hardlink_is_refused() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("a.txt");
        write(&src, b"x");
        let dst = t.path().join("b.txt");
        fs::hard_link(&src, &dst).unwrap();
        let plan = quick_plan(&src, &dst, false);
        assert!(!plan.errors.is_empty());
    }

    // --- Modes -l / -s et --preserve=links ---

    #[cfg(unix)]
    #[test]
    fn link_mode_creates_hard_links() {
        use std::os::unix::fs::MetadataExt;
        let t = TempDir::new().unwrap();
        let src = t.path().join("a.txt");
        let dst = t.path().join("b.txt");
        write(&src, b"data");
        let opts = CopyOptions { mode: CopyMode::Link, ..Default::default() };
        let plan = quick_plan(&src, &dst, false);
        let stats = execute_plan(&plan, &opts, &no_progress()).unwrap();
        assert!(stats.errors.is_empty());
        assert_eq!(fs::metadata(&src).unwrap().ino(), fs::metadata(&dst).unwrap().ino());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_mode_creates_links_to_absolute_sources() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("a.txt");
        write(&src, b"data");
        let dst = t.path().join("sub/b.txt");
        let opts = CopyOptions { mode: CopyMode::Symlink, ..Default::default() };
        let plan = quick_plan(&src, &dst, false);
        let stats = execute_plan(&plan, &opts, &no_progress()).unwrap();
        assert!(stats.errors.is_empty());
        assert_eq!(fs::read_link(&dst).unwrap(), src);
        assert_eq!(fs::read(&dst).unwrap(), b"data");
    }

    #[cfg(unix)]
    #[test]
    fn preserve_links_keeps_hard_links_together() {
        use std::os::unix::fs::MetadataExt;
        let t = TempDir::new().unwrap();
        let src = t.path().join("d");
        write(&src.join("original"), b"data");
        fs::hard_link(src.join("original"), src.join("clone")).unwrap();
        let out = t.path().join("out");
        let cfg = PlanConfig { recursive: true, preserve_links: true, ..plan_cfg(true) };
        let plan = build_plan(&[spec(&src)], &out, &cfg).unwrap();
        let stats = execute_plan(&plan, &quiet_opts(), &no_progress()).unwrap();
        assert!(stats.errors.is_empty());
        let i1 = fs::metadata(out.join("original")).unwrap().ino();
        let i2 = fs::metadata(out.join("clone")).unwrap().ino();
        assert_eq!(i1, i2);
    }

    #[cfg(unix)]
    #[test]
    fn without_preserve_links_hard_links_are_split() {
        use std::os::unix::fs::MetadataExt;
        let t = TempDir::new().unwrap();
        let src = t.path().join("d");
        write(&src.join("original"), b"data");
        fs::hard_link(src.join("original"), src.join("clone")).unwrap();
        let out = t.path().join("out");
        let plan = quick_plan(&src, &out, true);
        execute_plan(&plan, &quiet_opts(), &no_progress()).unwrap();
        let i1 = fs::metadata(out.join("original")).unwrap().ino();
        let i2 = fs::metadata(out.join("clone")).unwrap().ino();
        assert_ne!(i1, i2);
    }

    // --- Préservation des attributs ---

    #[cfg(unix)]
    #[test]
    fn archive_preserves_permissions_and_times() {
        use std::os::unix::fs::PermissionsExt;
        let t = TempDir::new().unwrap();
        let src = t.path().join("x.sh");
        write(&src, b"#!/bin/sh\n");
        fs::set_permissions(&src, fs::Permissions::from_mode(0o750)).unwrap();
        let when = FileTime::from_unix_time(1_500_000_000, 0);
        set_file_times(&src, when, when).unwrap();

        let dst = t.path().join("y.sh");
        let opts = CopyOptions { preserve: Preserve::ALL, ..Default::default() };
        let plan = quick_plan(&src, &dst, false);
        let stats = execute_plan(&plan, &opts, &no_progress()).unwrap();
        assert!(stats.errors.is_empty());
        assert_eq!(fs::metadata(&dst).unwrap().permissions().mode() & 0o777, 0o750);
        let mtime = fs::metadata(&dst).unwrap().modified().unwrap();
        assert_eq!(mtime, std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_500_000_000));
    }

    #[test]
    fn attributes_only_creates_empty_file_with_mode() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("a.txt");
        write(&src, b"du contenu");
        let dst = t.path().join("b.txt");
        let opts = CopyOptions { attributes_only: true, ..Default::default() };
        let plan = quick_plan(&src, &dst, false);
        let stats = execute_plan(&plan, &opts, &no_progress()).unwrap();
        assert!(stats.errors.is_empty());
        assert_eq!(fs::read(&dst).unwrap(), b"");
        assert_eq!(stats.bytes_copied, 0);
    }

    // --- --parents ---

    #[test]
    fn parents_recreates_full_source_path() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("a/b/c.txt");
        write(&src, b"x");
        let out = t.path().join("out");
        let cfg = PlanConfig { parents: true, ..plan_cfg(false) };
        let plan = build_plan(&[spec(&src)], &out, &cfg).unwrap();
        execute_plan(&plan, &quiet_opts(), &no_progress()).unwrap();
        let expected = out.join(strip_root(&src));
        assert_eq!(fs::read(&expected).unwrap(), b"x");
    }

    // --- Fichiers creux et spéciaux ---

    #[cfg(unix)]
    #[test]
    fn sparse_copy_keeps_content_and_size() {
        use std::os::unix::fs::MetadataExt;
        let t = TempDir::new().unwrap();
        let src = t.path().join("creux.bin");
        let f = fs::File::create(&src).unwrap();
        use std::io::Seek;
        let mut f = f;
        f.seek(SeekFrom::Start(1024 * 1024)).unwrap();
        f.write_all(b"fin").unwrap();
        drop(f);

        let dst = t.path().join("copie.bin");
        let plan = quick_plan(&src, &dst, false);
        assert!(matches!(plan.entries[0], PlanEntry::File { sparse_hint: true, .. }));
        let stats = execute_plan(&plan, &quiet_opts(), &no_progress()).unwrap();
        assert!(stats.errors.is_empty());
        assert_eq!(fs::metadata(&dst).unwrap().len(), 1024 * 1024 + 3);
        assert_eq!(fs::read(&dst).unwrap(), fs::read(&src).unwrap());
        // La copie reste creuse : pas plus de blocs que la source.
        assert!(fs::metadata(&dst).unwrap().blocks() <= fs::metadata(&src).unwrap().blocks() + 8);
    }

    #[cfg(unix)]
    #[test]
    fn recursive_copy_recreates_fifos() {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::FileTypeExt;
        let t = TempDir::new().unwrap();
        let src = t.path().join("d");
        fs::create_dir(&src).unwrap();
        let fifo = src.join("tube");
        let c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);

        let out = t.path().join("out");
        let plan = quick_plan(&src, &out, true);
        let stats = execute_plan(&plan, &quiet_opts(), &no_progress()).unwrap();
        assert!(stats.errors.is_empty());
        assert!(out.join("tube").symlink_metadata().unwrap().file_type().is_fifo());
    }

    #[test]
    fn dev_null_is_copied_as_empty_file() {
        let t = TempDir::new().unwrap();
        let dst = t.path().join("vide");
        let plan = quick_plan(Path::new("/dev/null"), &dst, false);
        let stats = execute_plan(&plan, &quiet_opts(), &no_progress()).unwrap();
        assert!(stats.errors.is_empty());
        assert_eq!(fs::read(&dst).unwrap(), b"");
    }

    #[cfg(unix)]
    #[test]
    fn file_over_existing_directory_is_an_error() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("a.txt");
        write(&src, b"x");
        let dst = t.path().join("d");
        fs::create_dir(&dst).unwrap();
        // -T : la destination est traitée comme un nom, pas un répertoire.
        let cfg = PlanConfig { dest_never_dir: true, ..plan_cfg(false) };
        let plan = build_plan(&[spec(&src)], &dst, &cfg).unwrap();
        let stats = execute_plan(&plan, &quiet_opts(), &no_progress()).unwrap();
        assert_eq!(stats.errors.len(), 1);
        assert!(dst.is_dir());
    }

    // --- Extensions wcp : --verify, -j, --bwlimit, --exclude ---

    #[test]
    fn verify_passes_on_intact_copy() {
        let t = TempDir::new().unwrap();
        let src = make_tree(&t);
        let out = t.path().join("out");
        let opts = CopyOptions { verify: true, ..Default::default() };
        let plan = quick_plan(&src, &out, true);
        let stats = execute_plan(&plan, &opts, &no_progress()).unwrap();
        assert!(stats.errors.is_empty());
        assert_eq!(stats.verified, 2);
        assert_eq!(fs::read(out.join("a.txt")).unwrap(), b"aaa");
    }

    #[test]
    fn verify_detects_mismatch() {
        let t = TempDir::new().unwrap();
        let a = t.path().join("a");
        let b = t.path().join("b");
        write(&a, b"identique");
        write(&b, b"identique");
        assert!(verify_identical(&a, &b).is_ok());
        write(&b, b"different");
        assert!(verify_identical(&a, &b).is_err());
    }

    #[test]
    fn parallel_copy_matches_sequential() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("d");
        for i in 0..20 {
            write(&src.join(format!("sub{i}/f{i}.bin")), format!("contenu-{i}").as_bytes());
        }
        let out = t.path().join("out");
        let opts = CopyOptions { jobs: 4, ..Default::default() };
        let plan = quick_plan(&src, &out, true);
        let stats = execute_plan(&plan, &opts, &no_progress()).unwrap();
        assert!(stats.errors.is_empty());
        assert_eq!(stats.files_copied, 20);
        for i in 0..20 {
            assert_eq!(
                fs::read(out.join(format!("sub{i}/f{i}.bin"))).unwrap(),
                format!("contenu-{i}").as_bytes()
            );
        }
    }

    #[test]
    fn parallel_preserves_hard_links_after_phase_two() {
        use std::os::unix::fs::MetadataExt;
        let t = TempDir::new().unwrap();
        let src = t.path().join("d");
        write(&src.join("original"), b"data");
        fs::hard_link(src.join("original"), src.join("clone")).unwrap();
        let out = t.path().join("out");
        let cfg = PlanConfig { recursive: true, preserve_links: true, ..plan_cfg(true) };
        let plan = build_plan(&[spec(&src)], &out, &cfg).unwrap();
        let opts = CopyOptions { jobs: 4, ..Default::default() };
        let stats = execute_plan(&plan, &opts, &no_progress()).unwrap();
        assert!(stats.errors.is_empty());
        let i1 = fs::metadata(out.join("original")).unwrap().ino();
        let i2 = fs::metadata(out.join("clone")).unwrap().ino();
        assert_eq!(i1, i2);
    }

    #[test]
    fn bwlimit_slows_the_copy() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("big.bin");
        write(&src, &vec![7u8; 512 * 1024]);
        let dst = t.path().join("copie.bin");
        let opts = CopyOptions { bwlimit: Some(1024 * 1024), ..Default::default() };
        let plan = quick_plan(&src, &dst, false);
        let start = std::time::Instant::now();
        let stats = execute_plan(&plan, &opts, &no_progress()).unwrap();
        let elapsed = start.elapsed();
        assert!(stats.errors.is_empty());
        // 512 Kio à 1 Mio/s ≈ 0.5 s minimum.
        assert!(elapsed >= std::time::Duration::from_millis(350), "trop rapide : {elapsed:?}");
        assert_eq!(fs::read(&dst).unwrap(), vec![7u8; 512 * 1024]);
    }

    fn globset_of(patterns: &[&str]) -> globset::GlobSet {
        let mut b = globset::GlobSetBuilder::new();
        for p in patterns {
            b.add(globset::GlobBuilder::new(p).literal_separator(false).build().unwrap());
        }
        b.build().unwrap()
    }

    #[test]
    fn exclude_skips_matching_files_anywhere() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("d");
        write(&src.join("keep.txt"), b"k");
        write(&src.join("drop.log"), b"x");
        write(&src.join("sub/drop.log"), b"x");
        let out = t.path().join("out");
        let cfg = PlanConfig { recursive: true, exclude: Some(globset_of(&["*.log"])), ..plan_cfg(true) };
        let plan = build_plan(&[spec(&src)], &out, &cfg).unwrap();
        assert_eq!(plan.excluded, 2);
        let stats = execute_plan(&plan, &quiet_opts(), &no_progress()).unwrap();
        assert!(stats.errors.is_empty());
        assert!(out.join("keep.txt").exists());
        assert!(!out.join("drop.log").exists());
        assert!(!out.join("sub/drop.log").exists());
    }

    #[test]
    fn exclude_prunes_whole_directories() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("d");
        write(&src.join("cache/x.bin"), b"x");
        write(&src.join("cache/deep/y.bin"), b"y");
        write(&src.join("keep/z.bin"), b"z");
        let out = t.path().join("out");
        let cfg = PlanConfig { recursive: true, exclude: Some(globset_of(&["cache"])), ..plan_cfg(true) };
        let plan = build_plan(&[spec(&src)], &out, &cfg).unwrap();
        assert_eq!(plan.excluded, 1);
        execute_plan(&plan, &quiet_opts(), &no_progress()).unwrap();
        assert!(!out.join("cache").exists());
        assert!(out.join("keep/z.bin").exists());
    }
}
