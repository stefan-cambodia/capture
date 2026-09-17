//! Barriere de depart commune aux flux.
//!
//! # Le probleme
//!
//! La capture video et la capture audio ne demarrent jamais au meme instant :
//! le portail negocie pendant que le peripherique audio s'amorce, et l'ecart
//! atteint couramment 100 a 300 ms. Si l'on posait naivement le PTS 0 de
//! chaque flux sur sa propre premiere donnee, le fichier commencerait avec
//! l'audio et la video decales de cet ecart — de facon permanente.
//!
//! # La solution
//!
//! Chaque flux annonce l'horodatage de sa premiere donnee. Le dernier arrive
//! calcule `t0 = max(annonces)` : l'instant a partir duquel **tous** les flux
//! ont des donnees. Chacun jette (video) ou complete (audio) ce qui precede.
//! Les deux flux commencent donc exactement au meme instant, sans silence
//! artificiel ni image figee au debut.
//!
//! Un flux qui echoue appelle [`StartSync::withdraw`] : les autres ne restent
//! pas bloques a l'attendre.

use std::time::Duration;

use parking_lot::{Condvar, Mutex};

/// Point de rendez-vous des flux au demarrage.
#[derive(Debug)]
pub struct StartSync {
    state: Mutex<State>,
    ready: Condvar,
    participants: usize,
}

#[derive(Debug, Default)]
struct State {
    proposals: Vec<i64>,
    withdrawn: usize,
    resolved: Option<i64>,
}

impl StartSync {
    pub fn new(participants: usize) -> Self {
        Self {
            state: Mutex::new(State::default()),
            ready: Condvar::new(),
            participants: participants.max(1),
        }
    }

    /// Annonce l'horodatage de la premiere donnee et attend l'origine commune.
    ///
    /// En cas d'expiration du delai, on resout avec ce qui est connu plutot
    /// que de bloquer l'enregistrement : mieux vaut un flux legerement decale
    /// qu'un enregistrement qui ne demarre jamais.
    pub fn propose(&self, first_ns: i64, timeout: Duration) -> i64 {
        let mut state = self.state.lock();
        if let Some(t0) = state.resolved {
            return t0;
        }
        state.proposals.push(first_ns);
        if state.proposals.len() + state.withdrawn >= self.participants {
            let t0 = state.proposals.iter().copied().max().unwrap_or(first_ns);
            state.resolved = Some(t0);
            self.ready.notify_all();
            return t0;
        }

        let timed_out = self
            .ready
            .wait_for(&mut state, timeout)
            .timed_out();
        match state.resolved {
            Some(t0) => t0,
            None => {
                let t0 = state.proposals.iter().copied().max().unwrap_or(first_ns);
                state.resolved = Some(t0);
                if timed_out {
                    tracing::warn!(
                        "un flux n'a pas annonce son depart a temps : origine fixee sans lui"
                    );
                }
                self.ready.notify_all();
                t0
            }
        }
    }

    /// Signale qu'un flux ne participera pas (echec d'ouverture, desactive).
    pub fn withdraw(&self) {
        let mut state = self.state.lock();
        state.withdrawn += 1;
        if state.resolved.is_none()
            && state.proposals.len() + state.withdrawn >= self.participants
            && !state.proposals.is_empty()
        {
            state.resolved = state.proposals.iter().copied().max();
            self.ready.notify_all();
        } else {
            self.ready.notify_all();
        }
    }

    /// Origine deja resolue, s'il y en a une.
    pub fn resolved(&self) -> Option<i64> {
        self.state.lock().resolved
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    const SHORT: Duration = Duration::from_secs(2);

    #[test]
    fn a_single_stream_resolves_immediately() {
        let s = StartSync::new(1);
        assert_eq!(s.propose(1_000, SHORT), 1_000);
        assert_eq!(s.resolved(), Some(1_000));
    }

    #[test]
    fn the_common_origin_is_the_latest_first_sample() {
        let sync = Arc::new(StartSync::new(2));
        let a = Arc::clone(&sync);
        let h = std::thread::spawn(move || a.propose(5_000, SHORT));
        // La video demarre plus tot que l'audio.
        let video = sync.propose(1_000, SHORT);
        let audio = h.join().expect("thread");
        // Les deux flux partent du plus tardif : personne n'a de trou.
        assert_eq!(video, 5_000);
        assert_eq!(audio, 5_000);
    }

    #[test]
    fn a_late_proposal_gets_the_already_resolved_origin() {
        let sync = StartSync::new(2);
        sync.withdraw();
        let first = sync.propose(7_000, SHORT);
        assert_eq!(first, 7_000);
        // Un retardataire recoit la meme origine, sans la modifier.
        assert_eq!(sync.propose(9_000, SHORT), 7_000);
    }

    #[test]
    fn a_failed_stream_does_not_block_the_others() {
        let sync = Arc::new(StartSync::new(2));
        let a = Arc::clone(&sync);
        let h = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            a.withdraw();
        });
        let t0 = sync.propose(3_000, Duration::from_secs(5));
        h.join().expect("thread");
        assert_eq!(t0, 3_000);
    }

    #[test]
    fn a_silent_stream_does_not_hang_the_recording_forever() {
        let sync = StartSync::new(2);
        let started = std::time::Instant::now();
        // Personne d'autre n'annonce : on resout apres expiration.
        let t0 = sync.propose(4_000, Duration::from_millis(100));
        assert_eq!(t0, 4_000);
        assert!(started.elapsed() < Duration::from_secs(1));
    }
}
