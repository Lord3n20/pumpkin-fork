# Stierzunge-Fork von Pumpkin

Fork von [Pumpkin](https://github.com/Pumpkin-MC/Pumpkin) für den Minecraft-Server auf der Home-Assistant-Box.
Branch `stierzunge` basiert auf dem Release-Tag `0.2.0+26.3-26.51` (Minecraft 26.3).

## Änderungen gegenüber Pumpkin

- **`/forceload` lädt Chunks wirklich.** In Pumpkin 0.2.0 werden mit `/forceload` markierte Chunks nur getickt,
  solange ein Spieler sie ohnehin geladen hält; ohne Spieler werden sie weder geladen noch erzeugt.
  Der Fork setzt dafür – wie Vanilla – ein Lade-Ticket. Damit funktioniert Vorgenerieren per Plugin.
- **Seed-Schutz** (`[world] seed_secret`, 0 = aus): Ein geheimer Wert wird in die Seeds für Strukturen,
  Deko/Erze, Höhlen und Slime-Chunks gemischt; Clients bekommen einen davon abgeleiteten Seed-Hash.
  Gelände und Biome bleiben wie bei Vanilla für den Seed, Tools wie SeedcrackerX finden den Seed
  aber nicht mehr über Strukturen oder Erze. Nur für neue Welten setzen und danach nie ändern.
- **Leichtes Anticheat** (`[anticheat]`): „zu schnell bewegt“ (Zurücksetzen bei mehr als 10 Blöcken pro
  Bewegungspaket, mit Elytra 30) und Flug-Kick nach ~4 s Schweben ohne Block in der Nähe.
  Creative, Spectator und Spieler mit Flugerlaubnis sind ausgenommen.
- **Anti-Xray** (`[anti_xray]`): wie Paper engine-mode 1 – nicht freiliegende Erze gehen als Stein,
  Deepslate bzw. Netherrack an den Client; bei Blockänderungen werden versteckte Blöcke im Umkreis
  von 2 Blöcken aufgedeckt. Erze direkt an Chunk-Grenzen bleiben versteckt, bis in der Nähe etwas
  abgebaut wird.
- **`enable_status`** (Grundeinstellung, Standard `true`): `false` beantwortet keine Serverlisten-Pings,
  wie Vanilla `enable-status=false` – unsichtbar für Scanner, Beitreten geht weiter.
- **Uhrzeit im Datei-Log** (`logs/latest.log`, Ortszeit), vorher stand dort nur `[INFO] …`.
- Upstream-Workflows entfernt, dafür `.github/workflows/stierzunge.yml`.

## Plugins

`stierzunge/plugins.sh` baut die WASM-Plugins gegen die Plugin-API aus diesem Baum (gleiche API wie der Server):

| Plugin | Quelle | Zweck |
|---|---|---|
| `pumpkin_voice` | [hmdnnrmn/PumpkinVoice](https://github.com/hmdnnrmn/PumpkinVoice) | Simple Voice Chat (UDP 24454) |
| `pumpkin_pregen` | [Thijs226/pumpkin-world-pregen](https://github.com/Thijs226/pumpkin-world-pregen) | Vorgenerieren (`/pregen start <radius> [x z [dimension]]`) |

Eigene Änderungen an Plugins liegen als `stierzunge/<plugin>-*.patch` daneben und werden beim Bauen angewendet:
`pregen-dimension.patch` ergänzt das Argument `overworld`/`the_nether`/`the_end` (Konsole und RCON haben keine
Welt, und `execute in` erreicht das Plugin nicht) und sucht die Welt im Job über die Dimension statt über den
Namen – Pumpkin benennt alle drei Dimensionen nach dem Weltordner.

## Release bauen

Tag setzen und pushen, z. B. `git tag stierzunge-0.2.0-2 && git push origin stierzunge-0.2.0-2`.
Der Workflow hängt `pumpkin-X64-Linux-musl`, die `.wasm`-Plugins und `checksums.sha256` an das Release.

Lokal: `RUST_MIN_STACK=268435456 cargo build --release --bin pumpkin` und `stierzunge/plugins.sh`.

## Auf neue Pumpkin-Version heben

```sh
git fetch upstream --tags
git rebase --onto <neuer-tag> 0.2.0+26.3-26.51 stierzunge
```

Danach die Plugin-Commits in `plugins.sh` prüfen (die Plugins müssen zur neuen API passen).
