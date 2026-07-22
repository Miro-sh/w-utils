//! Logique de copie : planification, exécution et sécurité.
//!
//! Chaque fichier est d'abord copié sous un nom temporaire dans le dossier
//! de destination, puis renommée atomiquement : en cas d'interruption
//! (Ctrl+C, erreur disque), la destination ne contient jamais de fichier
//! partiellement copié.

use std::ffi::OsString;
use std::fs;
use std::io::{self, ErrorKind, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::process;
use std::sync::{Mutex, OnceLock};

use anyhow::{bail, Context, Result};
use filetime::{set_file_times, FileTime};
use walkdir::WalkDir;

use crate::progress::CopyProgress;

/// Buffer standard pour la copie avec progression.
const BUFFER_SMALL: usize = 256 * 1024;
/// Buffer élargi pour les très gros fichiers (moins d'appels système).
const BUFFER_LARGE: usize = 4 * 1024 * 1024;
/// Au-delà d'1 Gio on passe sur le gros buffer.
const LARGE_FILE_THRESHOLD: u64 = 1 << 30;

pub struct CopyOptions {
    pub archive: bool,
    pub verbose: bool,
}

/// Une entrée du plan de copie (fichier, lien symbolique ou répertoire).
pub enum PlanEntry {
    File { src: PathBuf, dst: PathBuf, size: u64 },
    Symlink { src: PathBuf, dst: PathBuf },
    Dir { src: PathBuf, dst: PathBuf },
}

/// Résultat de la phase d'analyse, avant toute écriture.
pub struct CopyPlan {
    pub entries: Vec<PlanEntry>,
    pub total_bytes: u64,
    pub file_count: usize,
    /// Fichiers spéciaux (sockets, fifos, devices) ignorés.
    pub skipped: Vec<PathBuf>,
}

#[derive(Default)]
pub struct CopyStats {
    pub files_copied: usize,
    pub dirs_created: usize,
    pub bytes_copied: u64,
    pub warnings: Vec<String>,
    pub errors: Vec<String>,
}

// ---------------------------------------------------------------------------
// Planification
// ---------------------------------------------------------------------------

/// Analyse la source et construit la liste complète des opérations à faire.
pub fn build_plan(source: &Path, destination: &Path, recursive: bool) -> Result<CopyPlan> {
    if !source.exists() {
        bail!("{} : aucun fichier ou dossier de ce type", source.display());
    }
    let src_meta = fs::metadata(source)
        .with_context(|| format!("impossible de lire les métadonnées de '{}'", source.display()))?;

    if src_meta.is_dir() && !recursive {
        bail!(
            "omission du répertoire '{}' (utilisez -r pour copier récursivement)",
            source.display()
        );
    }

    let dst_root = resolve_dest_root(source, destination);
    ensure_not_nested(source, &dst_root)?;

    let mut plan = CopyPlan {
        entries: Vec::new(),
        total_bytes: 0,
        file_count: 0,
        skipped: Vec::new(),
    };

    if src_meta.is_dir() {
        // La racine elle-même, pour préserver les répertoires vides.
        plan.entries.push(PlanEntry::Dir {
            src: source.to_path_buf(),
            dst: dst_root.clone(),
        });
        for item in WalkDir::new(source).follow_links(false).min_depth(1) {
            let entry = item
                .with_context(|| format!("erreur lors du parcours de '{}'", source.display()))?;
            let rel = entry.path().strip_prefix(source).unwrap_or(entry.path());
            let dst = dst_root.join(rel);
            let ft = entry.file_type();
            if ft.is_symlink() {
                plan.entries.push(PlanEntry::Symlink {
                    src: entry.path().to_path_buf(),
                    dst,
                });
            } else if ft.is_dir() {
                plan.entries.push(PlanEntry::Dir {
                    src: entry.path().to_path_buf(),
                    dst,
                });
            } else if ft.is_file() {
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                plan.total_bytes += size;
                plan.file_count += 1;
                plan.entries.push(PlanEntry::File {
                    src: entry.path().to_path_buf(),
                    dst,
                    size,
                });
            } else {
                plan.skipped.push(entry.path().to_path_buf());
            }
        }
    } else if src_meta.is_file() {
        plan.total_bytes = src_meta.len();
        plan.file_count = 1;
        plan.entries.push(PlanEntry::File {
            src: source.to_path_buf(),
            dst: dst_root,
            size: src_meta.len(),
        });
    } else {
        bail!("'{}' est un fichier spécial, copie non supportée", source.display());
    }

    Ok(plan)
}

/// Sémantique de cp : si la destination est un répertoire existant, on copie
/// la source *dedans* en gardant son nom ; sinon la destination est le nom final.
/// Bonus UX : un « / » final signale un répertoire à créer (comme rsync).
fn resolve_dest_root(source: &Path, destination: &Path) -> PathBuf {
    if destination.is_dir() || has_trailing_slash(destination) {
        match source.file_name() {
            Some(name) => destination.join(name),
            None => destination.to_path_buf(),
        }
    } else {
        destination.to_path_buf()
    }
}

/// Path normalise et supprime les « / » finaux : il faut regarder la chaîne brute.
fn has_trailing_slash(p: &Path) -> bool {
    p.as_os_str()
        .to_string_lossy()
        .ends_with(std::path::MAIN_SEPARATOR)
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
pub fn execute_plan(plan: &CopyPlan, opts: &CopyOptions, progress: &CopyProgress) -> Result<CopyStats> {
    let mut stats = CopyStats::default();

    for skipped in &plan.skipped {
        stats
            .warnings
            .push(format!("ignoré (fichier spécial) : {}", skipped.display()));
    }

    // Sans barre de progression, on prend le chemin rapide (std::fs::copy,
    // qui utilise copy_file_range(2) sous Linux) plutôt que la copie par buffer.
    let progress_opt = if progress.is_enabled() { Some(progress) } else { None };

    let mut dirs: Vec<&PlanEntry> = Vec::new();

    for entry in &plan.entries {
        match entry {
            PlanEntry::Dir { dst, .. } => {
                dirs.push(entry);
                match fs::create_dir_all(dst) {
                    Ok(()) => stats.dirs_created += 1,
                    Err(e) => stats.errors.push(format!("{} : {}", dst.display(), e)),
                }
            }
            PlanEntry::Symlink { src, dst } => {
                let result = ensure_parent(dst).and_then(|()| copy_symlink(src, dst));
                match result {
                    Ok(()) => {
                        stats.files_copied += 1;
                        if opts.verbose {
                            progress.log(&format!("{} -> {} (lien symbolique)", src.display(), dst.display()));
                        }
                    }
                    Err(e) => stats
                        .errors
                        .push(format!("{} -> {} : {}", src.display(), dst.display(), e)),
                }
            }
            PlanEntry::File { src, dst, size } => {
                if let Err(e) = ensure_parent(dst) {
                    stats
                        .errors
                        .push(format!("{} : impossible de créer le répertoire parent ({})", dst.display(), e));
                    continue;
                }
                match copy_file_atomic(src, dst, *size, opts.archive, progress_opt) {
                    Ok(n) => {
                        stats.files_copied += 1;
                        stats.bytes_copied += n;
                        if opts.verbose {
                            progress.log(&format!(
                                "{} -> {} ({})",
                                src.display(),
                                dst.display(),
                                humansize::format_size(*size, humansize::DECIMAL)
                            ));
                        }
                    }
                    Err(e) => stats
                        .errors
                        .push(format!("{} -> {} : {}", src.display(), dst.display(), e)),
                }
            }
        }
    }

    // Mode archive : métadonnées des répertoires APPLIQUÉES EN DERNIER,
    // sinon l'écriture des fichiers écraserait les timestamps des dossiers.
    if opts.archive {
        for entry in dirs.into_iter().rev() {
            if let PlanEntry::Dir { src, dst } = entry {
                let _ = apply_metadata(src, dst); // non bloquant
            }
        }
    }

    Ok(stats)
}

fn ensure_parent(dst: &Path) -> io::Result<()> {
    match dst.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => fs::create_dir_all(parent),
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Copie d'un fichier : temporaire + renommage atomique
// ---------------------------------------------------------------------------

/// Copie `src` vers `dst` sans jamais laisser de fichier partiel visible.
pub fn copy_file_atomic(
    src: &Path,
    dst: &Path,
    size_hint: u64,
    archive: bool,
    progress: Option<&CopyProgress>,
) -> io::Result<u64> {
    let tmp = temp_path_for(dst);
    set_current_tmp(&tmp);

    let result = copy_inner(src, &tmp, size_hint, archive, progress).and_then(|n| {
        rename_over(&tmp, dst)?;
        Ok(n)
    });

    clear_current_tmp();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn copy_inner(
    src: &Path,
    tmp: &Path,
    size_hint: u64,
    archive: bool,
    progress: Option<&CopyProgress>,
) -> io::Result<u64> {
    let bytes = match progress {
        // Barre affichée : copie manuelle par buffer pour suivre l'avancement.
        Some(p) => copy_buffered(src, tmp, size_hint, p)?,
        // Silencieux : std::fs::copy (copy_file_range sous Linux, quasi zero-copy).
        None => fs::copy(src, tmp)?,
    };

    // std::fs::copy préserve déjà les permissions ; la copie par buffer non.
    if progress.is_some() || archive {
        apply_permissions(src, tmp)?;
    }
    if archive {
        let meta = fs::metadata(src)?;
        set_file_times(
            tmp,
            FileTime::from_last_access_time(&meta),
            FileTime::from_last_modification_time(&meta),
        )?;
    }
    Ok(bytes)
}

fn copy_buffered(src: &Path, dst: &Path, size_hint: u64, progress: &CopyProgress) -> io::Result<u64> {
    let mut reader = fs::File::open(src)?;
    let mut writer = fs::File::create(dst)?;
    let buf_size = if size_hint >= LARGE_FILE_THRESHOLD {
        BUFFER_LARGE
    } else {
        BUFFER_SMALL
    };
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
        progress.inc(n as u64);
    }
    writer.flush()?;
    Ok(total)
}

fn apply_permissions(src: &Path, dst: &Path) -> io::Result<()> {
    fs::set_permissions(dst, fs::metadata(src)?.permissions())
}

fn apply_metadata(src: &Path, dst: &Path) -> io::Result<()> {
    let meta = fs::metadata(src)?;
    fs::set_permissions(dst, meta.permissions())?;
    set_file_times(
        dst,
        FileTime::from_last_access_time(&meta),
        FileTime::from_last_modification_time(&meta),
    )
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
// Liens symboliques
// ---------------------------------------------------------------------------

#[cfg(unix)]
fn copy_symlink(src: &Path, dst: &Path) -> io::Result<()> {
    let target = fs::read_link(src)?;
    if dst.symlink_metadata().is_ok() {
        if dst.is_dir() && !dst.is_symlink() {
            fs::remove_dir_all(dst)?;
        } else {
            fs::remove_file(dst)?;
        }
    }
    std::os::unix::fs::symlink(target, dst)
}

#[cfg(not(unix))]
fn copy_symlink(src: &Path, _dst: &Path) -> io::Result<()> {
    Err(io::Error::new(
        ErrorKind::Unsupported,
        format!("lien symbolique non supporté sur cette plateforme : {}", src.display()),
    ))
}

// ---------------------------------------------------------------------------
// Gestion propre de Ctrl+C
// ---------------------------------------------------------------------------

/// Fichier temporaire en cours d'écriture (un seul à la fois : copie séquentielle).
static CURRENT_TMP: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

fn tmp_slot() -> &'static Mutex<Option<PathBuf>> {
    CURRENT_TMP.get_or_init(|| Mutex::new(None))
}

fn set_current_tmp(p: &Path) {
    if let Ok(mut g) = tmp_slot().lock() {
        *g = Some(p.to_path_buf());
    }
}

fn clear_current_tmp() {
    if let Ok(mut g) = tmp_slot().lock() {
        *g = None;
    }
}

/// Installe un handler Ctrl+C qui supprime le fichier temporaire en cours
/// avant de quitter (code 130, comme la convention shell).
pub fn install_ctrlc_handler() {
    let _ = ctrlc::set_handler(|| {
        if let Some(slot) = CURRENT_TMP.get() {
            // try_lock : si le thread principal tient le verrou, tant pis,
            // mieux vaut quitter vite que risquer un deadlock dans un handler.
            if let Ok(guard) = slot.try_lock() {
                if let Some(p) = guard.as_ref() {
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
        CopyOptions { archive: false, verbose: false }
    }

    fn make_tree(t: &TempDir) -> PathBuf {
        let src = t.path().join("d");
        write(&src.join("a.txt"), b"aaa");
        write(&src.join("sub/b.txt"), b"bbbbb");
        fs::create_dir_all(src.join("empty")).unwrap();
        src
    }

    #[test]
    fn plan_single_file() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("a.txt");
        write(&src, b"hello");
        let plan = build_plan(&src, &t.path().join("out"), false).unwrap();
        assert_eq!(plan.file_count, 1);
        assert_eq!(plan.total_bytes, 5);
        assert_eq!(plan.entries.len(), 1);
    }

    #[test]
    fn plan_requires_recursive_for_dirs() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("d");
        fs::create_dir(&src).unwrap();
        let err = build_plan(&src, &t.path().join("out"), false);
        assert!(err.is_err());
    }

    #[test]
    fn plan_missing_source_fails() {
        let t = TempDir::new().unwrap();
        assert!(build_plan(&t.path().join("nope"), &t.path().join("out"), false).is_err());
    }

    #[test]
    fn plan_recursive_walks_tree() {
        let t = TempDir::new().unwrap();
        let src = make_tree(&t);
        let plan = build_plan(&src, &t.path().join("out"), true).unwrap();
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
        let plan = build_plan(&src, &dst, false).unwrap();
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
        let plan = build_plan(&src, &out, true).unwrap();
        let stats = execute_plan(&plan, &quiet_opts(), &no_progress()).unwrap();
        assert!(stats.errors.is_empty());
        assert_eq!(fs::read(out.join("a.txt")).unwrap(), b"aaa");
        assert_eq!(fs::read(out.join("sub/b.txt")).unwrap(), b"bbbbb");
        assert!(out.join("empty").is_dir());
    }

    #[test]
    fn dest_existing_dir_keeps_source_name() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("a.txt");
        write(&src, b"x");
        let destdir = t.path().join("dest");
        fs::create_dir(&destdir).unwrap();
        let plan = build_plan(&src, &destdir, false).unwrap();
        execute_plan(&plan, &quiet_opts(), &no_progress()).unwrap();
        assert_eq!(fs::read(destdir.join("a.txt")).unwrap(), b"x");
    }

    #[test]
    fn dest_dir_into_existing_dir_keeps_name() {
        let t = TempDir::new().unwrap();
        let src = make_tree(&t);
        let destdir = t.path().join("backup");
        fs::create_dir(&destdir).unwrap();
        let plan = build_plan(&src, &destdir, true).unwrap();
        execute_plan(&plan, &quiet_opts(), &no_progress()).unwrap();
        assert!(destdir.join("d/a.txt").exists());
        assert!(destdir.join("d/sub/b.txt").exists());
    }

    #[test]
    fn refuses_copy_into_itself() {
        let t = TempDir::new().unwrap();
        let src = make_tree(&t);
        assert!(build_plan(&src, &src.join("sub"), true).is_err());
    }

    #[test]
    fn refuses_identical_source_and_dest() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("a.txt");
        write(&src, b"x");
        assert!(build_plan(&src, &src, false).is_err());
    }

    #[test]
    fn no_temp_files_left_behind() {
        let t = TempDir::new().unwrap();
        let src = make_tree(&t);
        let out = t.path().join("out");
        let plan = build_plan(&src, &out, true).unwrap();
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
        let plan = build_plan(&src, &dst, false).unwrap();
        // Barre "activée" : force le chemin de copie par buffer.
        let stats = execute_plan(&plan, &quiet_opts(), &CopyProgress::new(plan.total_bytes, true)).unwrap();
        assert!(stats.errors.is_empty());
        assert_eq!(fs::read(&dst).unwrap(), data);
    }

    #[cfg(unix)]
    #[test]
    fn archive_preserves_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let t = TempDir::new().unwrap();
        let src = t.path().join("x.sh");
        write(&src, b"#!/bin/sh\n");
        fs::set_permissions(&src, fs::Permissions::from_mode(0o750)).unwrap();
        let dst = t.path().join("y.sh");
        let plan = build_plan(&src, &dst, false).unwrap();
        execute_plan(&plan, &CopyOptions { archive: true, verbose: false }, &no_progress()).unwrap();
        assert_eq!(fs::metadata(&dst).unwrap().permissions().mode() & 0o777, 0o750);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_recreated() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("d");
        write(&src.join("real.txt"), b"data");
        std::os::unix::fs::symlink("real.txt", src.join("link.txt")).unwrap();
        let out = t.path().join("out");
        let plan = build_plan(&src, &out, true).unwrap();
        let stats = execute_plan(&plan, &quiet_opts(), &no_progress()).unwrap();
        assert!(stats.errors.is_empty());
        let link = out.join("link.txt");
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(fs::read_link(&link).unwrap(), PathBuf::from("real.txt"));
    }

    #[test]
    fn dest_trailing_slash_creates_dir() {
        let t = TempDir::new().unwrap();
        let src = t.path().join("a.txt");
        write(&src, b"x");
        // "newdir/" n'existe pas encore : le "/" final signale un répertoire.
        let dst = t.path().join("newdir").join("");
        let plan = build_plan(&src, &dst, false).unwrap();
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
    fn normalize_handles_dotdot() {
        assert_eq!(normalize_lexical(Path::new("/a/b/../c")), PathBuf::from("/a/c"));
        assert_eq!(normalize_lexical(Path::new("/a/./b")), PathBuf::from("/a/b"));
    }
}
