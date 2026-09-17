//! Interface graphique de `rscap`.
//!
//! Binaire distinct du `rscap` en ligne de commande : l'enregistreur doit
//! rester utilisable sur une machine sans GTK — un serveur, un conteneur de
//! CI — et une compilation sans `--features gui` n'a alors aucune liaison
//! graphique a construire.
//!
//! Il ne prend pas d'options : tout se regle dans la fenetre, et les reglages
//! s'exportent en `rscap.toml`, directement relisible par `rscap --config`.

use tracing_subscriber::prelude::*;
use tracing_subscriber::EnvFilter;

fn main() -> std::process::ExitCode {
    // Meme convention que le binaire en ligne de commande : `RSCAP_LOG`
    // pilote la verbosite, l'ecriture est non bloquante pour qu'un journal ne
    // fasse jamais attendre un thread temps reel.
    let (writer, _guard) = tracing_appender::non_blocking(std::io::stderr());
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_env("RSCAP_LOG").unwrap_or_else(|_| EnvFilter::new("rscap=warn")))
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_target(false),
        )
        .init();

    let code = rscap::gui::run();
    if code == gtk4::glib::ExitCode::SUCCESS {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    }
}
