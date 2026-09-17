//! Negociation de la session de capture via `xdg-desktop-portal`.
//!
//! Sous Wayland, aucune application ne peut lire le contenu de l'ecran sans
//! passer par le portail : c'est le compositeur qui decide, apres consentement
//! explicite de l'utilisateur. Le portail nous rend un identifiant de noeud
//! PipeWire et un descripteur de fichier vers le demon PipeWire ; toute la
//! capture se fait ensuite par ce canal.
//!
//! # Jeton de restauration
//!
//! Le premier lancement affiche une boite de dialogue de selection d'ecran.
//! On demande `PersistMode::ExplicitlyRevoked` et on conserve le jeton rendu
//! par le portail dans le repertoire d'etat de l'utilisateur : les lancements
//! suivants reprennent la meme source sans reafficher la boite de dialogue.
//! C'est ce qui rend l'outil utilisable dans un script ou pour un benchmark
//! repete.

use std::os::fd::OwnedFd;
use std::path::PathBuf;

use ashpd::desktop::screencast::{
    CursorMode, Screencast, SelectSourcesOptions, SourceType, StartCastOptions,
};
use ashpd::desktop::{PersistMode, Session};
use ashpd::enumflags2::BitFlags;
use tokio::runtime::Runtime;

use crate::error::{RecorderError, Result};

/// Resultat de la negociation.
pub struct PortalSession {
    /// Doit rester vivant : la fermeture du runtime fermerait la connexion
    /// D-Bus, donc la session, donc le flux PipeWire.
    _runtime: Runtime,
    session: Option<Session<Screencast>>,
    /// Identifiant du noeud PipeWire a consommer.
    pub node_id: u32,
    /// Descripteur vers le demon PipeWire, deja authentifie par le portail.
    pub fd: OwnedFd,
    /// Taille annoncee par le portail (logique, pas forcement en pixels).
    pub size: Option<(i32, i32)>,
    /// Identifiant lisible de la source choisie.
    pub source_id: String,
}

impl PortalSession {
    /// Ouvre une session de capture d'un moniteur.
    ///
    /// `embed_cursor` inclut le pointeur dans les images capturees.
    pub fn open(embed_cursor: bool) -> Result<Self> {
        // Un runtime multi-thread : zbus doit continuer a servir la connexion
        // D-Bus pendant que le thread appelant est bloque sur la boucle PipeWire.
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .thread_name("rscap-portal")
            .build()
            .map_err(|e| {
                RecorderError::ScreenCapture(format!("runtime D-Bus indisponible : {e}"))
            })?;

        let restore_token = read_restore_token();

        let negotiated = runtime.block_on(async move {
            let proxy = Screencast::new()
                .await
                .map_err(|e| portal_err("connexion au portail", e))?;

            let session = proxy
                .create_session(Default::default())
                .await
                .map_err(|e| portal_err("creation de la session", e))?;

            let cursor_mode = if embed_cursor {
                CursorMode::Embedded
            } else {
                CursorMode::Hidden
            };
            // Certains compositeurs n'offrent pas tous les modes de curseur ;
            // on retombe sur `Hidden`, toujours disponible, plutot que d'echouer.
            let cursor_mode = match proxy.available_cursor_modes().await {
                Ok(modes) if modes.contains(cursor_mode) => cursor_mode,
                Ok(_) => CursorMode::Hidden,
                Err(_) => cursor_mode,
            };

            let mut options = SelectSourcesOptions::default()
                .set_cursor_mode(cursor_mode)
                .set_sources(BitFlags::from(SourceType::Monitor))
                .set_multiple(false)
                .set_persist_mode(PersistMode::ExplicitlyRevoked);
            if let Some(token) = restore_token.as_deref() {
                options = options.set_restore_token(token);
            }

            proxy
                .select_sources(&session, options)
                .await
                .map_err(|e| portal_err("selection de la source", e))?;

            let streams = proxy
                .start(&session, None, StartCastOptions::default())
                .await
                .map_err(|e| portal_err("demarrage de la session", e))?
                .response()
                .map_err(|e| portal_err("reponse de demarrage", e))?;

            if let Some(token) = streams.restore_token() {
                store_restore_token(token);
            }

            let stream = streams.streams().first().cloned().ok_or_else(|| {
                RecorderError::ScreenCapture(
                    "le portail n'a retourne aucun flux : aucune source selectionnee".into(),
                )
            })?;

            let fd = proxy
                .open_pipe_wire_remote(&session, Default::default())
                .await
                .map_err(|e| portal_err("ouverture du canal PipeWire", e))?;

            Ok::<_, RecorderError>((
                session,
                stream.pipe_wire_node_id(),
                fd,
                stream.size(),
                stream
                    .id()
                    .map(str::to_owned)
                    .unwrap_or_else(|| "monitor".to_owned()),
            ))
        })?;

        let (session, node_id, fd, size, source_id) = negotiated;
        Ok(Self {
            _runtime: runtime,
            session: Some(session),
            node_id,
            fd,
            size,
            source_id,
        })
    }
}

impl Drop for PortalSession {
    fn drop(&mut self) {
        // Fermer la session rend la ressource au compositeur et fait
        // disparaitre l'indicateur "partage d'ecran en cours".
        if let Some(session) = self.session.take() {
            let _ = self._runtime.block_on(session.close());
        }
    }
}

fn portal_err(stage: &str, e: ashpd::Error) -> RecorderError {
    // Un refus de l'utilisateur n'est pas une panne : on le dit clairement.
    let msg = match &e {
        ashpd::Error::Response(_) => {
            format!("{stage} : requete annulee ou refusee dans la boite de dialogue du portail")
        }
        other => format!("{stage} : {other}"),
    };
    RecorderError::ScreenCapture(msg)
}

fn token_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(base.join("rscap").join("screencast.token"))
}

fn read_restore_token() -> Option<String> {
    let path = token_path()?;
    let token = std::fs::read_to_string(path).ok()?;
    let token = token.trim().to_owned();
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

fn store_restore_token(token: &str) {
    let Some(path) = token_path() else { return };
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    // Un echec d'ecriture n'est pas fatal : on reaffichera la boite de
    // dialogue au prochain lancement.
    if let Err(e) = std::fs::write(&path, token) {
        tracing::debug!(error = %e, path = %path.display(), "jeton de restauration non conserve");
    }
}

/// Efface le jeton : le prochain lancement redemandera le choix de l'ecran.
pub fn forget_restore_token() -> Result<()> {
    if let Some(path) = token_path() {
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(RecorderError::from_write(path, e)),
        }
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_path_is_under_the_state_directory() {
        let p = token_path();
        assert!(p.is_some());
        if let Some(p) = p {
            assert!(p.ends_with("rscap/screencast.token"), "{}", p.display());
        }
    }

    #[test]
    fn forgetting_an_absent_token_is_not_an_error() {
        // Ne doit jamais echouer quand le fichier n'existe pas.
        assert!(forget_restore_token().is_ok());
    }
}
