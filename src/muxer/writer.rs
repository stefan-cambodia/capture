//! Ecriture disque : gros tampon, mesures, et remontee fidele des erreurs.
//!
//! # Pourquoi ne pas laisser ffmpeg ecrire tout seul
//!
//! Le protocole `file` de ffmpeg utilise un tampon AVIO de 32 Kio : a 35 Mb/s
//! cela fait environ 130 appels systeme par seconde. Ce n'est pas dramatique,
//! mais on perd surtout deux choses importantes :
//!
//! - **La cause exacte d'un echec.** ffmpeg rend `AVERROR(EIO)` la ou le noyau
//!   disait `ENOSPC`. Sur un enregistrement long, « disque plein » est le
//!   diagnostic le plus utile qui soit, et il doit arriver tel quel.
//! - **La mesure.** Debit reel, duree de chaque ecriture, detection d'un
//!   disque qui decroche : rien de tout cela n'est visible depuis ffmpeg.
//!
//! On passe donc par un `Write + Seek` a nous, avec un tampon de plusieurs
//! mega-octets, que ffmpeg pilote via son interface d'E/S personnalisee.

use std::fs::File;
use std::io::{self, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::error::{RecorderError, Result};
use crate::performance::Stats;
use crate::timing::monotonic_ns;

/// Erreur d'E/S conservee pour etre rendue telle quelle a l'appelant.
///
/// ffmpeg ne transporte qu'un code numerique ; on garde ici l'erreur systeme
/// complete, que le muxer consulte des qu'une ecriture echoue.
pub type SharedIoError = Arc<Mutex<Option<io::Error>>>;

/// Destination fichier instrumentee.
pub struct DiskWriter {
    file: File,
    path: PathBuf,
    stats: Arc<Stats>,
    error: SharedIoError,
    /// Duree au-dela de laquelle une ecriture est consideree comme un
    /// decrochage disque.
    lag_threshold_ns: i64,
}

impl DiskWriter {
    /// Cree le fichier et un descripteur jumeau pour la synchronisation
    /// finale.
    ///
    /// Le jumeau est necessaire parce que ffmpeg prend possession du writer :
    /// sans lui, impossible d'appeler `fsync` apres la fermeture du muxer.
    pub fn create(
        path: &Path,
        stats: Arc<Stats>,
        fps: u32,
    ) -> Result<(Self, File, SharedIoError)> {
        let file = File::create(path).map_err(|e| RecorderError::from_write(path, e))?;
        let twin = file
            .try_clone()
            .map_err(|e| RecorderError::from_write(path, e))?;
        let error: SharedIoError = Arc::new(Mutex::new(None));
        let writer = Self {
            file,
            path: path.to_path_buf(),
            stats,
            error: Arc::clone(&error),
            // Une ecriture ne devrait jamais couter plus qu'une image.
            lag_threshold_ns: 1_000_000_000 / fps.max(1) as i64,
        };
        Ok((writer, twin, error))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn record(&self, err: &io::Error) {
        // On ne garde que la premiere erreur : c'est la cause, les suivantes
        // n'en sont que les consequences.
        let mut slot = self.error.lock();
        if slot.is_none() {
            // `io::Error` n'est pas `Clone` ; on reconstruit une erreur
            // equivalente qui conserve le code systeme.
            *slot = Some(match err.raw_os_error() {
                Some(code) => io::Error::from_raw_os_error(code),
                None => io::Error::new(err.kind(), err.to_string()),
            });
        }
    }
}

impl Write for DiskWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let start = monotonic_ns();
        let result = self.file.write(buf);
        let elapsed = monotonic_ns() - start;

        match &result {
            Ok(n) => {
                self.stats.bytes_written(*n as u64);
                self.stats.record_disk_write_us((elapsed / 1000).max(0) as u64);
                if elapsed > self.lag_threshold_ns {
                    self.stats.disk_write_lag(1);
                }
            }
            Err(e) => self.record(e),
        }
        result
    }

    fn flush(&mut self) -> io::Result<()> {
        let result = self.file.flush();
        if let Err(e) = &result {
            self.record(e);
        }
        result
    }
}

impl Seek for DiskWriter {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        // Le muxer MP4 revient en arriere a la fermeture pour ecrire la table
        // des echantillons : le flux doit donc etre positionnable.
        let result = self.file.seek(pos);
        if let Err(e) = &result {
            self.record(e);
        }
        result
    }
}

/// Transforme l'erreur conservee en erreur de projet, en distinguant le
/// disque plein.
pub fn take_io_error(shared: &SharedIoError, path: &Path) -> Option<RecorderError> {
    let err = shared.lock().take()?;
    Some(RecorderError::from_write(path, err))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_written_are_counted() {
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let path = dir.path().join("a.bin");
        let stats = Stats::shared();
        let (mut w, _twin, _err) = DiskWriter::create(&path, Arc::clone(&stats), 60).expect("creation");
        w.write_all(&[0u8; 4096]).expect("ecriture");
        w.flush().expect("vidange");
        assert_eq!(stats.snapshot().bytes_written, 4096);
        assert!(stats.snapshot().disk_latency.count >= 1);
    }

    #[test]
    fn the_writer_is_seekable_for_the_mp4_muxer() {
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let path = dir.path().join("b.bin");
        let stats = Stats::shared();
        let (mut w, _twin, _err) = DiskWriter::create(&path, stats, 60).expect("creation");
        w.write_all(b"0123456789").expect("ecriture");
        assert_eq!(w.seek(SeekFrom::Start(0)).expect("retour"), 0);
        w.write_all(b"ABCDE").expect("reecriture");
        w.flush().expect("vidange");
        let content = std::fs::read(&path).expect("relecture");
        assert_eq!(&content, b"ABCDE56789");
    }

    #[test]
    fn a_write_error_is_kept_with_its_os_code() {
        let shared: SharedIoError = Arc::new(Mutex::new(Some(io::Error::from_raw_os_error(
            libc::ENOSPC,
        ))));
        let err = take_io_error(&shared, Path::new("/tmp/x.mp4"));
        assert!(matches!(err, Some(RecorderError::DiskFull { .. })));
        // L'erreur n'est rendue qu'une fois.
        assert!(take_io_error(&shared, Path::new("/tmp/x.mp4")).is_none());
    }

    #[test]
    fn a_generic_write_error_is_not_reported_as_a_full_disk() {
        let shared: SharedIoError = Arc::new(Mutex::new(Some(io::Error::from_raw_os_error(
            libc::EIO,
        ))));
        let err = take_io_error(&shared, Path::new("/tmp/x.mp4"));
        assert!(matches!(err, Some(RecorderError::DiskWrite { .. })));
    }

    #[test]
    fn creating_in_a_missing_directory_fails_cleanly() {
        let stats = Stats::shared();
        let err = DiskWriter::create(
            Path::new("/nonexistent-dir-rscap/out.mp4"),
            stats,
            60,
        );
        assert!(err.is_err());
    }
}
