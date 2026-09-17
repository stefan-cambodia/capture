# Architecture du pipeline

Ce document répond, dans l'ordre, aux questions qui déterminent si un
enregistreur d'écran tient réellement sa cadence :

1. [Les goulots d'étranglement](#1-où-sont-les-goulots-détranglement)
2. [Comment les images vont de l'écran à l'encodeur](#2-de-lécran-à-lencodeur)
3. [Comment le son système est capturé](#3-capture-du-son-système)
4. [Comment les horodatages sont produits](#4-production-des-horodatages)
5. [Comment audio et vidéo restent synchronisés](#5-synchronisation-audiovidéo)
6. [Comment les tampons empêchent les saccades](#6-tampons-et-saccades)
7. [Comment les images perdues sont détectées](#7-détection-des-pertes)
8. [Comment l'encodeur matériel est choisi](#8-choix-de-lencodeur)
9. [Ce qui se passe quand 60 FPS n'est pas tenable](#9-quand-60-fps-nest-pas-tenable)
10. [Les choix de performance](#10-choix-de-performance)

---

## Vue d'ensemble

```
 ┌──────────────────┐   file bornée    ┌──────────────────┐
 │ CAPTURE VIDÉO    │  ┌────────────┐  │ ENCODAGE VIDÉO   │
 │ portail + PipeWire├─►│ 8 images   ├─►│ pacer + NV12 +   ├──┐
 │ thread dédié     │  │ (pool)     │  │ VAAPI/x264       │  │
 └──────────────────┘  └────────────┘  └──────────────────┘  │
                                                             │  ┌──────────┐
                                                  file bornée├─►│  MUXER   │
                                                  256 paquets│  │ thread   ├─► disque
 ┌──────────────────┐   file bornée    ┌──────────────────┐  │  │ dédié    │   (tampon 4 Mio)
 │ CAPTURE AUDIO    │  ┌────────────┐  │ ENCODAGE AUDIO   │  │  └──────────┘
 │ moniteur Pulse   ├─►│ 64 blocs   ├─►│ resample + AAC + ├──┘
 │ thread dédié     │  │ de 10 ms   │  │ asservissement   │
 └──────────────────┘  └────────────┘  └──────────────────┘
```

Cinq threads, quatre files bornées, **aucune boucle partagée**. Les règles de
circulation :

| Étage | Peut-il bloquer ? | Que fait-il en cas de saturation ? |
|---|---|---|
| Capture vidéo | **jamais** | jette l'image la plus récente, incrémente `capture_overrun` |
| Capture audio | **jamais** | jette le bloc, incrémente `audio_overrun` |
| Encodage | oui, sur le muxer | c'est le point de contre-pression voulu |
| Muxer | oui, sur le disque | incrémente `disk_write_lag` au-delà d'une image |

La conséquence importante : **la contre-pression ne remonte jamais jusqu'à la
capture**. Un disque lent fait grossir la file de paquets, puis la file
d'images, puis des images sont jetées et comptées — mais le thread de capture
n'attend jamais, donc le compositeur n'est jamais ralenti et l'écran de
l'utilisateur ne saccade pas.

---

## 1. Où sont les goulots d'étranglement

Sur une capture 2560×1440 à 60 FPS, chaque image brute pèse 14,7 Mio et le
budget par image est de **16,67 ms**. Les postes de dépense :

| Poste | Coût mesuré (Intel Core Ultra 7 155H) | Remarque |
|---|---|---|
| Copie du tampon PipeWire | ~1,5 ms | 14,7 Mio de `memcpy` |
| Conversion BGRx → NV12 | ~2 ms | libswscale, multithread |
| Téléversement vers le GPU | ~1 ms | mémoire partagée sur iGPU |
| Encodage H.264 VAAPI | **4,3 ms** (p99 5,5) | poste dominant |
| Muxage + écriture | 0,013 ms | négligeable |

Et les pièges qui ne se voient pas dans un profil :

- **L'allocation.** 14,7 Mio par image à 60 FPS, c'est 884 Mio/s d'allocations.
  D'où le pool de tampons préalloués (`BufferPool`), qui ramène le coût à zéro
  en régime établi.
- **Le cadencement.** Un `sleep(16 ms)` accumule le retard de chaque réveil :
  quelques centaines de microsecondes par image, soit **plusieurs secondes de
  dérive par heure**. D'où les échéances absolues (§4).
- **L'horloge audio.** Une carte son dérive de 10 à 100 ppm. À 50 ppm, c'est
  180 ms de décalage par heure (§5).
- **Le couplage.** Si l'encodage bloque la capture, une seule image lente fait
  perdre l'image suivante, qui fait perdre la suivante : l'effondrement est
  immédiat. D'où le découplage complet par files bornées.

---

## 2. De l'écran à l'encodeur

### Ce que le système permet réellement

Sous Wayland, **aucune application ne peut lire l'écran directement** : c'est
le compositeur qui décide, après consentement explicite. Le chemin obligatoire
est `xdg-desktop-portal` → flux PipeWire. Sous X11, le portail délègue à la
même infrastructure PipeWire, si bien qu'une implémentation X11/XShm séparée
n'apporterait rien — et ne fonctionnerait pas du tout sous Wayland, y compris
via XWayland, qui ne voit que les fenêtres X11.

### Le trajet d'une image

```
compositeur
   │  compose l'image, y attache un spa_meta_header (PTS CLOCK_MONOTONIC)
   ▼
mémoire partagée PipeWire (MemFd, projetée par MAP_BUFFERS)
   │
   ▼  callback `process`, sur le thread temps réel de PipeWire
copie unique vers un tampon du pool          ← seule copie du chemin
   │
   ▼  try_send, jamais bloquant
file bornée de 8 images
   │
   ▼  thread d'encodage
AVFrame « vue » : data[0] pointe sur le tampon, aucun octet copié
   │
   ▼  sws_scale_frame, multithread
NV12
   │
   ▼  av_hwframe_transfer_data (VAAPI/QSV) ou directement (NVENC/AMF/x264)
surface GPU
   │
   ▼  avcodec_send_frame / avcodec_receive_packet
paquet encodé
```

Le callback `process` ne contient **qu'un `memcpy` et un envoi non bloquant** :
aucune allocation, aucun verrou, aucune E/S. C'est ce qui le rend sûr sur un
thread temps réel.

### Le chemin zéro-copie DMA-BUF : pourquoi il n'est pas activé

L'idéal serait `compositeur → DMA-BUF → surface VAAPI → encodeur`, sans jamais
passer par la mémoire centrale. Ce chemin **n'est pas implémenté ici**, pour
des raisons qu'il vaut mieux énoncer que masquer :

1. **La copie GPU → CPU a déjà eu lieu** quand nous voyons l'image : c'est le
   compositeur qui l'a faite, avant de nous livrer un tampon en mémoire
   partagée. Convertir en NV12 sur le GPU imposerait de *remonter* le BGRx vers
   le GPU, donc deux transferts au lieu d'un.
2. Négocier un DMA-BUF impose d'énumérer les modificateurs DRM supportés
   (dance EGL/GBM), puis de traiter l'image **avant** de rendre le tampon à
   PipeWire — sinon le compositeur le réutilise et l'écrase. Cela suppose un
   blit GPU→GPU vers un pool propre, donc du travail dans le thread temps réel.
3. Ce chemin ne peut pas être validé automatiquement : le portail exige un
   consentement interactif. Livrer du code `unsafe` de mappage de surfaces qui
   n'a jamais été exécuté serait un mauvais service.

Les points d'ancrage existent : `VideoFrame` est déjà un type opaque pour le
reste du pipeline, et `Acceleration::HardwareFrames` sait déjà construire un
`AVHWFramesContext`. L'ajout se ferait dans `capture/linux/pw_screen.rs`
(négociation `SPA_FORMAT_VIDEO_modifier`) et `encoder/video.rs`
(`av_hwframe_map` depuis `AV_PIX_FMT_DRM_PRIME`).

**Ce que coûte ce choix, mesuré :** 0,84 cœur sur 22 en 1440p60, soit 3,8 % de
la machine. Le gain théorique d'un chemin zéro-copie serait de l'ordre de
0,3 cœur.

---

## 3. Capture du son système

On capture le **moniteur de la sortie** (`@DEFAULT_MONITOR@`), c'est-à-dire ce
qui sort des haut-parleurs — pas le microphone. C'est l'équivalent Linux du
mode *loopback* de WASAPI.

Le passage par l'interface PulseAudio (servie par `pipewire-pulse` sur une
machine moderne) apporte trois choses gratuitement :

- la notion de moniteur de sortie, standard et stable ;
- le suivi automatique du changement de périphérique par défaut ;
- la compatibilité avec les systèmes encore sous PulseAudio pur.

`pa_simple_read` est bloquant et rend un fragment complet de 10 ms. Le thread
audio ne fait que : lire, horodater, convertir en `f32`, envoyer. Aucun
`sleep`, aucun cadencement artificiel — c'est le périphérique qui donne le
rythme.

Si `@DEFAULT_MONITOR@` n'est pas résolu, on interroge le serveur par
introspection et on essaie le premier moniteur trouvé. `rscap --list-audio`
expose la même liste.

---

## 4. Production des horodatages

### Une seule horloge

Tout le pipeline lit `CLOCK_MONOTONIC` en nanosecondes. Elle ne recule jamais,
n'est pas affectée par les changements d'heure système, et c'est **aussi la
base des horodatages que PipeWire place dans `spa_meta_header.pts`**. On peut
donc comparer directement un horodatage du compositeur et une lecture locale,
sans conversion ni estimation.

### Vidéo

L'horodatage d'une image est, par ordre de préférence :

1. `spa_meta_header.pts` — l'instant où le compositeur a *composé* l'image.
   C'est la bonne valeur : elle exclut le trajet inter-processus.
2. à défaut, l'instant de réception, ce que l'on signale une fois dans les
   journaux.

Ce timestamp n'est pas écrit tel quel dans le fichier : il sert à placer
l'image sur la **grille CFR** (§ ci-dessous).

### Audio

`pa_simple_read` rend un fragment ; on interroge alors la latence du flux. Le
*dernier* échantillon du fragment a été capturé il y a `latence`
nanosecondes :

```
pts(premier échantillon) = maintenant − latence − durée_du_fragment
```

Ce timestamp sert d'**ancrage** et de mesure de dérive, pas de PTS : les PTS
écrits dans le fichier sont dérivés du compteur d'échantillons, qui ne comporte
aucun jitter d'ordonnancement.

### Le cadenceur CFR : des échéances, pas des `sleep`

```rust
// Ce qu'on ne fait PAS : l'erreur de chaque réveil s'accumule.
loop { capture(); thread::sleep(Duration::from_millis(16)); }

// Ce qu'on fait : l'échéance du slot k est recalculée depuis l'origine.
deadline(k) = origine + k × période + lookahead
```

Le slot `k` porte le PTS `k` dans une base de temps `1/fps`. Le PTS est donc
**exact par construction** : aucun arrondi, aucune accumulation d'erreur. Une
itération en retard ne décale pas les suivantes. Le test
`deadlines_do_not_drift_over_an_hour` vérifie cette propriété sur 216 000
slots.

Le `lookahead` (16 ms par défaut) est la marge laissée à une image pour
arriver. Il doit dépasser la latence de livraison de la source ; sinon des
images fraîches arrivent après l'échéance de leur slot et se retrouvent
recalées d'un cran. Le benchmark affiche cette latence en p99.

### Traitement des cas limites

| Situation | Traitement |
|---|---|
| Image en avance de moins d'une demi-période | placée dans son slot (arrondi au plus proche) |
| Deux images dans le même slot | la plus récente écrase l'autre, `frames_coalesced` |
| Image après l'échéance de son slot | **recalée sur le slot suivant**, `frames_late` — jamais jetée |
| Aucune image pour un slot | répétition de la précédente, `frames_duplicated` |
| Retard supérieur à 1 s | slots sautés, `slots_skipped` + `encoder_overload` |

---

## 5. Synchronisation audio/vidéo

C'est l'exigence la plus délicate du projet. Trois mécanismes distincts s'y
emploient.

### 5.1 Une origine commune

La capture vidéo et la capture audio ne démarrent jamais au même instant : le
portail négocie pendant que le périphérique audio s'amorce, et l'écart atteint
couramment 100 à 300 ms. Poser naïvement le PTS 0 de chaque flux sur sa propre
première donnée créerait un décalage **permanent** de cet écart.

Chaque flux annonce donc l'horodatage de sa première donnée à une barrière
(`StartSync`). Le dernier arrivé calcule :

```
t0 = max(première donnée vidéo, première donnée audio)
```

C'est l'instant à partir duquel *tous* les flux ont des données. La vidéo jette
ce qui précède (au plus une image), l'audio rogne ses premiers échantillons.
Les deux pistes commencent donc exactement au même instant, sans silence
artificiel ni image figée au début. Un flux qui échoue se retire de la barrière
pour ne pas bloquer les autres, et une expiration résout quand même plutôt que
d'empêcher l'enregistrement de démarrer.

### 5.2 Des PTS exacts, pas estimés

Ce que l'on **ne fait pas** :

```
pts_audio = numéro_de_paquet × durée_du_paquet     ← faux : suppose 48000 Hz exacts
pts_vidéo = numéro_d_image / fps                   ← faux : suppose une capture parfaite
```

Ce que l'on fait :

| Flux | Base de temps | PTS | Pourquoi |
|---|---|---|---|
| Vidéo CFR | `1/fps` | index du slot | exact par construction, sans arrondi |
| Vidéo VFR | `1/1 000 000` | horodatage réel en µs | fidèle à la capture |
| Audio | `1/rate` | index d'échantillon | continu, sans jitter d'ordonnancement |

Le muxer reçoit des paquets déjà convertis dans la base de temps du flux, et
`av_interleaved_write_frame` les écrit triés par DTS. Le conteneur porte donc
des `pts`, `dts`, `time_base` et `duration` corrects — ce que vérifie le test
`packet_timestamps_are_ordered_and_monotonic`.

> **Note sur l'AAC.** L'encodeur AAC introduit un délai de codage de 1024
> échantillons et le signale par un PTS négatif sur ses premiers paquets. Le
> MP4 compense par une *liste d'édition*, si bien qu'un lecteur démarre bien à
> zéro. C'est correct et attendu ; le test
> `both_streams_start_at_zero_for_a_player` le vérifie.

### 5.3 L'asservissement de l'horloge audio

Le point le plus important pour une session longue. L'horloge d'une carte son
n'est pas exactement à 48 000 Hz : elle dérive de 10 à 100 ppm par rapport à
l'horloge système. À 50 ppm, c'est **180 ms par heure** — largement audible.

Le PTS reste dérivé du compteur d'échantillons, mais sa **cadence** est
corrigée. À chaque bloc, on compare :

```
attendu  = (horodatage_du_bloc − t0) × rate / 1e9    ← selon l'horloge système
réel     = échantillons déjà remis à l'encodeur      ← selon l'horloge audio
dérive   = attendu − réel
```

Et on agit selon l'ampleur :

| Dérive | Action | Compteur |
|---|---|---|
| < 1 ms | rien (zone morte) | — |
| < 200 ms | `swr_set_compensation` : un huitième de l'écart, plafonné à 1 % de la cadence, étalé sur une seconde | `audio_compensations` |
| > 200 ms, audio en retard | insertion de silence | `audio_gaps_filled` |
| > 200 ms, audio en avance | échantillons écartés | `audio_dropped` |

La correction douce étire ou comprime le signal de façon inaudible (variation
de hauteur de l'ordre du millième de demi-ton). L'horloge audio est ainsi
asservie à l'horloge système : **la dérive ne peut pas s'accumuler**, quelle
que soit la durée de la session.

### 5.4 Mesurer la dérive sans se tromper de grandeur

Comparer directement les positions média des deux flux donnerait la différence
de **latence** entre les deux branches — plusieurs dizaines de millisecondes,
constantes, et sans aucun effet sur la synchronisation du fichier produit.

Ce que l'on veut mesurer, c'est ce qui *s'accumule*. On calcule donc pour
chaque flux son « avance » (position média moins temps réellement écoulé),
puis on retranche l'écart observé après une seconde de chauffe. La dérive part
ainsi de zéro et ne bouge que si une horloge s'éloigne réellement de l'autre.

Mesure sur 20 s, 1440p60, audio réel : **−0,24 ms**. Le fichier produit contient
1201 images à exactement 60/1 FPS, pistes vidéo 20,0167 s et audio 20,0096 s.

---

## 6. Tampons et saccades

| File | Capacité | Durée équivalente | Rôle |
|---|---|---|---|
| Images vidéo | 8 | 133 ms à 60 FPS | absorber une rafale d'encodage |
| Blocs audio | 64 | 640 ms | absorber un réveil tardif du thread audio |
| Paquets | 256 | variable | absorber une latence disque |
| Pool de tampons | 12 | — | supprimer les allocations |

Le dimensionnement suit un principe simple : **assez grand pour absorber une
variation de charge, assez petit pour que la latence reste bornée**. Une file
de 8 images signifie qu'au pire l'encodeur a 133 ms de retard ; au-delà, des
images sont jetées et comptées plutôt que d'accumuler un retard sans fin.

Le pool est le point qui évite les saccades les moins visibles : sans lui,
chaque image déclencherait une allocation de 14,7 Mio, et l'allocateur finirait
par déclencher un `mmap` au mauvais moment, en plein budget d'image.

---

## 7. Détection des pertes

Toutes les conditions anormales ont leur compteur, exposé en temps réel et dans
le résumé final. Rien n'est masqué.

| Compteur | Sens |
|---|---|
| `frames_dropped_queue` | file de capture pleine : l'image est perdue |
| `slots_skipped` | encodeur saturé : des slots CFR ont été abandonnés |
| `frames_late` | image arrivée après son échéance, **recalée** sur le slot suivant |
| `frames_coalesced` | deux captures pour un même slot, la plus récente gagne |
| `frames_duplicated` | slot comblé par répétition (écran fixe) |
| `capture_overrun` | occurrences de saturation de la file vidéo |
| `encoder_overload` | occurrences de saturation de l'encodeur |
| `audio_overrun` | saturation de la file audio |
| `disk_write_lag` | écriture disque plus longue qu'une image |

`frames_lost()` ne compte que ce qui manque réellement dans le fichier :
`frames_dropped_queue + slots_skipped`. Les duplications et les images recalées
n'en font pas partie — elles sont bien présentes dans le fichier.

**Le programme ne prétend jamais avoir tenu une cadence qu'il n'a pas tenue.**
Le résumé final compare le FPS réel au FPS cible et énonce le verdict, avec la
cause probable quand la cible n'est pas atteinte.

---

## 8. Choix de l'encodeur

### La règle

**On ne fait pas confiance à la présence d'un nom d'encodeur dans ffmpeg.** Sur
cette machine, `ffmpeg -encoders` liste `h264_nvenc` alors qu'il n'y a aucune
carte NVIDIA. Le seul test valable est d'**ouvrir l'encodeur avec les
paramètres réels** : résolution, débit, format de pixel. C'est ce que fait
`VideoEncoder::open`, en parcourant une liste ordonnée de candidats.

La détection matérielle sert donc à **ordonner** les candidats, pas à les
filtrer.

### L'ordre

Le fabricant du GPU est lu dans `/sys/class/drm/card*/device/vendor`, et le
nœud de rendu associé est résolu vers `/dev/dri/renderD*`.

| Fabricant | Ordre matériel |
|---|---|
| Intel | `vaapi`, puis `qsv` |
| AMD | `vaapi`, puis `amf` |
| NVIDIA | `nvenc`, puis `vaapi` |
| inconnu | `vaapi`, `qsv`, `nvenc`, `amf` |

VAAPI passe avant QSV sur Intel : le chemin QSV ajoute une couche (libvpl) pour
un gain nul sur une capture d'écran.

La chaîne complète de repli, telle que demandée au projet :

```
codec demandé, matériel
  → H.264 matériel
  → HEVC matériel
  → AV1 matériel
  → codec demandé, logiciel
  → H.264 logiciel
```

`hardware = "force"` coupe la liste avant les candidats logiciels ;
`hardware = "off"` ne garde que ceux-ci ; `encoder = "..."` court-circuite tout.

### Comment les images sont fournies

| Type | Encodeurs | Alimentation |
|---|---|---|
| `HardwareFrames` | VAAPI, QSV | `AVHWFramesContext` + `av_hwframe_transfer_data` |
| `HardwareDirect` | NVENC, AMF | images NV12 en mémoire centrale, le pilote téléverse |
| `Software` | x264, x265, SVT-AV1 | NV12, repli YUV420P si l'encodeur le refuse |

Le format logiciel est choisi par essai réel : NV12 d'abord, YUV420P si
l'ouverture échoue. Là encore, l'ouverture est la seule preuve.

---

## 9. Quand 60 FPS n'est pas tenable

C'est le cas le plus important à bien traiter, parce que c'est là qu'un
enregistreur médiocre se met à mentir.

**Ce qu'on ne fait pas :** continuer à accumuler du retard, ou dupliquer des
images pour remplir le fichier en annonçant 60 FPS.

**Ce qu'on fait :** au-delà d'une seconde de retard, le cadenceur **saute** les
slots en retard et reprend au slot courant :

```rust
if behind > fps {                 // plus d'une seconde de retard
    self.next_slot = current;     // on se recale sur le temps réel
    return Emit::Skip { from, to };
}
```

Les PTS restent exacts (ce sont des index de slot), donc **l'audio reste
synchrone** : la vidéo montre une saccade visible, à l'endroit exact où la
machine a décroché. C'est le comportement correct : une saccade honnête vaut
mieux qu'une désynchronisation progressive.

L'événement est compté (`slots_skipped`, `encoder_overload`), journalisé, et
affiché en direct. Le résumé final est explicite :

```
  ⚠ La cadence de 60 FPS n'a PAS été tenue (41.20 FPS réels).
    Cause probable : l'encodeur ne suit pas. Essayez un encodeur matériel,
    une qualité inférieure, ou 30 FPS.
```

Et `--benchmark` rend un verdict binaire, avec un code de sortie 2 utilisable
dans un script : la cible n'est déclarée tenue que si le FPS réel atteint 99 %
de la cible **et** qu'aucune image n'a été perdue.

---

## 10. Choix de performance

### Threads dédiés plutôt que `async`

`tokio` n'est utilisé que pour la négociation D-Bus du portail, qui est
intrinsèquement asynchrone et se produit une seule fois. Tout le reste tourne
sur des threads dédiés :

- un ordonnanceur asynchrone introduit une latence de réveil variable, exactement
  ce qu'un cadencement à 16,67 ms ne tolère pas ;
- les callbacks temps réel de PipeWire ne sont pas des tâches `async` ;
- `crossbeam::channel::recv_timeout` réveille à quelques dizaines de
  microsecondes près, ce qui suffit largement.

### Compteurs atomiques, un seul verrou

Les chemins chauds n'utilisent que des entiers atomiques en ordonnancement
`Relaxed` (~1 ns, aucune barrière). Le seul verrou du chemin chaud est celui
des histogrammes de latence (`parking_lot::Mutex`, ~20 ns non contendu), et il
n'est jamais tenu pendant une opération bloquante.

### Journalisation non bloquante

`tracing-appender` écrit sur un thread séparé. Un journal ne doit jamais faire
attendre un thread temps réel, même si la sortie est un terminal lent ou un
tube saturé. Aucun chemin chaud ne journalise par image : les avertissements
sont émis une seule fois (`warned_missing_header`) ou sur événement rare.

### Écriture disque

Tampon de 4 Mio via une interface d'E/S personnalisée, plutôt que les 32 Kio
par défaut de ffmpeg. Le gain en appels système est secondaire ; ce qui compte
vraiment :

- **la cause exacte d'un échec** : ffmpeg rend `AVERROR(EIO)` là où le noyau
  disait `ENOSPC`. Sur un enregistrement long, « disque plein » est le
  diagnostic le plus utile qui soit, et il doit arriver tel quel ;
- **la mesure** : débit réel, durée de chaque écriture, détection d'un disque
  qui décroche.

`fsync` est appelé après `write_trailer` : tant qu'il n'a pas rendu la main,
les données peuvent n'exister que dans le cache du noyau.

### Duplication gratuite

Devant un écran fixe, le cadenceur répète l'image précédente. Cette répétition
ne coûte **aucune conversion** : l'image de travail NV12 est simplement
resoumise avec un nouveau PTS. C'est ce qui permet de tenir 60 FPS sur un écran
immobile pour presque rien.

### Séquence d'arrêt

```
1. drapeau d'arrêt      → les captures sortent de leur boucle
2. jointure des captures (délai maximal de 3 s — un périphérique audio bloqué
   dans un appel système ne doit pas empêcher la finalisation)
3. les encodeurs vident leurs files, puis `send_eof` et récupèrent les derniers
   paquets
4. fermeture de la file de paquets
5. `write_trailer` : table des échantillons (`moov`)
6. destruction du contexte → vidange du tampon AVIO
7. `fsync`
8. le chemin du fichier est rendu
```

Le fichier est finalisé **même si un encodeur a échoué** : un arrêt, y compris
provoqué par une erreur, ne doit jamais laisser un fichier corrompu. Le test
`a_recording_stopped_immediately_still_produces_a_valid_file` couvre le cas le
plus défavorable.
