# rscap

Enregistreur d'écran temps réel en Rust : écran principal en plein écran, son
système synchronisé, encodage matériel, **30 FPS minimum et 60 FPS quand la
machine le permet**.

Architecture pipeline multithread : capture, encodage et écriture disque sont
entièrement découplés par des files bornées. Le thread de capture n'attend
jamais l'encodeur, ni l'audio, ni le muxer, ni le disque.

→ **[docs/PIPELINE.md](docs/PIPELINE.md)** détaille l'architecture, la
synchronisation A/V et les choix de performance.

---

## État

| | |
|---|---|
| **Linux** | implémenté, compilé, testé et mesuré |
| **Interface graphique** | GTK 4 / libadwaita, optionnelle (`--features gui`) |
| **Windows** | non implémenté — voir [Portage](#portage) |
| **macOS** | non implémenté |

Mesuré sur Intel Core Ultra 7 155H + Arc iGPU, 2560×1440 à 60 FPS, encodage
VAAPI H.264 :

```
FPS encodage     : 59.96 / 60      images perdues : 0
CPU              : 0.84 cœur (3.8 % de la machine)
Latence encodage : 4.3 ms (p99 5.5 ms) pour un budget de 16.7 ms
Dérive A/V       : −0.24 ms sur 20 s
```

---

## Prérequis

### Système

| | |
|---|---|
| Rust | 1.82 ou plus récent |
| FFmpeg | bibliothèques de développement, version 6 à 9 |
| PipeWire | 0.3 ou plus récent, avec `xdg-desktop-portal` |
| PulseAudio | bibliothèque cliente (fournie par `pipewire-pulse` sur une machine moderne) |
| clang | requis par `bindgen` pour les liaisons FFmpeg |
| GTK / libadwaita | **seulement pour l'interface graphique** : GTK 4.10 et libadwaita 1.5 au minimum |

Une session **Wayland ou X11** avec un portail de bureau actif
(`xdg-desktop-portal-gnome`, `-kde`, `-wlr`, `-hyprland`…).

### Installation des dépendances

**Arch / CachyOS / Manjaro**
```sh
sudo pacman -S --needed rust ffmpeg pipewire libpulse clang pkgconf \
                        xdg-desktop-portal
# pour l'interface graphique uniquement :
sudo pacman -S --needed gtk4 libadwaita
# puis le backend correspondant à votre bureau :
sudo pacman -S xdg-desktop-portal-kde     # Plasma
sudo pacman -S xdg-desktop-portal-gnome   # GNOME
sudo pacman -S xdg-desktop-portal-wlr     # Sway, river, …
```

**Debian / Ubuntu**
```sh
sudo apt install build-essential pkg-config clang \
     libavcodec-dev libavformat-dev libavutil-dev libavfilter-dev \
     libswscale-dev libswresample-dev \
     libpipewire-0.3-dev libspa-0.2-dev libpulse-dev \
     xdg-desktop-portal xdg-desktop-portal-gtk
# pour l'interface graphique uniquement :
sudo apt install libgtk-4-dev libadwaita-1-dev
```

**Fedora**
```sh
sudo dnf install clang pkgconf-pkg-config \
     ffmpeg-devel pipewire-devel pulseaudio-libs-devel \
     xdg-desktop-portal
# pour l'interface graphique uniquement :
sudo dnf install gtk4-devel libadwaita-devel
```

### Accélération matérielle

| GPU | Paquet à installer | Encodeurs obtenus |
|---|---|---|
| **Intel** | `intel-media-driver` (Gen9+) ou `libva-intel-driver` | `h264_vaapi`, `hevc_vaapi`, `av1_vaapi` |
| **Intel (QSV)** | `vpl-gpu-rt` / `intel-media-sdk` | `h264_qsv`, `hevc_qsv`, `av1_qsv` |
| **AMD** | `libva-mesa-driver` (VAAPI) ou `amdvlk` + `amf-amdgpu-pro` (AMF) | `h264_vaapi`, `h264_amf` |
| **NVIDIA** | pilote propriétaire ≥ 520 | `h264_nvenc`, `hevc_nvenc`, `av1_nvenc` |

L'utilisateur doit appartenir au groupe `video` ou `render` pour accéder à
`/dev/dri/renderD*` :

```sh
sudo usermod -aG render "$USER"   # puis se reconnecter
```

**Rien de tout cela n'est obligatoire :** sans GPU utilisable, `rscap` bascule
automatiquement sur `libx264`.

Vérifiez ce qui est réellement utilisable :

```sh
rscap --probe
```

```
Matériel
  CPU              : Intel(R) Core(TM) Ultra 7 155H
  GPU              : Intel 0x7d55 (Intel) via /dev/dri/renderD128
  Session          : wayland

Encodeurs candidats pour h264 (ordre d'essai)
  h264_vaapi     matériel   présent dans ffmpeg
  h264_qsv       matériel   présent dans ffmpeg
  ...
Ouverture réelle à 1920x1080
  retenu : h264_vaapi (h264, matériel)
```

`--probe` **ouvre réellement** chaque encodeur : un nom listé par ffmpeg ne
prouve rien, seul l'ouverture le fait.

---

## Compilation

```sh
git clone <dépôt> && cd capturevideo
cargo build --release
```

Le binaire est dans `target/release/rscap`.

L'interface graphique est un **second binaire**, derrière un drapeau de
compilation :

```sh
cargo build --release --features gui
```

Elle produit `target/release/rscap-gui`. Sans `--features gui`, aucune liaison
GTK n'est téléchargée ni compilée : `rscap` reste utilisable sur une machine
sans bureau — un serveur, un conteneur d'intégration continue.

Installation facultative :

```sh
cargo install --path .
```

---

## Utilisation

```sh
# Enregistrement simple : écran principal + son système, 60 FPS, H.264/AAC
rscap -o ma-capture.mp4

# 30 FPS, qualité élevée, HEVC
rscap -o capture.mp4 --fps 30 --codec hevc --quality high

# Débit imposé, arrêt automatique après 60 secondes
rscap -o capture.mp4 --bitrate 35000000 --duration 60

# Sans son
rscap -o capture.mp4 --no-audio

# Encodeur imposé
rscap -o capture.mp4 --encoder h264_nvenc

# Repli logiciel forcé
rscap -o capture.mp4 --hardware off

# Fichier de configuration
rscap --config rscap.toml
```

`Ctrl+C` arrête proprement : les tampons sont vidés, l'encodage terminé, le
fichier finalisé et synchronisé sur le disque.

Au premier lancement, le portail de bureau demande quel écran partager. Le
choix est mémorisé (jeton de restauration) ; les lancements suivants ne posent
plus la question. Pour le réinitialiser :

```sh
rscap --forget-permission
```

### Pendant l'enregistrement

```
● Enregistrement  0:01:23   312.4 Mio
FPS       59.98 / 60   capture  60.01
Images   perdues 0   dupliquées 4   fusionnées 0
A/V      dérive +0.3 ms   audio 0 trous, 2 corrections
Files    vidéo 1/8   audio 2/64   paquets 3/256
Charge   CPU 84 %   GPU n/d   débit 34.8 Mb/s
Latence  encodage 4.3 ms (p99 5.5)   capture 1.1 ms
```

Redirigé vers un fichier, l'affichage passe automatiquement en lignes de
journal, sans séquences de contrôle.

### Toutes les options

```sh
rscap --help
```

| Option | Effet |
|---|---|
| `-o, --output <CHEMIN>` | fichier de sortie |
| `--fps <N>` | images par seconde (30 minimum) |
| `--codec <h264\|hevc\|av1>` | codec vidéo |
| `--quality <low\|medium\|high\|very-high>` | niveau de qualité |
| `--bitrate <BPS>` | débit imposé (0 = automatique) |
| `--rate-control <vbr\|cbr\|cq>` | stratégie de débit |
| `--hardware <auto\|force\|off>` | politique d'encodage matériel |
| `--encoder <NOM>` | encodeur imposé |
| `--container <mp4\|mkv>` | conteneur |
| `--pacing <cfr\|vfr>` | cadencement du flux de sortie |
| `--no-audio` | désactive la capture du son |
| `--audio-device <NOM>` | moniteur de sortie à capturer |
| `-d, --duration <SECONDES>` | arrêt automatique |
| `--benchmark` | banc d'essai au lieu d'un enregistrement |
| `--probe` | matériel et encodeurs disponibles |
| `--list-audio` | moniteurs de sortie disponibles |
| `--print-config` | configuration effective en TOML |
| `--synthetic` | générateur d'images au lieu de l'écran réel |
| `-v`, `-vv`, `-vvv` | verbosité des journaux |
| `--log-file <CHEMIN>` | journaux dans un fichier |

---

## Interface graphique

```sh
rscap-gui
```

La fenêtre pilote **la même bibliothèque** que la ligne de commande : même
configuration, même validation, même pipeline, mêmes compteurs. Un réglage qui
n'existe pas dans `rscap.toml` n'existe pas non plus dans la fenêtre.

| Page | Contenu |
|---|---|
| **Capture** | bouton d'enregistrement, durée, taille, et le tableau de bord complet — cadence, images perdues/dupliquées/fusionnées, dérive A/V, files, débits, latences, surcharges |
| **Vidéo** | FPS, codec, qualité, débit, contrôle de débit, cadencement, politique matérielle, encodeur imposé, nœud de rendu, images clés, images B, preset, anticipation, fils de conversion |
| **Audio** | activation, moniteur de sortie (liste peuplée au démarrage), fréquence, canaux, débit, fragment, correction de dérive, dérive maximale |
| **Sortie** | fichier (avec sélecteur), conteneur, MP4 fragmenté, arrêt automatique, tampon d'écriture |
| **Pipeline** | tailles des trois files, pool de tampons, sources synthétiques |
| **Matériel** | sondage des encodeurs et banc d'essai, rapport affiché tel quel |

Le menu charge et enregistre un `rscap.toml`, directement relisible par
`rscap --config`, et permet d'oublier l'autorisation d'écran du portail.

**Trois garanties structurelles :**

- *La fenêtre ne bloque jamais.* Portail, encodeurs, enregistrement,
  finalisation et banc d'essai vivent dans un thread séparé qui communique par
  messages. Le thread principal ne fait que dessiner.
- *Les statistiques ne coûtent rien au pipeline.* Elles voyagent par copie
  d'une photo de compteurs atomiques : aucun verrou du chemin chaud n'est pris
  pour afficher quoi que ce soit.
- *Fermer la fenêtre pendant un enregistrement ne le perd pas.* La fermeture
  est retenue, l'arrêt propre est demandé, et la fenêtre ne se ferme qu'une
  fois le fichier finalisé et synchronisé sur le disque.

Pendant un enregistrement, les pages de réglages sont grisées : la
configuration est celle qui a été confiée au pipeline, la modifier n'aurait
aucun effet et laisserait croire le contraire.

---

## Configuration

`rscap.toml` documente chaque option. Extrait :

```toml
[video]
fps = 60
codec = "h264"
quality = "high"
bitrate = 0            # 0 = calculé depuis quality + résolution
hardware = "auto"
pacing = "cfr"
lookahead_ms = 16

[audio]
enabled = true
sample_rate = 48000
channels = 2
bitrate = 192000
drift_correction = true

[output]
path = "recording.mp4"
format = "mp4"
writer_buffer_kb = 4096

[pipeline]
video_queue = 8
audio_queue = 64
packet_queue = 256
frame_pool = 12
```

Toutes les valeurs sont validées au démarrage, avant qu'aucune ressource ne
soit ouverte : une configuration incohérente est refusée immédiatement, pas
découverte au milieu d'un enregistrement.

---

## Banc d'essai

```sh
# Chaîne complète, écran réel, 20 secondes
rscap --benchmark --benchmark-duration 20 --fps 60

# Sans interaction : générateur d'images, son système réel
rscap --benchmark --synthetic --fps 60

# Entièrement déterministe (pour la CI)
rscap --benchmark --synthetic --synthetic-audio --fps 60
```

Le rapport donne FPS capture et encodage, latences moyennes et p99 pour chaque
étage, images perdues détaillées par cause, CPU, GPU, mémoire, débit disque,
dérive A/V — puis un **verdict** :

```
VERDICT
  ✓ 60 FPS tenus de bout en bout, sans perte.
```

ou, si la machine ne suit pas :

```
VERDICT
  ✗ 60 FPS NON tenus : 41.20 FPS réels, 312 images perdues.
  → L'encodage est le goulot d'étranglement (p99 = 22.4 ms pour un budget de 16.7 ms).
    L'encodeur est logiciel : vérifiez la disponibilité de VAAPI/NVENC/QSV.
```

Code de sortie **2** si la cible n'est pas tenue — utilisable dans un script.
La cible n'est déclarée tenue que si le FPS réel atteint 99 % de la cible
**et** qu'aucune image n'a été perdue.

---

## Tests

```sh
cargo test                      # 133 tests
cargo test --features gui       # 136 tests, interface comprise
cargo test --release            # plus rapide pour les tests de bout en bout
cargo clippy --all-targets
```

Couverture :

| Domaine | Vérifie |
|---|---|
| Cadencement | absorption du jitter, duplication, absence de dérive sur 216 000 slots |
| Horodatages | exactitude des PTS, monotonie, ancrage audio |
| Synchronisation | origine commune, dérive, correction, insertion de silence |
| Tampons | recyclage sans allocation, rejet en cas de saturation |
| Encodeurs | ordre de sélection, repli, dimensions impaires, duplication |
| Muxage | fichier finalisé, relisible, MP4 fragmenté, MKV |
| Configuration | valeurs limites, clés inconnues, cohérence |
| Arrêt | finalisation même sur arrêt immédiat |
| Interface | le thread de commande enregistre, s'arrête à la demande comme à l'échéance, et rapporte une configuration refusée sans rien ouvrir |

Les tests d'intégration (`tests/recording.rs`) enregistrent réellement puis
**relisent et décodent** le fichier produit : ordre des paquets, PTS/DTS,
grille CFR, alignement des deux pistes.

> La capture d'écran réelle passe par `xdg-desktop-portal`, qui exige un
> consentement interactif : elle ne peut pas être automatisée. Les tests
> utilisent donc un générateur d'images. Tout le reste de la chaîne —
> cadencement, files, encodage, muxage, écriture, finalisation — est exercé
> exactement comme en production.

---

## Organisation

```
src/
├── main.rs              ligne de commande, journaux, boucle d'affichage
├── lib.rs
├── config/              configuration TOML, valeurs par défaut, validation
├── error.rs             erreurs typées (disque plein, périphérique perdu, …)
├── timing/              horloge monotone, cadenceur CFR, horloge audio
├── capture/
│   ├── mod.rs           traits ScreenSource / AudioSource, files, pool
│   ├── frame.rs         VideoFrame, AudioChunk, formats de pixels
│   ├── synthetic.rs     générateurs pour les tests et le banc d'essai
│   ├── linux/           portail xdg, PipeWire, moniteur PulseAudio
│   └── windows/         non implémenté
├── encoder/
│   ├── hwdetect.rs      GPU, ordre des candidats
│   ├── video.rs         sélection, conversion, VAAPI/QSV/NVENC/AMF/x264
│   └── audio.rs         AAC, rééchantillonnage, asservissement d'horloge
├── muxer/
│   ├── mod.rs           conteneur, entrelacement, finalisation
│   └── writer.rs        écriture disque instrumentée
├── pipeline/
│   ├── mod.rs           threads, boucles d'encodage, arrêt propre
│   └── sync.rs          barrière de départ commune
├── performance/         compteurs atomiques, histogrammes, CPU/GPU/mémoire
├── bench/               banc d'essai et diagnostic
├── ui/                  bannière, tableau de bord, résumé (terminal)
├── gui/                 interface GTK 4, optionnelle
│   ├── mod.rs           fenêtre, pages, machine d'états
│   ├── controller.rs    thread de commande, messages, arrêt garanti
│   ├── settings.rs      un widget par champ de configuration
│   └── stats.rs         tableau de bord temps réel
└── bin/
    └── rscap-gui.rs     binaire de l'interface (--features gui)
```

---

## Dépendances et justification

| Crate | Pourquoi celle-ci |
|---|---|
| `ffmpeg-next` 9 | la seule liaison Rust suivant FFmpeg 9 (libavcodec 63). Donne accès aux encodeurs matériels *et* logiciels, au muxage et au rééchantillonnage par une seule API. `ffi` expose le `-sys` complet, indispensable pour `AVHWFramesContext` et `swr_set_compensation`, que l'API sûre n'enveloppe pas. |
| `pipewire` + `libspa` | seule voie de capture d'écran sous Wayland. Liaisons officielles du projet PipeWire. |
| `ashpd` | client `xdg-desktop-portal` typé, avec gestion du jeton de restauration. Écrire le D-Bus à la main serait long et fragile. |
| `libpulse-simple-binding` | API bloquante simple, exactement ce qu'il faut sur un thread de capture dédié. `cpal` n'expose pas les moniteurs de sortie sous Linux, donc ne permet pas de capturer le son *système*. |
| `crossbeam-channel` | files bornées avec `try_send` non bloquant et `recv_timeout` précis. Les canaux de la bibliothèque standard n'offrent pas de borne. |
| `crossbeam-queue` | `ArrayQueue` sans verrou pour le pool de tampons, utilisable depuis un callback temps réel. |
| `parking_lot` | verrou plus rapide et plus compact que celui de la bibliothèque standard, pour le seul verrou du chemin chaud. |
| `hdrhistogram` | percentiles exacts à coût constant. Un `Vec` de mesures allouerait dans le chemin chaud. |
| `thiserror` | erreurs typées avec `source`, sans code répétitif. |
| `tracing` + `tracing-appender` | journalisation structurée **non bloquante** : un journal ne doit jamais faire attendre un thread temps réel. |
| `tokio` | uniquement pour la négociation D-Bus du portail, qui est intrinsèquement asynchrone et se produit une seule fois. Le reste du programme est en threads dédiés. |
| `gtk4` + `libadwaita` | **optionnelles, feature `gui`.** GTK 4 est la seule boîte à outils qui rende correctement sous Wayland *et* X11 sans couche de compatibilité, et libadwaita fournit les pages de préférences, les points de rupture adaptatifs et les dialogues modernes — sinon réécrits à la main. `gtk::FileDialog` (4.10) et `adw::AlertDialog` (1.5) remplacent des API dépréciées qui se comportent mal sous Wayland. |
| `clap`, `serde`, `toml`, `libc`, `ctrlc` | usages standards. |

Crates volontairement **écartées** :

- **`cpal`** : ne donne pas accès aux moniteurs de sortie sous Linux, donc ne
  permet pas de capturer le son système.
- **`scrap`, `captrs`** : X11 seulement, inutilisables sous Wayland.
- **`gstreamer`** : apporterait son propre ordonnancement et son propre modèle
  de tampons, en concurrence avec celui décrit ici.

---

## Portage

L'architecture est indépendante de la plateforme : cadencement, files,
encodeurs, muxer et statistiques ne connaissent que les traits `ScreenSource`
et `AudioSource`. Ajouter un système revient à écrire un module de capture.

**Windows** (`src/capture/windows/`) :

| Étage | API |
|---|---|
| Capture | `Windows.Graphics.Capture`, repli `IDXGIOutputDuplication` |
| Surface | `ID3D11Texture2D` (B8G8R8A8) |
| Conversion | `ID3D11VideoProcessor` : BGRA → NV12 sur le GPU |
| Encodage | `h264_nvenc`, `h264_amf`, `h264_qsv`, repli `libx264` — déjà gérés |
| Audio | WASAPI en mode *loopback* sur le périphérique de rendu par défaut |
| Horloge | `QueryPerformanceCounter`, et `QPCTime` du `Direct3D11CaptureFrame` |

**macOS** : `ScreenCaptureKit` + `CoreAudio`, encodage `h264_videotoolbox`.

---

## Licence

MIT ou Apache-2.0, au choix.
