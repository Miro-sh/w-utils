//! Logique de déplacement (mv) : rename(2) instantané quand source et
//! destination sont sur le même système de fichiers ; sinon fallback
//! copie + suppression (EXDEV) via le moteur de copie partagé.
//!
//! Différences clés avec cp :
//! - un lien symbolique de destination est REMPLACÉ, jamais écrit à travers ;
//! - tout est préservé par défaut (équivalent cp -a) lors du fallback ;
//! - la source n'est supprimée qu'après une copie intégralement réussie.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::copy::{
    self, backup_existing, decide_overwrite, ensure_not_nested, file_ids, has_trailing_slash,
    CopyOptions, CopyStats, Decision, PlanConfig, Reflink, SourceSpec, Sparse,
};
use crate::options::{BackupControl, CopyMode, Deref, Overwrite, Preserve};
use crate::progress::CopyProgress;

pub struct MoveOptions {
    pub overwrite: Overwrite,
    pub backup: Option<BackupControl>,
    pub backup_suffix: String,
    /// -T : la destination est toujours un nom de fichier, jamais un répertoire.
    pub dest_never_dir: bool,
    pub verbose: bool,
    pub resume: bool,
    pub verify: bool,
    pub jobs: usize,
    pub bwlimit: Option<u64>,
    pub exclude: Option<globset::GlobSet>,
    /// Barre de progression affichée pendant un fallback cross-device.
    pub show_progress: bool,
}

impl Default for MoveOptions {
    fn default() -> Self {
        Self {
            overwrite: Overwrite::Clobber,
            backup: None,
            backup_suffix: String::from("~"),
            dest_never_dir: false,
            verbose: false,
            resume: false,
            verify: false,
            jobs: 1,
            bwlimit: None,
            exclude: None,
            show_progress: false,
        }
    }
}

#[derive(Default)]
pub struct MoveStats {
    /// Sources déplacées par simple rename (instantané, même FS).
    pub renamed: usize,
    /// Sources déplacées par copie + suppression (cross-device).
    pub moved_across: usize,
    /// Ignorées par la politique d'écrasement (-n, -u, -i).
    pub skipped: usize,
    /// Statistiques agrégées des phases de copie cross-device.
    pub copy: CopyStats,
    pub errors: Vec<String>,
}

/// Action prévue pour une source, affichée par --dry-run.
pub enum PlannedAction {
    /// rename(2) : instantané.
    Rename,
    /// Copie + suppression : systèmes de fichiers différents.
    CopyThenDelete { bytes: u64, files: usize },
}

/// Résout la destination finale d'une source (sémantique mv = cp).
pub fn resolve_dest(src: &Path, destination: &Path, dest_never_dir: bool) -> PathBuf {
    if dest_never_dir {
        destination.to_path_buf()
    } else if destination.is_dir() || has_trailing_slash(destination) {
        match src.file_name() {
            Some(name) => destination.join(name),
            None => destination.to_path_buf(),
        }
    } else {
        destination.to_path_buf()
    }
}

/// Résultat de l'analyse d'une source pour --dry-run.
pub struct PlannedMove {
    pub dst_root: PathBuf,
    pub action: PlannedAction,
}

/// Prévisualisation --dry-run : valide une source et annonce l'action prévue.
pub fn plan_one(spec: &SourceSpec, destination: &Path, opts: &MoveOptions) -> Result<PlannedMove, String> {
    let src = &spec.path;
    let Some(meta) = check_source(src) else {
        return Err(format!("impossible d'accéder à '{}'", src.display()));
    };
    let dst_root = resolve_dest(src, destination, opts.dest_never_dir);
    if let Some(err) = validate_pair(src, &dst_root, &meta) {
        return Err(err);
    }
    if copy::same_file_system(src, &dst_root) {
        Ok(PlannedMove { dst_root, action: PlannedAction::Rename })
    } else {
        // Estimation du volume pour l'affichage.
        let plan_cfg = plan_config_for(opts);
        match copy::build_plan(std::slice::from_ref(spec), destination, &plan_cfg) {
            Ok(plan) => Ok(PlannedMove {
                dst_root,
                action: PlannedAction::CopyThenDelete { bytes: plan.total_bytes, files: plan.file_count },
            }),
            Err(e) => Err(format!("{e:#}")),
        }
    }
}

/// Déplace les sources vers la destination. Comme mv, une source en erreur
/// n'empêche pas les autres d'être traitées.
pub fn run_move(
    sources: &[SourceSpec],
    destination: &Path,
    opts: &MoveOptions,
) -> MoveStats {
    let mut stats = MoveStats::default();
    for spec in sources {
        move_one(spec, destination, opts, &mut stats);
    }
    stats
}

fn move_one(spec: &SourceSpec, destination: &Path, opts: &MoveOptions, stats: &mut MoveStats) {
    let src = &spec.path;
    let Some(meta) = check_source(src) else {
        stats.errors.push(format!("impossible d'accéder à '{}'", src.display()));
        return;
    };
    let dst_root = resolve_dest(src, destination, opts.dest_never_dir);
    if let Some(err) = validate_pair(src, &dst_root, &meta) {
        stats.errors.push(err);
        return;
    }

    // Politique d'écrasement. Pour un répertoire, elle ne vaut que le rename :
    // en fallback cross-device, l'exécuteur re-décidera par fichier (comme GNU).
    match decide_overwrite(opts.overwrite, src, &dst_root, false) {
        Decision::Skip => {
            stats.skipped += 1;
            return;
        }
        Decision::Fail(msg) => {
            stats.errors.push(msg);
            return;
        }
        Decision::Go => {}
    }

    let copy_opts = copy_options_for(opts, meta.is_dir());

    if let Err(e) = backup_existing(&dst_root, &copy_opts) {
        stats.errors.push(format!("sauvegarde de '{}' : {}", dst_root.display(), e));
        return;
    }

    // rename(2) d'abord : instantané sur le même système de fichiers.
    match fs::rename(src, &dst_root) {
        Ok(()) => {
            stats.renamed += 1;
            if opts.verbose {
                println!("{} -> {} (renommé)", src.display(), dst_root.display());
            }
        }
        Err(e) if is_exdev(&e) => cross_device_move(spec, destination, &dst_root, opts, &copy_opts, stats),
        Err(e) => stats.errors.push(format!("{} -> {} : {}", src.display(), dst_root.display(), e)),
    }
}

/// Fallback EXDEV : copie récursive complète (équivalent cp -a), puis
/// suppression de la source — uniquement si la copie a réussi de bout en bout.
fn cross_device_move(
    spec: &SourceSpec,
    destination: &Path,
    dst_root: &Path,
    opts: &MoveOptions,
    copy_opts: &CopyOptions,
    stats: &mut MoveStats,
) {
    let plan_cfg = plan_config_for(opts);
    let plan = match copy::build_plan(std::slice::from_ref(spec), destination, &plan_cfg) {
        Ok(p) => p,
        Err(e) => {
            stats.errors.push(format!("{e:#}"));
            return;
        }
    };
    for e in &plan.errors {
        stats.errors.push(e.clone());
    }

    let progress = CopyProgress::new(plan.total_bytes, opts.show_progress);
    let copy_stats = match copy::execute_plan(&plan, copy_opts, &progress) {
        Ok(s) => s,
        Err(e) => {
            stats.errors.push(format!("{e:#}"));
            return;
        }
    };
    progress.finish();

    let clean = plan.errors.is_empty() && copy_stats.errors.is_empty();
    if opts.verbose {
        for w in &copy_stats.warnings {
            println!("! {w}");
        }
    }
    stats.copy.merge(copy_stats);

    if clean {
        // La source n'est supprimée qu'après une copie intégralement réussie.
        match remove_source(&spec.path) {
            Ok(()) => {
                stats.moved_across += 1;
                if opts.verbose {
                    println!("{} -> {} (copié puis supprimé)", spec.path.display(), dst_root.display());
                }
            }
            Err(e) => stats.errors.push(format!(
                "copie réussie mais impossible de supprimer la source '{}' : {}",
                spec.path.display(),
                e
            )),
        }
    } else {
        stats.errors.push(format!("'{}' conservée suite aux erreurs de copie", spec.path.display()));
    }
}

/// Suppression de la source après un move cross-device réussi.
fn remove_source(src: &Path) -> io::Result<()> {
    let meta = fs::symlink_metadata(src)?;
    if meta.is_dir() {
        fs::remove_dir_all(src)
    } else {
        fs::remove_file(src)
    }
}

fn check_source(src: &Path) -> Option<fs::Metadata> {
    fs::symlink_metadata(src).ok()
}

/// Validations communes : « même fichier », type dir/fichier, récursion.
fn validate_pair(src: &Path, dst_root: &Path, meta: &fs::Metadata) -> Option<String> {
    // mv détecte la copie d'un fichier sur lui-même via (dev, ino).
    if let Ok(dm) = dst_root.symlink_metadata() {
        let (sdev, sino, _) = file_ids(meta);
        let (ddev, dino, _) = file_ids(&dm);
        if (ddev, dino) == (sdev, sino) {
            return Some(format!("'{}' et '{}' sont le même fichier", src.display(), dst_root.display()));
        }
    }
    if meta.is_dir() {
        if dst_root.symlink_metadata().is_ok() && !dst_root.is_dir() {
            return Some(format!("impossible d'écraser '{}' par un répertoire", dst_root.display()));
        }
        if let Err(e) = ensure_not_nested(src, dst_root) {
            return Some(format!("{e:#}"));
        }
    } else if dst_root.symlink_metadata().map(|m| m.is_dir() && !m.file_type().is_symlink()).unwrap_or(false) {
        // Fichier sur un vrai répertoire (un lien vers un répertoire, lui,
        // est remplacé — rename le fait naturellement).
        return Some(format!("impossible d'écraser le répertoire '{}' par un fichier", dst_root.display()));
    }
    None
}

/// Options de la phase copie du fallback. mv préserve TOUT (comme cp -a) et
/// remplace les liens symboliques de destination (jamais de write-through).
fn copy_options_for(opts: &MoveOptions, is_dir: bool) -> CopyOptions {
    CopyOptions {
        mode: CopyMode::Copy,
        // Fichier seul : la décision a déjà été prise avant le rename.
        // Répertoire : l'exécuteur re-décide par fichier (comportement GNU).
        overwrite: if is_dir { opts.overwrite } else { Overwrite::Clobber },
        preserve: Preserve::ALL,
        backup: opts.backup,
        backup_suffix: opts.backup_suffix.clone(),
        reflink: Reflink::Off,
        sparse: Sparse::Auto,
        remove_destination: true,
        attributes_only: false,
        verbose: opts.verbose,
        resume: opts.resume,
        verify: opts.verify,
        jobs: opts.jobs,
        bwlimit: opts.bwlimit,
    }
}

fn plan_config_for(opts: &MoveOptions) -> PlanConfig {
    PlanConfig {
        recursive: true,
        // mv déplace les liens symboliques eux-mêmes, jamais leur contenu.
        deref: Deref::Never,
        parents: false,
        one_file_system: false,
        copy_contents: false,
        preserve_links: true,
        dest_never_dir: opts.dest_never_dir,
        remove_destination: true,
        exclude: opts.exclude.clone(),
    }
}

#[cfg(unix)]
fn is_exdev(e: &io::Error) -> bool {
    e.raw_os_error() == Some(libc::EXDEV)
}

#[cfg(not(unix))]
fn is_exdev(_e: &io::Error) -> bool {
    false
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use filetime::{set_file_times, FileTime};
    use tempfile::TempDir;

    fn write(p: &Path, content: &[u8]) {
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    fn spec(p: &Path) -> SourceSpec {
        SourceSpec { path: p.to_path_buf(), follow: false }
    }

    fn quiet() -> MoveOptions {
        MoveOptions::default()
    }

    #[test]
    fn rename_moves_file_instantly() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("a.txt");
        let dst = t.path().join("b.txt");
        write(&src, b"data");
        let stats = run_move(&[spec(&src)], &dst, &quiet());
        assert!(stats.errors.is_empty());
        assert_eq!(stats.renamed, 1);
        assert!(!src.exists());
        assert_eq!(fs::read(&dst).unwrap(), b"data");
    }

    #[test]
    fn move_multiple_sources_into_dir() {
        let t = TempDir::new().unwrap();
        let a = t.path().join("a.txt");
        let b = t.path().join("b.txt");
        write(&a, b"A");
        write(&b, b"B");
        let dest = t.path().join("dest");
        fs::create_dir(&dest).unwrap();
        let stats = run_move(&[spec(&a), spec(&b)], &dest, &quiet());
        assert!(stats.errors.is_empty());
        assert_eq!(stats.renamed, 2);
        assert_eq!(fs::read(dest.join("a.txt")).unwrap(), b"A");
        assert!(!a.exists() && !b.exists());
    }

    #[cfg(unix)]
    #[test]
    fn move_replaces_destination_symlink_itself() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("src.txt");
        write(&src, b"nouveau");
        let target = t.path().join("target.txt");
        write(&target, b"ancien");
        let dst = t.path().join("lien.txt");
        std::os::unix::fs::symlink(&target, &dst).unwrap();

        let stats = run_move(&[spec(&src)], &dst, &quiet());
        assert!(stats.errors.is_empty());
        // Contrairement à cp : le lien est remplacé, la cible est INTACTE.
        assert!(!dst.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(fs::read(&dst).unwrap(), b"nouveau");
        assert_eq!(fs::read(&target).unwrap(), b"ancien");
    }

    #[cfg(unix)]
    #[test]
    fn move_symlink_keeps_it_a_symlink() {
        let t = TempDir::new().unwrap();
        let target = t.path().join("real.txt");
        write(&target, b"data");
        let link = t.path().join("lien.txt");
        std::os::unix::fs::symlink("real.txt", &link).unwrap();
        let dst = t.path().join("out.txt");
        let stats = run_move(&[spec(&link)], &dst, &quiet());
        assert!(stats.errors.is_empty());
        assert!(dst.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(fs::read_link(&dst).unwrap(), PathBuf::from("real.txt"));
    }

    #[test]
    fn no_clobber_skips_move() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("a.txt");
        let dst = t.path().join("b.txt");
        write(&src, b"nouveau");
        write(&dst, b"ancien");
        let opts = MoveOptions { overwrite: Overwrite::NoClobber, ..Default::default() };
        let stats = run_move(&[spec(&src)], &dst, &opts);
        assert_eq!(stats.skipped, 1);
        assert!(src.exists());
        assert_eq!(fs::read(&dst).unwrap(), b"ancien");
    }

    #[test]
    fn update_skips_when_dest_is_newer() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("a.txt");
        let dst = t.path().join("b.txt");
        write(&src, b"nouveau");
        write(&dst, b"ancien");
        let old = FileTime::from_unix_time(1_000_000, 0);
        set_file_times(&src, old, old).unwrap();
        let opts = MoveOptions { overwrite: Overwrite::Update, ..Default::default() };
        let stats = run_move(&[spec(&src)], &dst, &opts);
        assert_eq!(stats.skipped, 1);
        assert!(src.exists());
    }

    #[test]
    fn backup_on_rename_overwrite() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("a.txt");
        let dst = t.path().join("b.txt");
        write(&src, b"nouveau");
        write(&dst, b"ancien");
        let opts = MoveOptions { backup: Some(BackupControl::Simple), ..Default::default() };
        let stats = run_move(&[spec(&src)], &dst, &opts);
        assert!(stats.errors.is_empty());
        assert_eq!(fs::read(&dst).unwrap(), b"nouveau");
        assert_eq!(fs::read(t.path().join("b.txt~")).unwrap(), b"ancien");
    }

    #[cfg(unix)]
    #[test]
    fn same_file_via_hardlink_refused() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("a.txt");
        write(&src, b"x");
        let dst = t.path().join("b.txt");
        fs::hard_link(&src, &dst).unwrap();
        let stats = run_move(&[spec(&src)], &dst, &quiet());
        assert_eq!(stats.errors.len(), 1);
        assert!(src.exists());
    }

    #[test]
    fn dir_onto_file_is_an_error() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("d");
        fs::create_dir(&src).unwrap();
        let dst = t.path().join("f.txt");
        write(&dst, b"x");
        let stats = run_move(&[spec(&src)], &dst, &quiet());
        assert_eq!(stats.errors.len(), 1);
        assert!(src.is_dir());
    }

    #[test]
    fn move_into_own_subdir_refused() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("d");
        write(&src.join("a.txt"), b"x");
        let stats = run_move(&[spec(&src)], &src.join("sub"), &quiet());
        assert_eq!(stats.errors.len(), 1);
        assert!(src.is_dir());
    }

    #[test]
    fn dry_run_touches_nothing() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("a.txt");
        write(&src, b"data");
        let dst = t.path().join("b.txt");
        let planned = plan_one(&spec(&src), &dst, &quiet()).unwrap();
        assert_eq!(planned.dst_root, dst.clone());
        assert!(matches!(planned.action, PlannedAction::Rename));
        assert!(src.exists() && !dst.exists());
    }

    /// Un move cross-device réel, si /dev/shm est un autre montage (Linux).
    #[cfg(target_os = "linux")]
    #[test]
    fn cross_device_move_copies_then_deletes() {
        let shm = Path::new("/dev/shm");
        if !shm.is_dir() {
            return;
        }
        let t = TempDir::new().unwrap();
        let probe_here = t.path();
        if copy::same_file_system(probe_here, shm) {
            return; // même FS : impossible de tester EXDEV ici
        }
        let src = t.path().join("d");
        write(&src.join("a.txt"), b"contenu");
        std::os::unix::fs::symlink("a.txt", src.join("lien")).unwrap();
        let dst = shm.join(format!("wmv-test-{}", std::process::id()));

        let stats = run_move(&[spec(&src)], &dst, &quiet());
        assert!(stats.errors.is_empty(), "erreurs : {:?}", stats.errors);
        assert_eq!(stats.moved_across, 1);
        assert!(!src.exists());
        assert_eq!(fs::read(dst.join("a.txt")).unwrap(), b"contenu");
        assert!(dst.join("lien").symlink_metadata().unwrap().file_type().is_symlink());
        let _ = fs::remove_dir_all(&dst);
    }
}
