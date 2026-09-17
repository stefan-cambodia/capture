//! Base temporelle unique du pipeline.
//!
//! # Principe
//!
//! Tout le pipeline partage **une seule horloge** : `CLOCK_MONOTONIC`, lue en
//! nanosecondes. Elle ne recule jamais, n'est pas affectee par les changements
//! d'heure systeme, et c'est aussi la base des timestamps que PipeWire place
//! dans `spa_meta_header.pts`. On peut donc comparer directement un timestamp
//! fourni par le compositeur et une lecture locale, sans conversion ni
//! estimation.
//!
//! # Ce que l'on ne fait pas
//!
//! - Pas de `thread::sleep(16ms)` comme mecanisme de cadencement : l'erreur
//!   s'accumulerait (chaque reveil est en retard de quelques centaines de
//!   microsecondes, soit plusieurs secondes de derive par heure).
//! - Pas de PTS derive d'un simple compteur de frames cote video, ni d'un
//!   simple compteur de paquets cote audio.
//!
//! A la place : chaque slot CFR `k` possede une **echeance absolue**
//! `origine + k * periode`, recalculee depuis l'origine a chaque tour. Une
//! iteration en retard ne decale pas les suivantes.

use std::time::Duration;

/// Nanosecondes par seconde.
pub const NS_PER_SEC: i64 = 1_000_000_000;

/// Lecture de l'horloge monotone, en nanosecondes.
///
/// Sur Linux on appelle directement `clock_gettime(CLOCK_MONOTONIC)` pour
/// partager exactement la base de temps de PipeWire et de PulseAudio.
#[cfg(unix)]
#[inline]
pub fn monotonic_ns() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` est une struct valide et initialisee ; `clock_gettime` ne
    // fait qu'y ecrire. CLOCK_MONOTONIC est toujours disponible sous Linux.
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    if rc != 0 {
        // Ne peut arriver qu'avec un clock_id invalide ; on degrade plutot que
        // de paniquer dans un thread temps reel.
        return 0;
    }
    // Les casts sont redondants sur x86_64 (les deux champs sont deja des
    // i64) mais pas sur les cibles 32 bits, ou `tv_nsec` est un `c_long`.
    #[allow(clippy::unnecessary_cast)]
    {
        ts.tv_sec as i64 * NS_PER_SEC + ts.tv_nsec as i64
    }
}

/// Repli portable : `Instant` est monotone sur toutes les plateformes cibles.
#[cfg(not(unix))]
#[inline]
pub fn monotonic_ns() -> i64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    let epoch = EPOCH.get_or_init(Instant::now);
    epoch.elapsed().as_nanos() as i64
}

/// Convertit des nanosecondes (>= 0) en `Duration`, en saturant a zero.
#[inline]
pub fn ns_to_duration(ns: i64) -> Duration {
    if ns <= 0 {
        Duration::ZERO
    } else {
        Duration::from_nanos(ns as u64)
    }
}

/// Division entiere avec arrondi au plus proche, valable pour `n` negatif.
#[inline]
fn div_round(n: i64, d: i64) -> i64 {
    debug_assert!(d > 0);
    if n >= 0 {
        (n + d / 2) / d
    } else {
        -((-n + d / 2) / d)
    }
}

// ---------------------------------------------------------------------------
// Pacer CFR
// ---------------------------------------------------------------------------

/// Sort de `CfrPacer::ingest`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ingest {
    /// La frame occupe un nouveau slot : elle devient la frame en attente.
    Accepted { slot: i64 },
    /// Une frame plus recente tombe dans le meme slot qu'une frame deja en
    /// attente : l'ancienne est ecrasee (le compositeur a produit plus vite
    /// que le framerate cible).
    Superseded { slot: i64 },
    /// La frame appartient a un slot deja emis : elle arrive trop tard et est
    /// jetee. C'est compte explicitement, jamais masque.
    Late { slot: i64 },
}

/// Ce que la boucle d'encodage doit faire a l'echeance d'un slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Emit {
    /// Rien a faire : l'echeance du prochain slot n'est pas atteinte.
    Wait,
    /// Emettre le slot `slot`. `duplicated` indique qu'aucune frame neuve
    /// n'etait disponible et que l'image precedente est repetee.
    Slot { slot: i64, duplicated: bool },
    /// Le pipeline est trop en retard (encodeur sature) : on saute des slots
    /// pour rester en temps reel plutot que d'accumuler une latence infinie.
    /// Les slots sautes sont comptes comme frames perdues.
    Skip { from: i64, to: i64 },
}

/// Cadenceur a framerate constant.
///
/// Il traduit des timestamps de capture arbitraires (jitter, avance, retard)
/// vers une grille reguliere `1/fps`. Le PTS final vaut l'index du slot dans
/// la base de temps `1/fps` : il est donc exact par construction et ne derive
/// jamais, quelle que soit l'irregularite de la capture.
#[derive(Debug)]
pub struct CfrPacer {
    period_ns: i64,
    /// Marge ajoutee a l'echeance pour absorber le jitter de capture.
    lookahead_ns: i64,
    /// Instant (horloge monotone) du slot 0. `None` tant qu'aucune frame n'est
    /// arrivee : on ne demarre pas la grille sur une capture vide.
    origin_ns: Option<i64>,
    /// Prochain slot a emettre.
    next_slot: i64,
    /// Au-dela de ce retard, on saute au lieu de rattraper.
    max_behind_slots: i64,
    /// Nombre max de slots emis d'affilee, pour garder la boucle reactive.
    max_burst: i64,
}

impl CfrPacer {
    /// `fps` doit etre > 0. `lookahead` absorbe le jitter : une frame qui
    /// arrive jusqu'a `lookahead` apres l'echeance theorique est encore prise
    /// en compte pour son slot.
    pub fn new(fps: u32, lookahead: Duration) -> Self {
        let fps = fps.max(1) as i64;
        let period_ns = NS_PER_SEC / fps;
        Self {
            period_ns,
            lookahead_ns: lookahead.as_nanos() as i64,
            origin_ns: None,
            next_slot: 0,
            // 1 seconde de retard = l'encodeur ne tient pas la cadence.
            max_behind_slots: fps,
            max_burst: 8,
        }
    }

    #[inline]
    pub fn period_ns(&self) -> i64 {
        self.period_ns
    }

    #[inline]
    pub fn next_slot(&self) -> i64 {
        self.next_slot
    }

    #[inline]
    pub fn started(&self) -> bool {
        self.origin_ns.is_some()
    }

    /// Index du slot auquel appartient une capture faite a `pts_ns`.
    #[inline]
    pub fn slot_of(&self, pts_ns: i64) -> Option<i64> {
        self.origin_ns
            .map(|origin| div_round(pts_ns - origin, self.period_ns))
    }

    /// Echeance absolue (horloge monotone) a laquelle `next_slot` doit partir.
    #[inline]
    pub fn next_deadline_ns(&self) -> Option<i64> {
        self.origin_ns
            .map(|origin| origin + self.next_slot * self.period_ns + self.lookahead_ns)
    }

    /// Duree d'attente restante avant la prochaine echeance.
    #[inline]
    pub fn time_to_deadline(&self, now_ns: i64) -> Option<Duration> {
        self.next_deadline_ns()
            .map(|deadline| ns_to_duration(deadline - now_ns))
    }

    /// Enregistre une frame capturee. Fixe l'origine de la grille sur la
    /// premiere frame recue.
    pub fn ingest(&mut self, pts_ns: i64, pending: bool) -> Ingest {
        let origin = match self.origin_ns {
            Some(o) => o,
            None => {
                self.origin_ns = Some(pts_ns);
                self.next_slot = 0;
                return Ingest::Accepted { slot: 0 };
            }
        };
        let slot = div_round(pts_ns - origin, self.period_ns);
        if slot < self.next_slot {
            Ingest::Late { slot }
        } else if pending {
            Ingest::Superseded { slot }
        } else {
            Ingest::Accepted { slot }
        }
    }

    /// A appeler quand l'echeance est atteinte (ou depassee).
    ///
    /// `has_pending` indique qu'une frame neuve attend pour un slot <=
    /// `next_slot`. `has_previous` indique qu'une image precedente existe et
    /// peut etre repetee.
    pub fn emit(&mut self, now_ns: i64, has_pending: bool, has_previous: bool) -> Emit {
        let Some(deadline) = self.next_deadline_ns() else {
            return Emit::Wait;
        };
        if now_ns < deadline {
            return Emit::Wait;
        }
        // Rien a emettre du tout (aucune image encore disponible).
        if !has_pending && !has_previous {
            return Emit::Wait;
        }

        // Combien de slots sont echus ?
        let Some(origin) = self.origin_ns else {
            return Emit::Wait;
        };
        let current = (now_ns - self.lookahead_ns - origin).div_euclid(self.period_ns);
        let behind = current - self.next_slot;

        if behind > self.max_behind_slots {
            // Surcharge prolongee : on abandonne les slots en retard. Les PTS
            // restent exacts (index de slot), donc l'audio reste synchrone ;
            // la video montre une saccade, comptee et signalee.
            let from = self.next_slot;
            let to = current;
            self.next_slot = current;
            return Emit::Skip { from, to };
        }

        let slot = self.next_slot;
        self.next_slot += 1;
        Emit::Slot {
            slot,
            duplicated: !has_pending,
        }
    }

    /// Nombre de slots pouvant etre emis d'affilee avant de re-sonder la file.
    #[inline]
    pub fn max_burst(&self) -> i64 {
        self.max_burst
    }

    /// PTS media (base `1/fps`) d'un slot. Exact par construction.
    #[inline]
    pub fn slot_pts(&self, slot: i64) -> i64 {
        slot
    }
}

// ---------------------------------------------------------------------------
// Horloge audio
// ---------------------------------------------------------------------------

/// Horloge audio asservie a l'horloge monotone.
///
/// Le PTS audio est derive du **compteur d'echantillons** (continu, sans
/// jitter), mais sa *cadence* est comparee en permanence a l'horloge monotone.
/// L'ecart mesure (`drift`) sert a piloter une compensation de reechantillonnage :
/// l'horloge du peripherique audio ne peut donc pas s'eloigner de la video,
/// meme sur plusieurs heures.
#[derive(Debug)]
pub struct AudioClock {
    rate: i64,
    /// Instant monotone correspondant a l'echantillon 0.
    anchor_ns: i64,
    /// Echantillons deja pousses vers l'encodeur.
    samples: i64,
}

impl AudioClock {
    pub fn new(rate: u32, anchor_ns: i64) -> Self {
        Self {
            rate: rate.max(1) as i64,
            anchor_ns,
            samples: 0,
        }
    }

    #[inline]
    pub fn samples(&self) -> i64 {
        self.samples
    }

    #[inline]
    pub fn advance(&mut self, samples: i64) {
        self.samples += samples;
    }

    /// PTS media du prochain echantillon, en base `1/rate`.
    #[inline]
    pub fn next_pts(&self) -> i64 {
        self.samples
    }

    /// Instant monotone theorique du prochain echantillon.
    #[inline]
    pub fn media_time_ns(&self) -> i64 {
        self.anchor_ns + self.samples * NS_PER_SEC / self.rate
    }

    /// Derive en echantillons : positif = l'audio est **en retard** sur
    /// l'horloge murale (il manque des echantillons), negatif = en avance.
    #[inline]
    pub fn drift_samples(&self, observed_ns: i64) -> i64 {
        let expected = (observed_ns - self.anchor_ns) * self.rate / NS_PER_SEC;
        expected - self.samples
    }

    /// Derive en nanosecondes, pour l'affichage.
    #[inline]
    pub fn drift_ns(&self, observed_ns: i64) -> i64 {
        self.drift_samples(observed_ns) * NS_PER_SEC / self.rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FPS: u32 = 60;
    fn pacer() -> CfrPacer {
        CfrPacer::new(FPS, Duration::from_millis(0))
    }
    const P: i64 = NS_PER_SEC / 60;

    #[test]
    fn monotonic_never_goes_backwards() {
        let a = monotonic_ns();
        let b = monotonic_ns();
        assert!(b >= a, "{b} < {a}");
        assert!(a > 0);
    }

    #[test]
    fn div_round_handles_negatives() {
        assert_eq!(div_round(10, 4), 3);
        assert_eq!(div_round(-10, 4), -3);
        assert_eq!(div_round(2, 4), 1);
        assert_eq!(div_round(1, 4), 0);
    }

    #[test]
    fn first_frame_defines_origin_slot_zero() {
        let mut p = pacer();
        // Un timestamp arbitraire (pas zero) : l'origine s'aligne dessus.
        assert_eq!(p.ingest(123_456_789, false), Ingest::Accepted { slot: 0 });
        assert_eq!(p.next_deadline_ns(), Some(123_456_789));
    }

    #[test]
    fn jitter_is_absorbed_into_the_correct_slot() {
        let mut p = pacer();
        p.ingest(0, false);
        // +/- 40% d'une periode de jitter doit rester dans le bon slot.
        for k in 1..50i64 {
            let jitter = if k % 2 == 0 { P * 2 / 5 } else { -P * 2 / 5 };
            let got = p.ingest(k * P + jitter, false);
            assert_eq!(got, Ingest::Accepted { slot: k }, "slot {k}");
        }
    }

    #[test]
    fn early_duplicate_in_same_slot_is_superseded_not_dropped() {
        let mut p = pacer();
        p.ingest(0, false);
        // Deux captures dans le meme slot : la seconde ecrase la premiere.
        assert_eq!(p.ingest(P / 10, true), Ingest::Superseded { slot: 0 });
    }

    #[test]
    fn frame_for_an_already_emitted_slot_is_late() {
        let mut p = pacer();
        p.ingest(0, false);
        assert_eq!(p.emit(0, true, false), Emit::Slot { slot: 0, duplicated: false });
        // Une frame retardataire pour le slot 0 arrive apres son emission.
        assert_eq!(p.ingest(P / 10, false), Ingest::Late { slot: 0 });
    }

    #[test]
    fn static_screen_duplicates_to_keep_cfr() {
        let mut p = pacer();
        p.ingest(0, false);
        assert_eq!(p.emit(0, true, false), Emit::Slot { slot: 0, duplicated: false });
        // Plus aucune frame : les slots suivants repetent l'image precedente.
        for k in 1..10i64 {
            let now = k * P;
            assert_eq!(
                p.emit(now, false, true),
                Emit::Slot { slot: k, duplicated: true },
                "slot {k}"
            );
        }
    }

    #[test]
    fn no_emission_before_the_deadline() {
        let mut p = pacer();
        p.ingest(0, false);
        p.emit(0, true, false);
        // A mi-periode, le slot 1 n'est pas encore du.
        assert_eq!(p.emit(P / 2, true, true), Emit::Wait);
    }

    #[test]
    fn nothing_is_emitted_before_the_first_frame() {
        let mut p = pacer();
        assert_eq!(p.emit(monotonic_ns(), false, false), Emit::Wait);
        assert_eq!(p.next_deadline_ns(), None);
    }

    #[test]
    fn sustained_overload_skips_instead_of_accumulating_latency() {
        let mut p = pacer();
        p.ingest(0, false);
        p.emit(0, true, false);
        // 3 secondes se sont ecoulees sans que l'on ait pu encoder : au-dela
        // d'une seconde de retard on saute pour rester en temps reel.
        let now = 3 * NS_PER_SEC;
        match p.emit(now, true, true) {
            Emit::Skip { from, to } => {
                assert_eq!(from, 1);
                assert_eq!(to, 180);
            }
            other => panic!("attendu Skip, obtenu {other:?}"),
        }
        // Et on repart exactement au slot courant : pas de derive residuelle.
        assert_eq!(p.next_slot(), 180);
    }

    #[test]
    fn deadlines_do_not_drift_over_an_hour() {
        // Verifie la propriete essentielle : l'echeance du slot k vaut
        // exactement origine + k*periode, sans accumulation d'erreur.
        let mut p = pacer();
        p.ingest(0, false);
        let one_hour_slots = 60 * 60 * 60;
        for _ in 0..one_hour_slots {
            let d = p.next_deadline_ns().unwrap_or_default();
            assert_eq!(d, p.next_slot() * P);
            p.emit(d, true, true);
        }
        assert_eq!(p.next_slot(), one_hour_slots);
    }

    #[test]
    fn audio_clock_pts_is_sample_exact() {
        let mut c = AudioClock::new(48_000, 1_000);
        c.advance(48_000);
        assert_eq!(c.next_pts(), 48_000);
        assert_eq!(c.media_time_ns(), 1_000 + NS_PER_SEC);
    }

    #[test]
    fn audio_drift_sign_is_positive_when_audio_lags() {
        let c = AudioClock::new(48_000, 0);
        // 1 seconde de temps murale s'est ecoulee, 0 echantillon emis :
        // l'audio est en retard de 48000 echantillons.
        assert_eq!(c.drift_samples(NS_PER_SEC), 48_000);
        assert_eq!(c.drift_ns(NS_PER_SEC), NS_PER_SEC);
    }

    #[test]
    fn audio_drift_is_zero_when_locked() {
        let mut c = AudioClock::new(48_000, 0);
        c.advance(48_000);
        assert_eq!(c.drift_samples(NS_PER_SEC), 0);
    }
}
